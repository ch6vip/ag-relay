//! 网关进程生命周期管理（Windows 专用，本项目自用平台）。
//!
//! 启动链（配方见共享笔记 tauri-2-recipes.md §4，含 2026-10-11 实测更正）：
//!   spawn 自带 node.exe relay-supervisor.mjs（cwd=网关资源目录，CREATE_NO_WINDOW）。
//!   ⚠ 绝不用 CREATE_NEW_PROCESS_GROUP——它会隐式禁用 Ctrl+C，毁掉优雅关停。
//!   → 子进程挂进 KILL_ON_JOB_CLOSE 的 Job Object（壳崩溃则网关整树必死）。
//! 停止链（受管）：AttachConsole(supervisor_pid) → 忽略自身 CTRL_C → 广播 CTRL_C_EVENT
//!   → 等待优雅退出（总预算 ≤10s，F4）→ 超时 TerminateJobObject 强杀整树。全程幂等。
//! 停止链（外部，F1「关了就彻底关了」）：本壳没有受管网关但 8787 有网关在跑时，
//!   从 status 的网关 pid 经 Toolhelp 快照找父进程并核实命令行含 relay-supervisor.mjs
//!   （宁可不杀不可误杀），对 supervisor 广播 CTRL_C → 等 ≤10s → 超时 taskkill /T /F 树杀
//!   （外部网关不归本壳 Job 管）。
//! ⚠ 实测更正（配方笔记未覆盖）：AttachConsole 必须在 FreeConsole 之后的数百毫秒内
//!   重试才能成功（free 后立即 attach 稳定报 err=6 INVALID_HANDLE），故此处带重试。

use std::fmt;
use std::net::TcpListener;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_C_EVENT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

use std::os::windows::io::AsRawHandle;

use crate::connection_settings::ConnectionSettings;
use crate::gateway_env::{gateway_env_pairs_with_policy, gateway_env_pairs_with_settings};
use crate::policy_settings::PolicySettings;

/// 无窗口创建标志（MSDN CREATE_NO_WINDOW）。
/// 实测：子进程拥有一个隐藏控制台（可 AttachConsole），CTRL_C 广播可达。
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 网关监听端口（与 gateway_env 表中 PROXY_PORT=8787 一致，测试断言两者同步）。
pub const GATEWAY_PORT: u16 = 8787;
pub const GATEWAY_HOST: &str = "127.0.0.1";

/// 优雅关停默认预算（工单：最多等 10 秒后强杀）。
/// F4 语义：这是**总挂钟预算**——BOOT_GRACE 启动缓冲计入其中（剩余 = 预算 - 已耗），
/// 最坏总耗时 ≤ 预算 + 进程收尾的毫秒级开销，不会出现「宽限 1.5s + 优雅 10s」的叠加。
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// 广播 CTRL_C 前的启动缓冲。node 需要数百毫秒引导后才注册 SIGINT 处理器，
/// 过早广播会命中引导期的默认处置：进程以 STATUS_CONTROL_C_EXIT（0xC000013A）裸死，
/// 完全不优雅（高负载下 node 引导变慢，T5 基线实测桩测试间歇复现）。
/// 缓冲期间子进程若自行退出则按优雅提前返回。对常驻网关只是把广播推迟约一瞬。
const BOOT_GRACE: Duration = Duration::from_millis(1_500);

/// 串行化整个进程生命周期序列（spawn 与 stop）。原因：
/// 1) 「忽略 CTRL_C」属性是进程级且会被子进程继承，spawn 前要清、广播时要设、
///    广播后不能立刻清（CTRL_C 派发有毫秒级延迟，并行交叠会自伤或让子进程忽略信号）；
/// 2) 广播的 CTRL_C 会到达本进程自己。
/// 生产中同一时刻至多一个网关，串行无副作用（stop 最长 10 秒）。
static LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

/// 网关相关路径（全部运行期解析，不硬编码安装位置）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayPaths {
    /// 自带 node.exe（externalBin 剥 triple 后落在主程序 exe 旁）
    pub node_exe: PathBuf,
    /// 网关脚本目录（resources 落盘，exe 旁 resources\gateway\，三个 .mjs 必须同处一目录）
    pub gateway_dir: PathBuf,
    /// 诊断日志：%APPDATA%\ag-relay\logs\relay-diagnostics.jsonl
    pub diagnostics_file: PathBuf,
}

/// 诊断文件名（上游网关固定落位的 live 诊断文件）。**单一真值**：诊断读取层
/// `gateway_diagnostics_reader` 复用本常量，杜绝文件名在两处各写字面量而漂移。
pub const DIAGNOSTICS_FILE_NAME: &str = "relay-diagnostics.jsonl";

/// 由 exe 所在目录与应用数据目录解析网关路径。
pub fn resolve_paths(exe_dir: &Path, app_data_dir: &Path) -> GatewayPaths {
    GatewayPaths {
        node_exe: exe_dir.join("node.exe"),
        gateway_dir: exe_dir.join("resources").join("gateway"),
        // 日志目录知识聚拢（F3）：与「打开日志文件夹」同一真值 gateway_control::logs_dir
        diagnostics_file: crate::gateway_control::logs_dir(app_data_dir)
            .join(DIAGNOSTICS_FILE_NAME),
    }
}

#[derive(Debug)]
pub enum GatewayError {
    NodeNotFound(PathBuf),
    SupervisorScriptNotFound(PathBuf),
    PortAlreadyInUse { host: String, port: u16 },
    SpawnFailed(String),
    JobSetupFailed(&'static str),
    InvalidPolicy(String),
    InvalidProxy(&'static str),
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GatewayError::NodeNotFound(p) => write!(f, "自带 node 运行时缺失：{}", p.display()),
            GatewayError::SupervisorScriptNotFound(p) => {
                write!(f, "网关脚本缺失：{}", p.display())
            }
            GatewayError::PortAlreadyInUse { host, port } => {
                write!(
                    f,
                    "端口 {host}:{port} 已被占用，网关可能已在运行，为避免重复拉起未执行启动"
                )
            }
            GatewayError::InvalidPolicy(why) => write!(f, "策略设置无效：{why}"),
            GatewayError::InvalidProxy(why) => {
                write!(f, "网络代理配置无效：{why}；请使用 HTTP(S) 代理地址")
            }
            GatewayError::SpawnFailed(why) => write!(f, "拉起网关进程失败：{why}"),
            GatewayError::JobSetupFailed(why) => {
                write!(f, "进程树兜底（Job Object）建立失败：{why}")
            }
        }
    }
}

impl std::error::Error for GatewayError {}

/// Job Object 句柄 newtype：裸指针本身非 Send/Sync，但句柄只是整数，
/// 所有 Win32 调用都发生在 LIFECYCLE_LOCK 串行区内，跨线程移动/共享是安全的。
pub struct JobHandle(pub(crate) HANDLE);
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0); }
    }
}

/// 受管子进程：壳持有 Child 与 Job Object 句柄。
/// 刻意不实现「Drop 杀进程」（std Child 默认不 kill on drop）——
/// 生命周期只由 stop_managed / KILL_ON_JOB_CLOSE 决定。
pub struct ManagedChild {
    pub(crate) child: Child,
    /// supervisor 进程 PID（CTRL_C 广播 attach 的目标）
    pub supervisor_pid: u32,
    /// KILL_ON_JOB_CLOSE 的 Job Object 句柄；存活至本进程退出（内核兜底杀整树）
    pub(crate) job: JobHandle,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// 优雅退出（supervisor 排空后退出；75=热重启排空，0=关停排空，均属优雅）
    Graceful { exit_code: Option<i32> },
    /// 优雅窗口超时，TerminateJobObject 强杀整树
    ForceKilled,
}

fn create_kill_on_close_job() -> Result<HANDLE, &'static str> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err("CreateJobObjectW 失败");
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            return Err("SetInformationJobObject 失败");
        }
        Ok(job)
    }
}

/// 通用受管 spawn：node 运行脚本，CREATE_NO_WINDOW，挂 KILL_ON_JOB_CLOSE Job。
/// extra_env 在最小系统环境之上追加（网关配置、诊断路径及显式代理白名单）。
pub fn spawn_managed(
    node_exe: &Path,
    script: &Path,
    cwd: &Path,
    extra_env: &[(String, String)],
) -> Result<ManagedChild, GatewayError> {
    if !node_exe.is_file() {
        return Err(GatewayError::NodeNotFound(node_exe.to_path_buf()));
    }
    if !script.is_file() {
        return Err(GatewayError::SupervisorScriptNotFound(script.to_path_buf()));
    }

    let _lifecycle = LIFECYCLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut cmd = Command::new(node_exe);
    // 只继承 Node 在 Windows 上运行必需的非凭据环境；配置、日志路径由显式参数给出。
    // 避免继承外部 PROXY_LOG_FILE / NODE_OPTIONS，测试与生产均不会写到另一实例的数据目录。
    cmd.env_clear();
    for key in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) { cmd.env(key, value); }
    }
    cmd.arg(script).current_dir(cwd);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // stdio 全置 null：网关是后台进程，诊断日志走 PROXY_DIAGNOSTICS_FILE，stdout 无消费方。
    // ⚠ T7 实测：GUI 壳（windows_subsystem，无控制台）优雅停止后（AttachConsole→广播→
    //   FreeConsole 往返），进程会遗留失效的 std 句柄；默认 stdio=inherit 把它们传给
    //   CreateProcess 导致「停止后再启动」稳定报 SpawnFailed（os error 6 INVALID_HANDLE）。
    //   启动期首拉不受影响（那时 std 句柄还是 NULL），故 T2–T6 均未暴露。置 null 与
    //   首拉时的 NULL 句柄语义一致，spawn 不再依赖壳自身的控制台状态。
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // 只用 CREATE_NO_WINDOW；绝不用 CREATE_NEW_PROCESS_GROUP（隐式禁 Ctrl+C）
    cmd.creation_flags(CREATE_NO_WINDOW);

    // ⚠ 实测关键：「忽略 CTRL_C」属性会沿进程树继承。若壳本身运行在设置了该属性的
    // 环境（自动化工具/部分终端链路），子进程会继承它导致 CTRL_C 广播被静默忽略。
    // spawn 前先清除本进程的忽略属性，保证子进程继承到可响应 CTRL_C 的干净状态。
    unsafe {
        SetConsoleCtrlHandler(None, 0);
    }

    let mut child = cmd.spawn().map_err(|e| GatewayError::SpawnFailed(e.to_string()))?;
    let supervisor_pid = child.id();

    let job = create_kill_on_close_job().map_err(|why| {
        let _ = child.kill();
        let _ = child.wait();
        GatewayError::JobSetupFailed(why)
    })?;
    unsafe {
        if AssignProcessToJobObject(job, child.as_raw_handle()) == 0 {
            // 杀不掉就不留孤儿：TerminateJobObject + 关 job 句柄（KILL_ON_JOB_CLOSE 双保险）
            TerminateJobObject(job, 1);
            let _ = child.wait();
            return Err(GatewayError::JobSetupFailed("AssignProcessToJobObject 失败"));
        }
    }

    Ok(ManagedChild {
        child,
        supervisor_pid,
        job: JobHandle(job),
    })
}

/// 把 CTRL_C 广播到目标进程所在的控制台（受管子进程与外部 supervisor 共用，F1）：
/// supervisor 与网关都注册了 SIGINT → 优雅 drain。壳自己注册空处理器忽略该信号。
///
/// 实测坑：FreeConsole 之后立即 AttachConsole 会偶发失败（err=6 INVALID_HANDLE），
/// 需要间隔重试（conhost 侧收尾有延迟）——以下用 50ms 间隔、上限 2 秒的重试。
/// 调用方必须已持有 LIFECYCLE_LOCK。
fn broadcast_ctrl_c_to_console(target_pid: u32) {
    unsafe {
        let mut attached = AttachConsole(target_pid) != 0;
        if !attached {
            // 已有控制台（GUI 壳平时没有；dev/test 从终端启动时有）先脱离再挂目标的
            FreeConsole();
            for _ in 0..40 {
                if AttachConsole(target_pid) != 0 {
                    attached = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if attached {
            // HandlerRoutine=NULL + Add=TRUE ⇒ 本进程忽略 CTRL_C，避免被一并终止
            SetConsoleCtrlHandler(None, 1);
            // CTRL_C 不能定向进程组，只能 0=同控制台全体进程（我们已挂到目标控制台）
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
            FreeConsole();
            // 注意：忽略属性保持到本次 stop 全程结束再解除（在 stop 收尾），
            // 因为 CTRL_C 的派发有延迟，立即解除可能被迟到的派发命中。
        }
    }
}

/// 广播窗口结束后解除「忽略 CTRL_C」属性（CTRL_C 派发有毫秒级延迟，
/// 必须等优雅等待期覆盖之后再解除；stop_managed 与外部停止共用此收尾）。
fn reset_console_ctrl_c_ignoring() {
    unsafe {
        SetConsoleCtrlHandler(None, 0);
    }
}

/// 停止受管子进程：先 CTRL_C 优雅（等 ≤timeout），超时 TerminateJobObject 强杀整树。
/// 对已退出的进程调用是安全的（返回其退出码或直接强杀兜底）。
pub fn stop_managed(proc: &mut ManagedChild, timeout: Duration) -> StopOutcome {
    let _lifecycle = LIFECYCLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let outcome = stop_managed_locked(proc, timeout);
    // 广播期间设置的「忽略 CTRL_C」到此解除（等待期已覆盖派发延迟）
    reset_console_ctrl_c_ignoring();
    outcome
}

fn stop_managed_locked(proc: &mut ManagedChild, timeout: Duration) -> StopOutcome {
    // 幂等：已退出直接返回
    if let Ok(Some(status)) = proc.child.try_wait() {
        return StopOutcome::Graceful { exit_code: status.code() };
    }

    // F4：总预算从进入本函数起算，BOOT_GRACE 计入其中
    let deadline = Instant::now() + timeout;

    // 启动缓冲：等子进程完成引导、注册好 SIGINT 处理器再广播（见 BOOT_GRACE 注释）。
    // 期间自行退出（含刚被外部杀掉）都按优雅处理，提前返回。
    let grace_deadline = std::cmp::min(Instant::now() + BOOT_GRACE, deadline);
    while Instant::now() < grace_deadline {
        match proc.child.try_wait() {
            Ok(Some(status)) => return StopOutcome::Graceful { exit_code: status.code() },
            Ok(None) => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    broadcast_ctrl_c_to_console(proc.supervisor_pid);

    loop {
        match proc.child.try_wait() {
            Ok(Some(status)) => {
                return StopOutcome::Graceful { exit_code: status.code() }
            }
            Ok(None) => {}
            Err(_) => break, // 句柄异常时交给强杀兜底
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    unsafe {
        TerminateJobObject(proc.job.0, 1);
    }
    let _ = proc.child.wait();
    StopOutcome::ForceKilled
}

/// 8787 端口探测：被占用（含已有网关实例）返回 true。
pub fn port_in_use(host: &str, port: u16) -> bool {
    TcpListener::bind((host, port)).is_err()
}

/// 启动网关：端口被占则明确报错（不重复拉起）；否则拉起 supervisor。
pub fn start_gateway_process(paths: &GatewayPaths) -> Result<ManagedChild, GatewayError> {
    start_gateway_process_with_settings(paths, &ConnectionSettings::default())
}

pub fn start_gateway_process_with_settings(
    paths: &GatewayPaths,
    settings: &ConnectionSettings,
) -> Result<ManagedChild, GatewayError> {
    let extra_env = gateway_env_pairs_with_settings(&paths.diagnostics_file, settings);
    start_gateway_process_with_env(paths, settings, &extra_env)
}

/// 桌面受管入口：通过 env_clear 后的显式 extra_env 把完整策略交给 supervisor。
pub fn start_gateway_process_with_policy(
    paths: &GatewayPaths,
    settings: &ConnectionSettings,
    policy: &PolicySettings,
) -> Result<ManagedChild, GatewayError> {
    let extra_env = gateway_env_pairs_with_policy(&paths.diagnostics_file, settings, policy)
        .map_err(GatewayError::InvalidPolicy)?;
    start_gateway_process_with_env(paths, settings, &extra_env)
}

fn start_gateway_process_with_env(
    paths: &GatewayPaths,
    settings: &ConnectionSettings,
    extra_env: &[(String, String)],
) -> Result<ManagedChild, GatewayError> {
    if port_in_use(settings.host(), settings.port()) {
        return Err(GatewayError::PortAlreadyInUse {
            host: settings.host().to_string(),
            port: settings.port(),
        });
    }
    let supervisor_script = paths.gateway_dir.join("relay-supervisor.mjs");
    let mut env = extra_env.to_vec();
    env.extend(
        crate::gateway_proxy::gateway_proxy_env(settings.upstream())
            .map_err(GatewayError::InvalidProxy)?,
    );
    spawn_managed(
        &paths.node_exe,
        &supervisor_script,
        &paths.gateway_dir,
        &env,
    )
}

// ---------------------------------------------------------------------------
// 外部网关的识别与优雅停止（F1「关了就彻底关了」）
// ---------------------------------------------------------------------------

/// 判定命令行是否为本网关 supervisor：大小写不敏感包含特征脚本名 `relay-supervisor.mjs`。
/// 真实形态：壳内 `node.exe relay-supervisor.mjs`（相对路径）；外部 bat/终端多为
/// `node <完整路径>\relay-supervisor.mjs`。直接跑主网关脚本 / cmd / 其他程序不命中。
pub fn is_supervisor_command_line(command_line: &str) -> bool {
    command_line.to_lowercase().contains("relay-supervisor.mjs")
}

/// Toolhelp 进程快照：child_pid 的父进程 pid。快照打不开 / 查无此进程 → None。
pub fn find_parent_pid(child_pid: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut parent = None;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == child_pid {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        parent
    }
}

/// 进程句柄 RAII：离开作用域即 CloseHandle，防句柄泄漏。
struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.0) };
    }
}

/// 读目标进程的完整命令行（UTF-16 → String）。任何一步失败 → None（宁可不杀不可误杀）。
///
/// 手法：OpenProcess(QUERY_LIMITED|VM_READ) → NtQueryInformationProcess(ProcessBasicInformation)
/// 取 PEB 地址 → ReadProcessMemory 读 PEB.ProcessParameters（x64 偏移 +0x20）→
/// RTL_USER_PROCESS_PARAMETERS.CommandLine（x64 偏移 +0x70，UNICODE_STRING）→ 读 Buffer。
/// 仅保证 x64 目标（本项目自带 node 与系统 node 均为 x64）；读到的垃圾数据因不含
/// 特征子串而自然落入「不可核实」，不产生误杀。
pub fn process_command_line(pid: u32) -> Option<String> {
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    /// x64 PEB → ProcessParameters 指针的字节偏移
    const PEB_PROCESS_PARAMETERS: usize = 0x20;
    /// x64 RTL_USER_PROCESS_PARAMETERS → CommandLine（UNICODE_STRING，16 字节）的字节偏移
    const RUPP_COMMAND_LINE: usize = 0x70;

    /// PROCESS_BASIC_INFORMATION 的 x64 布局（只用到 PebBaseAddress）。
    #[repr(C)]
    struct ProcessBasicInfo {
        exit_status: isize,
        peb_base_address: *mut core::ffi::c_void,
        affinity_mask: usize,
        base_priority: isize,
        unique_process_id: usize,
        inherited_from_unique_process_id: usize,
    }

    unsafe fn read_process_mem(
        handle: windows_sys::Win32::Foundation::HANDLE,
        addr: usize,
        buf: *mut u8,
        len: usize,
    ) -> bool {
        if addr == 0 || buf.is_null() || len == 0 {
            return false;
        }
        ReadProcessMemory(
            handle,
            addr as *const core::ffi::c_void,
            buf as *mut core::ffi::c_void,
            len,
            std::ptr::null_mut(),
        ) != 0
    }

    unsafe {
        let handle =
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
        if handle.is_null() {
            return None;
        }
        let _guard = ProcessHandle(handle);

        let mut pbi: ProcessBasicInfo = std::mem::zeroed();
        let mut returned = 0u32;
        if NtQueryInformationProcess(
            handle,
            ProcessBasicInformation,
            &mut pbi as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<ProcessBasicInfo>() as u32,
            &mut returned,
        ) != 0
            || pbi.peb_base_address.is_null()
        {
            return None;
        }

        let peb = pbi.peb_base_address as usize;
        let mut params = 0usize;
        if !read_process_mem(
            handle,
            peb + PEB_PROCESS_PARAMETERS,
            &mut params as *mut usize as *mut u8,
            std::mem::size_of::<usize>(),
        ) || params == 0
        {
            return None;
        }

        // UNICODE_STRING：Length/MaximumLength（字节）+ 填充 + Buffer 指针
        let mut us = [0u8; 16];
        if !read_process_mem(handle, params + RUPP_COMMAND_LINE, us.as_mut_ptr(), 16) {
            return None;
        }
        let byte_len = u16::from_le_bytes([us[0], us[1]]) as usize;
        let buffer = usize::from_le_bytes(us[8..16].try_into().expect("8 字节指针"));
        // 合理性校验：非空、含结尾 NUL、不超过 32K 字符（UTF-16 64K 字节）
        if buffer == 0 || byte_len < 2 || byte_len > 65_536 {
            return None;
        }
        let mut raw = vec![0u8; byte_len];
        if !read_process_mem(handle, buffer, raw.as_mut_ptr(), byte_len) {
            return None;
        }
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_string())
    }
}

/// 外部停止链的解析结果：从 status 的网关 pid 出发核实 supervisor。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalStopTarget {
    /// 父进程核实为本网关 supervisor（命令行含 relay-supervisor.mjs）
    Supervisor { pid: u32 },
    /// 父链任一环节拿不到 / 父进程不是本网关 supervisor（宁可不杀不可误杀）
    NotVerifiable,
}

/// 由网关 pid 核实其 supervisor：网关 pid →（Toolhelp）父 pid →（PEB）父命令行
/// 含 relay-supervisor.mjs。任何一环不成立都返回 NotVerifiable，绝不猜测。
pub fn resolve_external_stop_target(gateway_pid: u32) -> ExternalStopTarget {
    if gateway_pid == 0 {
        return ExternalStopTarget::NotVerifiable;
    }
    let Some(parent_pid) = find_parent_pid(gateway_pid) else {
        return ExternalStopTarget::NotVerifiable;
    };
    if parent_pid == 0 {
        return ExternalStopTarget::NotVerifiable;
    }
    let Some(command_line) = process_command_line(parent_pid) else {
        return ExternalStopTarget::NotVerifiable;
    };
    if !is_supervisor_command_line(&command_line) {
        return ExternalStopTarget::NotVerifiable;
    }
    ExternalStopTarget::Supervisor { pid: parent_pid }
}

/// 外部停止结果（F1）。调用方按结果记白名单事件（app_exit::log_external_stop）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalStopOutcome {
    /// 状态端点不可达：8787 上没有可识别的网关，未做任何动作
    NoGateway,
    /// 端点可达但 pid 链无法核实，已放弃（宁可不杀不可误杀），未做任何动作
    Abandoned,
    /// 已广播 CTRL_C，supervisor（连同网关整树）在预算内退出
    StoppedGraceful,
    /// 优雅窗口超时，已 `taskkill /T /F` 强杀 supervisor 进程树
    ForceKilled,
}

/// 外部优雅停止（F1）：本壳没有受管网关、但 8787 上有网关在跑时，
/// 从 status 的网关 pid 核实出 supervisor 后对其广播 CTRL_C
/// （supervisor 捕获 SIGINT → drain 网关 → 退出），总预算 timeout
/// （BOOT_GRACE 计入，F4 语义同 stop_managed）。超时则 `taskkill /T /F` 树杀——
/// 外部网关不归本壳的 Job Object 管，只能树杀。
/// 边界：
/// - pid 链任何一环拿不到 / 父进程不是本网关 supervisor → Abandoned（宁可不杀不可误杀）；
/// - 检查后、停止前网关自然退出 → 幂等返回 StoppedGraceful
///   （OpenProcess 打不开 / 等待句柄已触发，均按「已退出」处理）。
pub fn stop_external_gateway(port: u16, timeout: Duration) -> ExternalStopOutcome {
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    let body = match crate::gateway_status::fetch_status_body(GATEWAY_HOST, port) {
        Ok(body) => body,
        Err(_) => return ExternalStopOutcome::NoGateway,
    };
    let Some(gateway_pid) = crate::gateway_status::parse_status_pid(&body) else {
        return ExternalStopOutcome::Abandoned;
    };
    let ExternalStopTarget::Supervisor { pid: supervisor_pid } =
        resolve_external_stop_target(gateway_pid)
    else {
        return ExternalStopOutcome::Abandoned;
    };

    // 广播前打开句柄：SYNCHRONIZE 用于等待退出。打不开 ≈ 进程刚好已退出
    // （同用户进程不存在权限问题）→ 按「自然退出」幂等处理，不做任何杀动作。
    let handle = unsafe {
        OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, supervisor_pid)
    };
    if handle.is_null() {
        return ExternalStopOutcome::StoppedGraceful;
    }
    let _guard = ProcessHandle(handle);

    // 广播与等待要动控制台/信号状态，与 spawn/stop 互斥（同 LIFECYCLE_LOCK）
    let _lifecycle = LIFECYCLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 总预算（F4 语义）：BOOT_GRACE 计入——外部网关可能刚被拉起（node 引导未完成时
    // 广播会命中默认处置裸死），缓冲期间 supervisor 已退出则按自然退出处理。
    let deadline = Instant::now() + timeout;
    let grace_deadline = std::cmp::min(Instant::now() + BOOT_GRACE, deadline);
    while Instant::now() < grace_deadline {
        if unsafe { WaitForSingleObject(handle, 50) } != WAIT_TIMEOUT {
            return ExternalStopOutcome::StoppedGraceful;
        }
    }

    broadcast_ctrl_c_to_console(supervisor_pid);

    // 等待优雅退出：supervisor 退出 ⇒ 网关已被 drain 停止或随树带走
    loop {
        let wait = unsafe { WaitForSingleObject(handle, 100) };
        if wait != WAIT_TIMEOUT {
            reset_console_ctrl_c_ignoring();
            return ExternalStopOutcome::StoppedGraceful;
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    // 超时强杀：taskkill /T /F 树杀 supervisor（外部进程不归本壳 Job 管）
    let kill_ok = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &supervisor_pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let exited = unsafe { WaitForSingleObject(handle, 3_000) } != WAIT_TIMEOUT;
    reset_console_ctrl_c_ignoring();
    if kill_ok || exited {
        ExternalStopOutcome::ForceKilled
    } else {
        // 连 taskkill 都没生效且进程还活着：如实上报，调用方记「放弃」
        ExternalStopOutcome::Abandoned
    }
}

// ---------------------------------------------------------------------------
// 测试：桩进程生命周期 + 路径解析 + 端口占用
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_env::gateway_env_table;
    use std::net::TcpStream;
    use std::sync::Mutex as TestMutex;

    // ------------------------------------------------------------------
    // /_relay/status 完整解析（F6：生产代码已无调用方——T3 起壳统一走
    // gateway_status 的宽松解析与四卡快照；完整结构体仅供真网关冒烟断言用。
    // 字段保留全量形状作为契约文档，个别字段不在冒烟断言内 → allow(dead_code)）
    // ------------------------------------------------------------------

    #[allow(dead_code)]
    #[derive(Debug, Clone, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct RelayConfig {
        pub status_interval_ms: u64,
        pub stream_idle_ms: u64,
        pub stream_progress_ms: u64,
        pub connect_timeout_ms: u64,
        pub total_budget_ms: u64,
        pub max_attempts: u64,
        /// status 输出里是布尔（实测：未设 PROXY_SESSION_COMPAT 时默认也为 true）
        pub session_compat: bool,
    }

    #[allow(dead_code)]
    #[derive(Debug, Clone, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct RelayStatus {
        pub loaded_version: String,
        pub pid: u32,
        pub supervisor_enabled: bool,
        pub restarting: bool,
        pub draining: bool,
        pub active_requests: u64,
        pub config: RelayConfig,
    }

    /// GET http://127.0.0.1:{port}/_relay/status（网关本地控制端点要求 Host 头精确匹配端口）。
    /// HTTP 细节（HTTP/1.0 + 仅 Host 头、超时）统一在 gateway_status::fetch_status_body。
    pub fn get_relay_status(port: u16) -> Result<RelayStatus, String> {
        let body = crate::gateway_status::fetch_status_body(GATEWAY_HOST, port)?;
        serde_json::from_str(body.as_str()).map_err(|e| format!("解析状态 JSON 失败：{e}"))
    }

    /// 旧控制台广播桩仍需串行；真实核心测试仅使用动态端口与 HTTP 安全停机。
    static PORT_TESTS_LOCK: TestMutex<()> = TestMutex::new(());

    /// 持 LIFECYCLE_LOCK 后执行 stop 并返回（耗时不含与并行测试排队等锁的时间）。
    /// 等价于 stop_managed，只是把「持锁」与「计时」的责任留给调用方——
    /// 直接用 stop_managed 计时会把锁排队时间算进优雅关停耗时（桩2 的强杀窗口
    /// 持锁约 10 秒，桩1 满载下排队 + 0.7 秒优雅退出 > 10 秒断言，误报）。
    fn timed_stop_locked(proc: &mut ManagedChild, timeout: Duration) -> (StopOutcome, Duration) {
        let lifecycle = LIFECYCLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let started = Instant::now();
        let outcome = stop_managed_locked(proc, timeout);
        let elapsed = started.elapsed();
        drop(lifecycle);
        // 与 stop_managed 收尾一致：解除广播期间设置的「忽略 CTRL_C」属性
        reset_console_ctrl_c_ignoring();
        (outcome, elapsed)
    }

    fn manifest_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn sidecar_node() -> PathBuf {
        manifest_dir().join("binaries").join("node-x86_64-pc-windows-msvc.exe")
    }

    fn fixtures_dir() -> PathBuf {
        manifest_dir().join("tests").join("fixtures")
    }

    /// 领一个空闲端口给桩当就绪信号用（先占后放，桩会立刻绑定）。
    fn free_port() -> u16 {
        let listener = TcpListener::bind((GATEWAY_HOST, 0)).expect("bind 随机端口");
        listener.local_addr().expect("local_addr").port()
    }

    /// 等桩就绪：以「桩监听的 STUB_READY_PORT 可连接」为信号。
    /// 端口可连 = node 引导完成、用户 JS 已运行、SIGINT 控制台处理器已注册。
    /// 不等它就广播 CTRL_C 会在高负载下命中 node 引导窗口，桩以
    /// STATUS_CONTROL_C_EXIT 默认终止（实测：全量并行测试时稳定复现的竞态）。
    fn wait_stub_ready(port: u16) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if TcpStream::connect((GATEWAY_HOST, port)).is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "桩 10 秒内未就绪（STUB_READY_PORT={port}）"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// spawn 一个带就绪信号的桩并等到它真正可用。
    fn spawn_ready_stub(script: &Path) -> ManagedChild {
        let ready_port = free_port();
        let env = vec![("STUB_READY_PORT".to_string(), ready_port.to_string())];
        let proc = spawn_managed(&sidecar_node(), script, &fixtures_dir(), &env)
            .expect("spawn 桩");
        wait_stub_ready(ready_port);
        proc
    }

    #[test]
    fn 路径解析_node在exe旁_网关脚本在resources下_诊断日志在应用数据logs() {
        let paths = resolve_paths(Path::new("C:\\安装目录"), Path::new("C:\\Users\\x\\AppData\\Roaming\\ag-relay"));
        assert_eq!(paths.node_exe, PathBuf::from("C:\\安装目录\\node.exe"));
        assert_eq!(paths.gateway_dir, PathBuf::from("C:\\安装目录\\resources\\gateway"));
        assert_eq!(
            paths.diagnostics_file,
            PathBuf::from("C:\\Users\\x\\AppData\\Roaming\\ag-relay\\logs\\relay-diagnostics.jsonl")
        );
    }

    #[test]
    fn 端口常量与环境变量表保持一致() {
        let table = gateway_env_table();
        let (_, port_value) = table.iter().find(|(k, _)| *k == "PROXY_PORT").expect("表里有 PROXY_PORT");
        assert_eq!(port_value.parse::<u16>().unwrap(), GATEWAY_PORT);
        let (_, host_value) = table.iter().find(|(k, _)| *k == "PROXY_HOST").expect("表里有 PROXY_HOST");
        assert_eq!(*host_value, GATEWAY_HOST);
    }

    #[test]
    fn 桩1_收到CTRL_C应优雅退出_耗时远小于10秒上限() {
        // CTRL_C 广播会送达同控制台的所有子进程，桩测试必须串行（本锁是 T2 既定设计）
        let _guard = PORT_TESTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let script = fixtures_dir().join("stub_graceful.mjs");
        let mut proc = spawn_ready_stub(&script);
        let (outcome, elapsed) = timed_stop_locked(&mut proc, STOP_TIMEOUT);

        match outcome {
            StopOutcome::Graceful { exit_code } => {
                assert_eq!(exit_code, Some(0), "桩1 应以退出码 0 优雅退出");
            }
            other => panic!("桩1 应走优雅路径，实际 {:?}", other),
        }
        assert!(
            elapsed < Duration::from_secs(10),
            "优雅关停必须在 10 秒上限内完成，实际 {:?}",
            elapsed
        );
    }

    #[test]
    fn 桩2_忽略CTRL_C_须在10秒上限后被强杀且进程消失() {
        // 同桩1：广播影响同控制台全部子进程，必须串行
        let _guard = PORT_TESTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let script = fixtures_dir().join("stub_stubborn.mjs");
        let mut proc = spawn_ready_stub(&script);
        let (outcome, elapsed) = timed_stop_locked(&mut proc, STOP_TIMEOUT);

        assert!(matches!(outcome, StopOutcome::ForceKilled), "桩2 忽略 CTRL_C，应被强杀，实际 {:?}", outcome);
        assert!(
            elapsed >= Duration::from_secs(9),
            "强杀前应给满优雅窗口（≥9s），实际 {:?}",
            elapsed
        );
        // F4：BOOT_GRACE(1.5s) 计入 10 秒总预算 → 总耗时 ≈10.1s；
        // 若预算不合并（宽限 1.5s 后再等满 10s）会到 11.5s+，此上界锁住总预算语义
        assert!(
            elapsed < Duration::from_secs(11),
            "总预算必须 ≤10s+收尾（BOOT_GRACE 计入），实际 {:?}",
            elapsed
        );
        assert!(
            proc.child.try_wait().expect("try_wait").is_some(),
            "强杀后桩2 必须已退出（进程消失）"
        );
    }

    #[test]
    fn 启动_端口已被占用时返回明确错误且不拉起() {
        let _guard = PORT_TESTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let manifest = manifest_dir();
        let paths = GatewayPaths {
            node_exe: sidecar_node(),
            gateway_dir: manifest.join("resources").join("gateway"),
            diagnostics_file: std::env::temp_dir().join("ag-relay-port-test").join("diag.jsonl"),
        };
        let listener = TcpListener::bind((GATEWAY_HOST, 0)).unwrap();
        let occupied_port = listener.local_addr().unwrap().port();
        let settings = ConnectionSettings::new("http://127.0.0.1:1", occupied_port.into()).unwrap();
        let result = start_gateway_process_with_settings(&paths, &settings);
        drop(listener);
        match result {
            Err(GatewayError::PortAlreadyInUse { host, port }) => {
                assert_eq!((host.as_str(), port), (GATEWAY_HOST, occupied_port));
            }
            other => panic!("端口被占时应返回 PortAlreadyInUse，实际 {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn 真网关冒烟_起停_版本与配置断言_端口释放_诊断日志落位() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let _guard = PORT_TESTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let manifest = manifest_dir();
        // 日志路径经同一真值 logs_dir 构造（F3），与生产 resolve_paths 行为一致
        let diag = crate::gateway_control::logs_dir(
            &std::env::temp_dir().join(format!("ag-relay-smoke-{}", std::process::id())),
        )
        .join("relay-diagnostics.jsonl");
        let paths = GatewayPaths {
            node_exe: sidecar_node(),
            gateway_dir: manifest.join("resources").join("gateway"),
            diagnostics_file: diag.clone(),
        };

        let port = free_port();
        let upstream = TcpListener::bind((GATEWAY_HOST, 0)).unwrap();
        let settings = ConnectionSettings::new(&format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port()), port.into()).unwrap();
        let gw = start_gateway_process_with_settings(&paths, &settings).expect("启动隔离真网关");

        // 断言失败也要把网关停掉，避免污染后续测试
        let result = catch_unwind(AssertUnwindSafe(|| {
            // 轮询 /_relay/status 直到就绪（60 秒上限；status 有 20s 缓存期，不设短超时）
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut status = None;
            while Instant::now() < deadline {
                if let Ok(s) = get_relay_status(port) {
                    status = Some(s);
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            let status = status.expect("60 秒内 /_relay/status 未就绪");

            // 独立真值：sha256sum 对本仓当前核心资源的结果（#22 有界日志 + #23 安全停机）。
            // 核心变更时显式更新，保持「运行的是打包脚本」的精确契约，不以长度检查代替。
            assert_eq!(status.loaded_version, "d9e6343175b5d0e619b7afe1227b2d3016301a743120d1da18b15cb48e9b7dfc");
            // supervisor 链路成立（gateway 由 supervisor fork，PROXY_SUPERVISED 由它自己注入）
            assert!(status.supervisor_enabled, "网关应处于 supervisor 托管模式");
            // status.config 暴露的配置项与环境变量表逐项对上（端到端证明环境变量生效；
            // max_attempts 默认值是 5，110 只能来自 PROXY_MAX_ATTEMPTS）
            assert_eq!(status.config.max_attempts, 110, "PROXY_MAX_ATTEMPTS 未生效");
            assert_eq!(status.config.total_budget_ms, 500_000, "PROXY_TOTAL_BUDGET_MS 未生效");
            assert_eq!(status.config.connect_timeout_ms, 220_000, "PROXY_CONNECT_TIMEOUT_MS 未生效");
            assert_eq!(status.config.status_interval_ms, 20_000, "PROXY_STATUS_INTERVAL_MS 未生效");
            assert_eq!(status.config.stream_idle_ms, 90_000, "PROXY_STREAM_IDLE_MS 未生效");
            assert_eq!(status.config.stream_progress_ms, 180_000, "PROXY_STREAM_PROGRESS_MS 未生效");
            assert_eq!(status.config.session_compat, true, "PROXY_SESSION_COMPAT 未生效");
        }));

        // 控制仅发往本测试独占端口，并核对刚拉起的 supervisor；不广播 CTRL_C。
        assert_eq!(crate::gateway_safe_shutdown::safe_shutdown_at(port, Some(gw.supervisor_pid), STOP_TIMEOUT), Ok(true));

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !port_in_use(GATEWAY_HOST, port),
            "停止后测试端口应释放（无残留网关进程）"
        );
        // 诊断日志写入指定路径（PROXY_DIAGNOSTICS_FILE 生效，而非脚本目录默认值）
        assert!(diag.is_file(), "诊断日志应写入 PROXY_DIAGNOSTICS_FILE 指定路径：{}", diag.display());

        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    // ------------------------------------------------------------------
    // F1 外部停止链：父 pid 定位 + 命令行读取 + supervisor 判定
    // ------------------------------------------------------------------

    #[test]
    fn 外部停止_supervisor命令行判定_真实形态与反例() {
        // 真实形态：壳内相对路径 / 外部完整路径（含引号、大小写混排）
        assert!(is_supervisor_command_line("node.exe relay-supervisor.mjs"));
        assert!(is_supervisor_command_line(
            r#""C:\Program Files\nodejs\node.exe" "D:\网关\relay-supervisor.mjs""#
        ));
        assert!(is_supervisor_command_line("NODE.EXE RELAY-SUPERVISOR.MJS"));
        // 反例：直接跑主网关脚本（无 supervisor）、cmd、占位程序、空串
        assert!(!is_supervisor_command_line("node.exe relay-retry-proxy.mjs"));
        assert!(!is_supervisor_command_line(
            r"C:\Windows\System32\cmd.exe /c start.bat"
        ));
        assert!(!is_supervisor_command_line(r"C:\Windows\System32\notepad.exe"));
        assert!(!is_supervisor_command_line(""));
    }

    #[test]
    fn 外部停止_查无此进程_父链一律不可核实() {
        // pid 0（System Idle）与必然不存在的 pid：快照/命令行任何一环拿不到都必须
        // 返回 NotVerifiable（宁可不杀不可误杀，绝不猜测）
        assert_eq!(resolve_external_stop_target(0), ExternalStopTarget::NotVerifiable);
        assert_eq!(
            resolve_external_stop_target(u32::MAX - 12_345),
            ExternalStopTarget::NotVerifiable
        );
        // 命令行读取对不存在的进程同样必须安全返回 None
        assert_eq!(process_command_line(u32::MAX - 12_345), None);
    }

    #[test]
    fn 外部停止_能定位自身父pid并读到自己进程的命令行() {
        let cmdline = process_command_line(std::process::id()).expect("自进程命令行应可读");
        assert!(!cmdline.is_empty(), "命令行不应为空：{cmdline:?}");
        // 父 pid 定位对活进程应成立（测试运行器的父进程必存在）
        assert!(find_parent_pid(std::process::id()).is_some());
    }

    /// F1 主链路单测（桩进程树）：node 桩（stub_parent.mjs，supervisor 替身）+ 其真子进程
    /// （模拟网关子进程）。验证：
    /// 1) 子进程的父 pid 定位 = 桩（与 status→父链 的真实形状一致）；
    /// 2) 桩的完整命令行可跨进程读取（含脚本名）；
    /// 3) 桩不是 supervisor → 外部停止链必须放弃（宁可不杀不可误杀）。
    #[test]
    fn 外部停止_父链桩_父pid定位与命令行判定真实可走通() {
        // 结尾的停止会广播 CTRL_C（影响同控制台所有子进程），持锁原因与桩1/桩2 相同
        let _guard = PORT_TESTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let script = fixtures_dir().join("stub_parent.mjs");
        let pid_file = std::env::temp_dir()
            .join(format!("ag-relay-stub-child-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);

        let ready_port = free_port();
        let env = vec![
            ("STUB_READY_PORT".to_string(), ready_port.to_string()),
            ("STUB_CHILD_PID_FILE".to_string(), pid_file.to_string_lossy().into_owned()),
        ];
        let mut proc = spawn_managed(&sidecar_node(), &script, &fixtures_dir(), &env)
            .expect("spawn 父链桩");
        wait_stub_ready(ready_port);

        // 等桩把子进程 pid 写出来（子进程 spawn 是异步的）
        let deadline = Instant::now() + Duration::from_secs(10);
        let child_pid: u32 = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    if pid != 0 {
                        break pid;
                    }
                }
            }
            assert!(Instant::now() < deadline, "桩子进程 pid 未就绪（pid 文件缺失）");
            std::thread::sleep(Duration::from_millis(25));
        };

        // 1) 父 pid 定位：子进程的父 = 桩进程
        assert_eq!(
            find_parent_pid(child_pid),
            Some(proc.supervisor_pid),
            "子进程的父 pid 应定位到桩（status→父链 的真实形状）"
        );

        // 2) 命令行跨进程读取：桩命令行含 stub_parent.mjs
        let cmdline = process_command_line(proc.supervisor_pid).expect("桩命令行应可读");
        assert!(
            cmdline.to_lowercase().contains("stub_parent.mjs"),
            "读到的应是桩的真实命令行，实际：{cmdline}"
        );

        // 3) 判定：桩不是 supervisor → 外部停止链必须放弃
        assert!(!is_supervisor_command_line(&cmdline));
        assert_eq!(
            resolve_external_stop_target(child_pid),
            ExternalStopTarget::NotVerifiable,
            "父进程非本网关 supervisor 时必须放弃外部停止（宁可不杀不可误杀）"
        );

        // 清理：CTRL_C 广播影响同控制台，必须串行（锁已持有）
        let (outcome, _) = timed_stop_locked(&mut proc, STOP_TIMEOUT);
        assert!(
            matches!(outcome, StopOutcome::Graceful { .. }),
            "桩收到 CTRL_C 应优雅退出，实际 {:?}",
            outcome
        );
        let _ = std::fs::remove_file(&pid_file);
    }
}
