use tauri::Manager;

pub mod app_exit;
mod exit_requests;
pub mod app_update;
pub mod shutdown_wait;
mod update_commands;
mod update_journal;
pub mod atomic_write;
pub mod bounded_log;
pub mod autostart;
pub mod autostart_commands;
pub mod connection_settings;
pub mod policy_settings;
pub mod gateway_activity;
pub mod gateway_commands;
pub mod gateway_connection;
pub mod gateway_safe_shutdown;
pub mod gateway_control;
pub mod gateway_diagnostics;
pub mod gateway_diagnostics_reader;
pub mod gateway_env;
pub mod gateway_process;
mod gateway_proxy;
pub mod gateway_status;
pub mod single_instance;
pub mod timeutil;
pub mod tray;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let startup = single_instance::StartupGuard::acquire(&context.config().identifier)
        .expect("无法取得单实例启动锁");
    let activation = std::sync::Arc::new(single_instance::ActivationRequests::default());
    let callback_activation = activation.clone();
    let app = tauri::Builder::default()
        // 必须最先注册：第二个进程在插件初始化阶段转发参数后立即退出，
        // 不会创建第二个面板/托盘，也不会执行更新、自启与网关初始化。
        .plugin(tauri_plugin_single_instance::init(move |app, _argv, _cwd| {
            if callback_activation.request() {
                tray::show_main_window(app);
            }
        }))
        // 只由 Rust 安全更新服务调用；不给前端开放可绕过排空的 updater 安装权限。
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(gateway_commands::GatewayState::default())
        .manage(autostart_commands::AutostartUiState::default())
        .manage(gateway_activity::ActivityState::default())
        .manage(app_exit::AppExitState::default())
        .setup(|app| {
            // T6：应用启动即幂等编排开机自启（建立/改写自启项 + 清理确认属于本网关的旧快捷方式）
            autostart_commands::sync_on_launch(app.handle());
            // 更新恢复必须先于默认启动，停止态及未知安装结果不能被默认自启覆盖。
            update_commands::initialize(app.handle()).map_err(std::io::Error::other)?;
            // T5：系统托盘常驻（左键切换面板 / 右键菜单）。失败不阻塞壳（面板仍可用），
            // 仅记录到 stderr（release 无控制台时不可见，托盘缺失属可见问题，不影响网关安全）
            if let Err(e) = tray::build_tray(app.handle()) {
                eprintln!("托盘初始化失败（面板不受影响）：{e}");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    // 保留窗口与受管句柄，先让用户选择取消、隐藏到托盘或安全退出。
                    api.prevent_close();
                    app_exit::request_window_close(window.app_handle());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            update_commands::get_update_state,
            update_commands::check_for_update,
            update_commands::confirm_update,
            update_commands::get_shutdown_progress,
            update_commands::cancel_shutdown,
            gateway_commands::get_connection_settings,
            gateway_commands::save_connection_settings,
            gateway_commands::get_policy_settings,
            gateway_commands::save_policy_settings,
            gateway_commands::start_gateway,
            gateway_commands::stop_gateway,
            gateway_status::get_status,
            gateway_commands::restart_gateway,
            gateway_commands::open_logs_dir,
            autostart_commands::get_autostart,
            autostart_commands::set_autostart_enabled,
            app_exit::get_close_request,
            app_exit::respond_to_close,
            app_exit::quit_app,
            // T5：诊断读取（只读消费者）——取最近活动事件 + 分页查询（级别筛选 + 搜索）
            gateway_diagnostics_reader::get_recent_activity,
            gateway_diagnostics_reader::query_activity_events,
            gateway_activity::watch_activity_events,
            gateway_activity::stop_activity_watch
        ])
        .build(context)
        .expect("error while building tauri application");
    // build 已创建插件通知窗口；不要持锁进入事件循环，否则其他进程无法通知本实例。
    drop(startup);
    app.run(move |app, event| {
        // 主窗口与应用 setup 在 Ready 期间完成，比插件初始化晚。
        if matches!(event, tauri::RunEvent::Ready) && activation.mark_ready() {
            tray::show_main_window(app);
        }
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if !update_commands::exit_authorized(app) {
                api.prevent_exit();
                app_exit::request_exit_from_event(app, None);
            }
        }
        // Exit 不再二次扫描端口/强杀：安全停机已在授权退出前完成，不能误停新外部实例。
    });
}
