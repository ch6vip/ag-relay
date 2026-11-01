use ag_relay_lib::{
    app_update::{GatewayHost, UpdateService},
    connection_settings::ConnectionSettingsStore,
    gateway_connection::GatewayConnection,
    gateway_process::GatewayPaths,
    policy_settings::{PolicySettings, PolicySettingsStore},
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn request(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(stream, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer fixture-only-not-a-key\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

struct Fixture {
    dir: PathBuf,
    pub gateway: GatewayConnection,
}
impl Fixture {
    // 既有回归继续走无策略 JSON 的旧环境入口，保留临时脚本的环境覆盖语义。
    fn new(name: &str, upstream: &str, port: u16) -> Self {
        Self::build(name, upstream, port, false)
    }

    // 新策略测试才走生产桌面的完整策略快照入口。
    fn with_policy(name: &str, upstream: &str, port: u16) -> Self {
        Self::build(name, upstream, port, true)
    }

    fn build(name: &str, upstream: &str, port: u16, with_policy: bool) -> Self {
        let dir =
            std::env::temp_dir().join(format!("ag-relay-connection-{name}-{}", std::process::id()));
        let gateway = Self::gateway_at(&dir, upstream, port, with_policy);
        Self { dir, gateway }
    }

    fn gateway_at(
        dir: &std::path::Path,
        upstream: &str,
        port: u16,
        with_policy: bool,
    ) -> GatewayConnection {
        std::fs::create_dir_all(dir.join("gateway")).unwrap();
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // 完整资源闭包，包含后继日志模块；核心绝不从真实安装目录运行。
        for entry in std::fs::read_dir(manifest.join("resources/gateway")).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), dir.join("gateway").join(entry.file_name())).unwrap();
            }
        }
        let store = ConnectionSettingsStore::new(dir.join("connection.json"));
        store.save(upstream, port.into()).unwrap();
        let paths = GatewayPaths {
            node_exe: manifest.join("binaries/node-x86_64-pc-windows-msvc.exe"),
            gateway_dir: dir.join("gateway"),
            diagnostics_file: dir.join("logs/diagnostics.jsonl"),
        };
        if with_policy {
            let policy_store = PolicySettingsStore::new(dir.join("policy-settings.json"));
            GatewayConnection::new_with_policy_store(paths, store, policy_store)
        } else {
            GatewayConnection::new(paths, store)
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // 正常完成先走安全停机；断言 panic 时只回收自己持有的 Job，不按端口杀进程。
        let _ = self.gateway.safe_shutdown(Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// 贯通公开退出入口；不伪造 UpdateGateway，也不把 restart 当作 prepare_exit。
mod safe_exit {
    use super::*;
    use std::{
        io,
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, Arc, Mutex,
        },
        thread::{self, JoinHandle},
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    struct ExitHost(Mutex<GatewayConnection>);
    impl GatewayHost for ExitHost {
        type Gateway = GatewayConnection;

        fn with_gateway<T>(
            &self,
            action: impl FnOnce(&mut GatewayConnection) -> T,
        ) -> Result<T, String> {
            let mut gateway = self.0.try_lock().map_err(|_| "测试网关锁不可用".to_string())?;
            Ok(action(&mut gateway))
        }
    }

    struct ExitDirectory(PathBuf);
    impl ExitDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "ag-relay-safe-exit-{name}-{}-{nonce}",
                std::process::id()
            ));
            // 不复用旧目录；从这里起，后续资源复制/配置写入失败也会清理。
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for ExitDirectory {
        fn drop(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match std::fs::remove_dir_all(&self.0) {
                    Ok(()) => break,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                    Err(error) if Instant::now() >= deadline => {
                        eprintln!("隔离退出测试目录未能清理 {}：{error}", self.0.display());
                        break;
                    }
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            }
        }
    }

    struct ExitFixture {
        // 字段按声明顺序释放：先关闭自己持有的 Job，再删除该子进程的临时目录。
        // 析构不发任何 HTTP 控制请求，尤其不访问成功退出后可能被复用的端口。
        host: ExitHost,
        _dir: ExitDirectory,
    }
    impl ExitFixture {
        fn new(name: &str, upstream: &str, port: u16) -> Self {
            assert_ne!(port, ag_relay_lib::gateway_process::GATEWAY_PORT);
            let dir = ExitDirectory::new(name);
            let gateway = Fixture::gateway_at(&dir.0, upstream, port, false);
            Self { host: ExitHost(Mutex::new(gateway)), _dir: dir }
        }
    }

    // 父对象始终保有监听器，即使 worker 出错也不提前释放端口；Drop 停止并回收线程。
    struct ExitHttpServer {
        listener: TcpListener,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<io::Result<Vec<String>>>>,
    }
    impl ExitHttpServer {
        fn new(
            mut respond: impl FnMut(&mut TcpStream, &str, &AtomicBool) -> io::Result<()>
                + Send
                + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            assert_ne!(listener.local_addr().unwrap().port(), ag_relay_lib::gateway_process::GATEWAY_PORT);
            listener.set_nonblocking(true).unwrap();
            let worker_listener = listener.try_clone().unwrap();
            worker_listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                let mut requests = Vec::new();
                while !stopped.load(Ordering::SeqCst) {
                    let mut stream = match worker_listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    stream.set_nonblocking(false)?;
                    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                    let deadline = Instant::now() + Duration::from_secs(2);
                    let mut bytes = Vec::new();
                    while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() || bytes.len() >= 16384 {
                            return Err(io::Error::new(io::ErrorKind::TimedOut, "测试 HTTP 头未完整到达"));
                        }
                        stream.set_read_timeout(Some(remaining))?;
                        let mut chunk = [0; 4096];
                        let count = stream.read(&mut chunk)?;
                        if count == 0 {
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "测试 HTTP 头中断"));
                        }
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    let headers = String::from_utf8(bytes)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    respond(&mut stream, &headers, &stopped)?;
                    requests.push(headers);
                }
                Ok(requests)
            });
            Self { listener, stop, worker: Some(worker) }
        }

        fn port(&self) -> u16 {
            self.listener.local_addr().unwrap().port()
        }

        fn finish(&mut self) -> Vec<String> {
            self.stop.store(true, Ordering::SeqCst);
            self.worker.take().unwrap().join().expect("隔离 HTTP 线程 panic")
                .expect("隔离 HTTP 服务失败")
        }
    }
    impl Drop for ExitHttpServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                // 断言失败时仍回收线程，但不在展开栈时二次 panic。
                let _ = worker.join();
            }
        }
    }

    fn exit_request(port: u16, path: &str) -> TcpStream {
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        write!(stream, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer fixture-only-not-a-key\r\n\r\n").unwrap();
        stream
    }

    fn assert_complete_response(stream: TcpStream, expected: &str) {
        let mut response = String::new();
        stream.take(65536).read_to_string(&mut response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").expect("完整 HTTP 响应");
        assert!(headers.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(body, expected, "响应必须逐字完整，不能仅含成功片段");
    }

    #[test]
    fn 可取消等待_长请求超过十秒仍完整完成才退出且等待时拒绝新请求() {
        use ag_relay_lib::shutdown_wait::{ShutdownKind, ShutdownWaits};
        const BODY: &str = r#"{"ok":true,"request":"long-held","complete":true}"#;
        let (seen_tx, seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut upstream = ExitHttpServer::new(move |stream, headers, stop| {
            assert!(headers.starts_with("GET /long-hold HTTP/"));
            let _ = seen_tx.send(());
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                if stop.load(Ordering::SeqCst) { return Ok(()); }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "长请求未被测试释放"));
                }
                match release_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
            stream.write_all(upstream_reply(200, "application/json", BODY).as_bytes())
        });
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        let fixture = ExitFixture::new(
            "wait-long-success", &format!("http://127.0.0.1:{}", upstream.port()), port,
        );
        drop(reservation);
        fixture.host.with_gateway(|gateway| gateway.start()).unwrap().unwrap();
        let client = exit_request(port, "/long-hold");
        client.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waits = ShutdownWaits::default();
        let wait = waits.begin(ShutdownKind::Exit).unwrap();
        let started = Instant::now();
        thread::scope(|scope| {
            let release = scope.spawn(|| {
                wait_retry_status(port, |status| status["draining"] == true);
                let deadline = Instant::now() + Duration::from_secs(3);
                while waits.progress().unwrap().remaining_requests != Some(1) {
                    assert!(Instant::now() < deadline, "面板应显示仍有一个请求");
                    thread::sleep(Duration::from_millis(10));
                }
                assert!(request(port, "/must-not-reach-upstream").starts_with("HTTP/1.1 503"));
                // 跨过旧十秒停机预算，也跨过一次 lease，必须靠 refresh 继续等待。
                while started.elapsed() < Duration::from_secs(11) {
                    thread::sleep(Duration::from_millis(20));
                }
                let status = retry_status(port);
                assert_eq!(status["draining"], true);
                assert_eq!(status["activeRequests"], 1);
                release_tx.send(()).unwrap();
            });
            assert!(fixture.host.with_gateway(|gateway| gateway.safe_shutdown_wait(&wait))
                .unwrap().unwrap());
            release.join().unwrap();
        });
        assert!(started.elapsed() >= Duration::from_secs(11));
        assert_complete_response(client, BODY);
        fixture.host.with_gateway(|gateway| {
            assert!(!gateway.holds_managed());
            assert_eq!(gateway.managed_supervisor_pid(), None);
        }).unwrap();
        let requests = upstream.finish();
        assert_eq!(requests.len(), 1, "排空期间的503请求不能到达上游，原请求不能重放");
        drop(wait);
        assert!(waits.progress().is_none());
        // 退出成功后不探测/控制端口，fixture Drop 只释放自己的 Job。
    }

    #[test]
    fn 可取消等待_取消恢复接单保留原请求与句柄且明确重试才退出() {
        use ag_relay_lib::gateway_connection::ConnectionError;
        use ag_relay_lib::shutdown_wait::{ShutdownKind, ShutdownPhase, ShutdownWaits};
        const HELD_BODY: &str = r#"{"ok":true,"request":"cancel-held","complete":true}"#;
        const NEXT_BODY: &str = r#"{"ok":true,"request":"after-wait-cancel","complete":true}"#;
        let (seen_tx, seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut upstream = ExitHttpServer::new(move |stream, headers, stop| {
            let held = headers.starts_with("GET /cancel-hold HTTP/");
            if held {
                let _ = seen_tx.send(());
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    if stop.load(Ordering::SeqCst) { return Ok(()); }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "取消测试未释放请求"));
                    }
                    match release_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
            }
            stream.write_all(upstream_reply(
                200, "application/json", if held { HELD_BODY } else { NEXT_BODY },
            ).as_bytes())
        });
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        let fixture = ExitFixture::new(
            "wait-cancel", &format!("http://127.0.0.1:{}", upstream.port()), port,
        );
        drop(reservation);
        let original_supervisor = fixture.host.with_gateway(|gateway| gateway.start()).unwrap().unwrap();
        let original_core = retry_status(port)["pid"].clone();
        let client = exit_request(port, "/cancel-hold");
        seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waits = ShutdownWaits::default();
        let wait = waits.begin(ShutdownKind::Exit).unwrap();
        let failure = thread::scope(|scope| {
            let cancel = scope.spawn(|| {
                wait_retry_status(port, |status| status["draining"] == true);
                // 生命周期锁正在被停机调用持有，取消只取等待状态锁，不能卡住。
                assert!(fixture.host.0.try_lock().is_err());
                assert!(request(port, "/reject-during-wait").starts_with("HTTP/1.1 503"));
                waits.cancel(wait.operation_id()).unwrap();
            });
            let failure = fixture.host.with_gateway(|gateway| gateway.safe_shutdown_wait(&wait))
                .unwrap().unwrap_err();
            cancel.join().unwrap();
            failure
        });
        assert_eq!(failure, ConnectionError::Failed(
            "已取消等待，网关已恢复接单；未强制结束请求。".into(),
        ));
        assert_eq!(waits.progress().unwrap().phase, ShutdownPhase::Cancelling);
        let status = retry_status(port);
        assert_eq!(status["pid"], original_core);
        assert_eq!(status["draining"], false);
        assert_eq!(status["activeRequests"], 1);
        fixture.host.with_gateway(|gateway| {
            assert!(gateway.holds_managed());
            assert_eq!(gateway.managed_supervisor_pid(), Some(original_supervisor));
        }).unwrap();
        release_tx.send(()).unwrap();
        assert_complete_response(client, HELD_BODY);
        assert_complete_response(exit_request(port, "/after-wait-cancel"), NEXT_BODY);
        wait_retry_status(port, |status| status["activeRequests"] == 0);
        let status = retry_status(port);
        assert_eq!(status["pid"], original_core, "取消不能暗中重启或延迟退出");
        assert_eq!(status["draining"], false);
        let requests = upstream.finish();
        assert_eq!(requests.len(), 2, "拒绝的新请求不能转发，原请求不能重放");
        assert!(requests[0].starts_with("GET /cancel-hold HTTP/"));
        assert!(requests[1].starts_with("GET /after-wait-cancel HTTP/"));
        let old_id = wait.operation_id();
        drop(wait);
        assert!(waits.progress().is_none());
        let retry = waits.begin(ShutdownKind::Exit).unwrap();
        assert!(waits.cancel(old_id).is_err(), "旧编号不得取消明确重试");
        assert!(fixture.host.with_gateway(|gateway| gateway.safe_shutdown_wait(&retry))
            .unwrap().unwrap());
        fixture.host.with_gateway(|gateway| assert!(!gateway.holds_managed())).unwrap();
    }

    #[test]
    fn 可取消等待_预算耗尽确认撤销后才报超时且不丢受管句柄() {
        use ag_relay_lib::gateway_connection::ConnectionError;
        use ag_relay_lib::shutdown_wait::{ShutdownKind, ShutdownWaits};
        const BODY: &str = r#"{"ok":true,"request":"timeout-held","complete":true}"#;
        let (seen_tx, seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut upstream = ExitHttpServer::new(move |stream, _, stop| {
            let _ = seen_tx.send(());
            loop {
                if stop.load(Ordering::SeqCst) { return Ok(()); }
                match release_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
            stream.write_all(upstream_reply(200, "application/json", BODY).as_bytes())
        });
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        let fixture = ExitFixture::new(
            "wait-budget", &format!("http://127.0.0.1:{}", upstream.port()), port,
        );
        drop(reservation);
        let supervisor = fixture.host.with_gateway(|gateway| gateway.start()).unwrap().unwrap();
        let core = retry_status(port)["pid"].clone();
        let client = exit_request(port, "/timeout-hold");
        seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waits = ShutdownWaits::default();
        let wait = waits.begin_with_timeout(ShutdownKind::Update, Duration::from_millis(500)).unwrap();
        assert_eq!(fixture.host.with_gateway(|gateway| gateway.safe_shutdown_wait(&wait)).unwrap(),
            Err(ConnectionError::Timeout));
        let status = retry_status(port);
        assert_eq!(status["pid"], core);
        assert_eq!(status["draining"], false);
        assert_eq!(status["activeRequests"], 1);
        fixture.host.with_gateway(|gateway| {
            assert!(gateway.holds_managed());
            assert_eq!(gateway.managed_supervisor_pid(), Some(supervisor));
        }).unwrap();
        release_tx.send(()).unwrap();
        assert_complete_response(client, BODY);
        upstream.finish();
        drop(wait);
        let retry = waits.begin(ShutdownKind::Update).unwrap();
        assert!(fixture.host.with_gateway(|gateway| gateway.safe_shutdown_wait(&retry))
            .unwrap().unwrap());
    }

    #[test]
    fn 可取消等待_旧能力明确拒绝且绝不退回旧十秒协议() {
        use ag_relay_lib::gateway_connection::ConnectionError;
        use ag_relay_lib::shutdown_wait::{ShutdownKind, ShutdownWaits};
        for version in [None, Some(0), Some(2)] {
            let mut body = serde_json::json!({
                "pid": std::process::id(), "safeShutdownVersion": 1,
                "draining": false, "activeRequests": 0
            });
            if let Some(version) = version { body["drainWaitVersion"] = serde_json::json!(version); }
            let mut server = ExitHttpServer::new(move |stream, headers, _| {
                let reply = if headers.starts_with("GET /_relay/status HTTP/") {
                    upstream_reply(200, "application/json", &body.to_string())
                } else {
                    upstream_reply(403, "application/json", r#"{"error":"unexpected_control"}"#)
                };
                stream.write_all(reply.as_bytes())
            });
            let fixture = ExitFixture::new(
                "wait-unsupported", &format!("http://127.0.0.1:{}", server.port()), server.port(),
            );
            let waits = ShutdownWaits::default();
            let wait = waits.begin(ShutdownKind::Exit).unwrap();
            assert_eq!(fixture.host.with_gateway(|gateway| gateway.safe_shutdown_wait(&wait)).unwrap(),
                Err(ConnectionError::Failed(
                    "此网关不支持可取消等待，请先手动处理旧实例；不会强制停止".into(),
                )));
            let requests = server.finish();
            assert_eq!(requests.len(), 1, "旧能力仅查状态，不能控制任意PID或回退旧停机");
            assert!(requests[0].starts_with("GET /_relay/status HTTP/"));
        }
    }

    #[test]
    fn 安全退出_旧协议拒绝保留具体原因且不授权退出() {
        for version in [None, Some(0), Some(2)] {
            let mut body = serde_json::json!({
                "pid": std::process::id(),
                "loadedVersion": "0".repeat(64),
                "startedAt": "2026-10-22T00:00:00.000Z",
                "supervisorEnabled": true,
                "restarting": false,
                "draining": false,
                "activeRequests": 0,
                "safeShutdownVersion": 1,
                "requests": []
            });
            if let Some(version) = version {
                body["drainWaitVersion"] = serde_json::json!(version);
            }
            let mut server = ExitHttpServer::new(move |stream, headers, _| {
                let reply = if headers.starts_with("GET /_relay/status HTTP/") {
                    upstream_reply(200, "application/json", &body.to_string())
                } else {
                    // 仅自持假服务，绝不转发控制请求到任何真实进程。
                    upstream_reply(403, "application/json", r#"{"error":"unexpected_control"}"#)
                };
                stream.write_all(reply.as_bytes())
            });
            let fixture = ExitFixture::new(
                "unsupported", &format!("http://127.0.0.1:{}", server.port()), server.port(),
            );
            let updates = UpdateService::<()>::new(false, "0.3.10".into());
            // 协议检查必须先于父链查询；本测试 PID 不是网关，不得拿它解析父链。
            let failure = updates.prepare_exit(&fixture.host).unwrap_err();
            assert_eq!(failure.reason.as_str(), "safe_shutdown_unsupported", "{version:?}");
            assert_eq!(failure.detail, "此网关不支持可取消等待，请先手动处理旧实例；不会强制停止");
            assert_eq!(updates.view().detail, failure.detail);
            assert!(!updates.exit_authorized());
            assert!(!updates.controls_blocked());
            assert!(!fixture.host.with_gateway(|gateway| gateway.holds_managed()).unwrap());
            let requests = server.finish();
            assert_eq!(requests.len(), 1, "仅可读取状态，不能发送停机控制：{requests:?}");
            assert!(requests[0].starts_with("GET /_relay/status HTTP/"));
            // fixture 先于 server 析构；两者析构都不会再连接这个端口。
        }
    }

    #[test]
    fn 旧安全停机_真实十秒超时恢复接单且可再次明确停机() {
        const HELD_BODY: &str = r#"{"ok":true,"request":"held","complete":true}"#;
        const NEXT_BODY: &str = r#"{"ok":true,"request":"after-cancel","complete":true}"#;
        let (seen_tx, seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut upstream = ExitHttpServer::new(move |stream, headers, stop| {
            let held = headers.starts_with("GET /hold HTTP/");
            if held {
                let _ = seen_tx.send(());
                let deadline = Instant::now() + Duration::from_secs(25);
                loop {
                    if stop.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "测试未释放 hold 请求"));
                    }
                    match release_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
            }
            stream.write_all(upstream_reply(
                200, "application/json", if held { HELD_BODY } else { NEXT_BODY },
            ).as_bytes())
        });
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        let fixture = ExitFixture::new(
            "drain-timeout", &format!("http://127.0.0.1:{}", upstream.port()), port,
        );
        // 仅在真核心接管前释放随机非默认端口；生产 start 自己拒绝端口冲突。
        drop(reservation);
        let original_supervisor = fixture.host.with_gateway(|gateway| gateway.start()).unwrap().unwrap();
        let original_core = retry_status(port)["pid"].clone();
        let client = exit_request(port, "/hold");
        seen_rx.recv_timeout(Duration::from_secs(5)).expect("假上游必须先持有在途请求");
        let started = Instant::now();
        let failure = fixture.host.with_gateway(|gateway| gateway.safe_shutdown(Duration::from_secs(10)))
            .unwrap().unwrap_err();
        // 保留旧配置切换入口的真实10秒回归；退出/更新的新入口已由上方测试覆盖。
        assert!(started.elapsed() >= Duration::from_millis(9900));
        assert_eq!(failure, ag_relay_lib::gateway_connection::ConnectionError::Timeout);
        fixture.host.with_gateway(|gateway| {
            assert!(gateway.holds_managed(), "超时不能丢失自己持有的 Job");
            assert_eq!(gateway.managed_supervisor_pid(), Some(original_supervisor));
        }).unwrap();
        let status = retry_status(port);
        assert_eq!(status["pid"], original_core);
        assert_eq!(status["draining"], false);
        assert_eq!(status["activeRequests"], 1);
        release_tx.send(()).unwrap();
        assert_complete_response(client, HELD_BODY);
        assert_complete_response(exit_request(port, "/after-cancel"), NEXT_BODY);
        let status = retry_status(port);
        assert_eq!(status["pid"], original_core, "恢复请求后仍是同一核心，不能暗中重启");
        assert_eq!(status["draining"], false);
        let requests = upstream.finish();
        assert_eq!(requests.len(), 2, "原请求不可重放，新请求必须到达假上游");
        assert!(requests[0].starts_with("GET /hold HTTP/"));
        assert!(requests[1].starts_with("GET /after-cancel HTTP/"));
        assert!(requests.iter().all(|headers| headers.contains("fixture-only-not-a-key")));
        assert!(fixture.host.with_gateway(|gateway| gateway.safe_shutdown(Duration::from_secs(3)))
            .unwrap().expect("用户明确重试后可以安全停机"));
        fixture.host.with_gateway(|gateway| {
            assert!(!gateway.holds_managed());
            assert_eq!(gateway.managed_supervisor_pid(), None);
        }).unwrap();
        // 成功后不再按端口探测/停机；退出确认已经由真实进程句柄完成。
    }
}

#[test]
fn 策略保存_不重启_安全重启后才由核心确认完整生效() {
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port());
    let port = free_port();
    let mut fixture = Fixture::with_policy("policy-apply", &url, port);
    fixture.gateway.start().unwrap();
    let original_pid = fixture.gateway.managed_supervisor_pid();
    let original = fixture.gateway.policy_settings().unwrap();
    assert!(original.active_supported);
    assert_eq!(original.active.as_ref(), Some(&original.saved));
    assert!(original
        .active_revision
        .as_ref()
        .is_some_and(|r| !r.is_empty()));
    assert!(!original.restart_required);
    let mut changed = original.saved.clone();
    changed.profiles.codex.max_attempts = 17;
    changed.profiles.general.base_delay_ms = 333;
    let saved = fixture.gateway.save_policy(&changed).unwrap();
    assert_eq!(saved.saved, changed);
    assert_eq!(saved.active, original.active);
    assert_eq!(saved.active_revision, original.active_revision);
    assert!(saved.restart_required);
    assert_eq!(fixture.gateway.managed_supervisor_pid(), original_pid);
    assert!(fixture.gateway.restart(Duration::from_secs(3)).unwrap());
    let applied = fixture.gateway.policy_settings().unwrap();
    assert_eq!(applied.active, Some(changed));
    assert!(!applied.restart_required);
    assert_ne!(applied.active_revision, original.active_revision);
    assert_ne!(fixture.gateway.managed_supervisor_pid(), original_pid);
}

#[test]
fn 未覆盖错误保底_仅新字段变化需重启且生效回读保留关闭() {
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port());
    let port = free_port();
    let mut fixture = Fixture::with_policy("fallback-apply", &url, port);
    fixture.gateway.start().unwrap();
    for codex in [true, false] {
        let original = fixture.gateway.policy_settings().unwrap();
        let original_pid = fixture.gateway.managed_supervisor_pid();
        assert!(original.active_supported);
        assert_eq!(original.active.as_ref(), Some(&original.saved));
        assert!(!original.restart_required);
        let mut changed = original.saved.clone();
        let profile = if codex { &mut changed.profiles.codex } else { &mut changed.profiles.general };
        assert!(profile.uncovered_error_fallback);
        profile.uncovered_error_fallback = false;
        let saved = fixture.gateway.save_policy(&changed).unwrap();
        assert_eq!(saved.saved, changed);
        assert_eq!(saved.active, original.active);
        assert_eq!(saved.active_revision, original.active_revision);
        assert!(saved.restart_required, "仅新开关不同也需重启，codex={codex}");
        assert_eq!(fixture.gateway.managed_supervisor_pid(), original_pid);
        assert_eq!(fixture.gateway.policy_settings().unwrap().saved, changed);
        assert!(fixture.gateway.restart(Duration::from_secs(3)).unwrap());
        let applied = fixture.gateway.policy_settings().unwrap();
        assert_eq!(applied.active, Some(changed));
        assert!(applied.active_supported);
        assert!(!applied.restart_required);
        assert_ne!(applied.active_revision, original.active_revision);
    }
}

#[test]
fn 策略重启_排空期间磁盘变化不能替换已确认的启动快照() {
    use std::sync::mpsc;
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port());
    let (seen_tx, seen_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut bytes = [0; 8192];
        stream.read(&mut bytes).unwrap();
        seen_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
    });
    let old_port = free_port();
    let mut fixture = Fixture::with_policy("policy-snapshot", &url, old_port);
    fixture.gateway.start().unwrap();
    let planned_port = free_port();
    let later_port = free_port();
    let mut planned_policy = PolicySettings::default();
    planned_policy.profiles.codex.max_attempts = 13;
    fixture.gateway.save(&url, planned_port.into()).unwrap();
    fixture.gateway.save_policy(&planned_policy).unwrap();
    let client = std::thread::spawn(move || request(old_port, "/hold"));
    seen_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let dir = fixture.dir.clone();
    let replace_during_drain = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let body =
                ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", old_port).unwrap();
            if serde_json::from_str::<serde_json::Value>(&body).unwrap()["draining"] == true {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        ConnectionSettingsStore::new(dir.join("connection.json"))
            .save(&url, later_port.into())
            .unwrap();
        std::fs::write(dir.join("policy-settings.json"), "{").unwrap();
        release_tx.send(()).unwrap();
    });
    assert!(fixture.gateway.restart(Duration::from_secs(5)).unwrap());
    replace_during_drain.join().unwrap();
    assert!(client.join().unwrap().contains("{\"ok\":true}"));
    server.join().unwrap();
    let connection = fixture.gateway.settings().unwrap();
    assert_eq!(connection.active.unwrap().port(), planned_port);
    assert_eq!(connection.saved.port(), later_port);
    let body = ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", planned_port).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["policy"]["settings"],
        serde_json::to_value(planned_policy).unwrap()
    );
}

#[test]
fn 策略实况_只接受已核实核心的完整有效回执() {
    use serde_json::json;
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port());
    let owner_port = free_port();
    let mut owner = Fixture::with_policy("policy-status", &url, owner_port);
    owner.gateway.start().unwrap();
    let pid = ag_relay_lib::gateway_status::parse_status_pid(
        &ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", owner_port).unwrap(),
    )
    .unwrap();
    let mut effective = PolicySettings::default();
    effective.profiles.codex.max_attempts = 19;
    let revision = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
    let complete = json!({"pid": pid, "policy": {"revision": revision, "settings": effective}});
    let mut cases = vec![
        (json!({"pid": pid}), false),
        (json!({"pid": pid, "policy": null}), false),
        (
            json!({"pid": pid, "policy": {"revision": revision, "settings": {"schemaVersion": 1}}}),
            false,
        ),
    ];
    for pointer in [
        "/pid",
        "/policy/revision",
        "/policy/settings/profiles/codex/maxAttempts",
        "/policy/settings/profiles/codex/uncoveredErrorFallback",
        "/policy/settings/profiles/general/uncoveredErrorFallback",
    ] {
        let mut missing = complete.clone();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        missing
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        cases.push((missing, false));
    }
    for (pointer, value) in [
        ("/pid", json!(0)),
        ("/policy/revision", json!("")),
        ("/policy/revision", json!(42)),
        ("/policy/revision", json!("a".repeat(63))),
        ("/policy/revision", json!("a".repeat(65))),
        ("/policy/revision", json!("g".repeat(64))),
        ("/policy/revision", json!("全".repeat(64))),
        ("/policy/settings/schemaVersion", json!(2)),
        (
            "/policy/settings/profiles/general/stripItemIds",
            json!(true),
        ),
        (
            "/policy/settings/profiles/codex/historyMode",
            json!("future_mode"),
        ),
        ("/policy/settings/profiles/general/baseDelayMs", json!(0)),
        ("/policy/settings/profiles/codex/uncoveredErrorFallback", json!("true")),
        ("/policy/settings/profiles/general/uncoveredErrorFallback", json!(null)),
    ] {
        let mut invalid = complete.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        cases.push((invalid, false));
    }
    let mut unknown = complete.clone();
    unknown["policy"]["settings"]["profiles"]["general"]["futureModule"] = json!(false);
    cases.push((unknown, false));
    cases.push((complete, true));
    for (index, (body, supported)) in cases.into_iter().enumerate() {
        // HTTP 是公开系统边界；所有回执与 PID 均来自本测试持有的临时实例。
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut buffer = [0; 4096];
            stream.read(&mut buffer).unwrap();
            let text = body.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            )
            .unwrap();
        });
        let connection =
            ConnectionSettingsStore::new(owner.dir.join(format!("observer-{index}.json")));
        connection.save(&url, port.into()).unwrap();
        let observer = GatewayConnection::new_with_policy_store(
            GatewayPaths {
                node_exe: owner.dir.join("unused-node.exe"),
                gateway_dir: owner.dir.join("gateway"),
                diagnostics_file: owner.dir.join("logs/observer.jsonl"),
            },
            connection,
            PolicySettingsStore::new(owner.dir.join("observer-policy.json")),
        );
        let view = observer.policy_settings().unwrap();
        server.join().unwrap();
        assert_eq!(view.active_supported, supported, "case {index}");
        if supported {
            assert_eq!(view.active, Some(effective.clone()));
            assert_eq!(view.active_revision.as_deref(), Some(revision));
            assert!(
                view.restart_required,
                "必须比较实际回执与保存值，而非假设已生效"
            );
        } else {
            assert!(view.active.is_none());
            assert!(view.active_revision.is_none());
            assert!(!view.restart_required);
        }
    }
}

#[test]
fn 策略损坏_启动拒绝且重启不能先停止旧实例() {
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port());
    let port = free_port();
    let mut fixture = Fixture::with_policy("policy-preflight", &url, port);
    let path = fixture.dir.join("policy-settings.json");
    std::fs::write(&path, "{").unwrap();
    assert!(fixture.gateway.start().is_err());
    assert!(!fixture.gateway.holds_managed());
    PolicySettingsStore::new(path.clone())
        .save(&PolicySettings::default())
        .unwrap();
    fixture.gateway.start().unwrap();
    let original_pid = fixture.gateway.managed_supervisor_pid();
    for invalid in [
        "{",
        r#"{"schemaVersion":2}"#,
        r#"{"schemaVersion":1,"profiles":{"codex":{"historyConfirmations":5}}}"#,
    ] {
        std::fs::write(&path, invalid).unwrap();
        assert!(fixture.gateway.restart(Duration::from_millis(200)).is_err());
        assert_eq!(fixture.gateway.managed_supervisor_pid(), original_pid);
        let body = ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", port).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["draining"],
            false
        );
    }
}

#[test]
fn 安全排空_请求自然完成后才停止整个网关() {
    use std::sync::mpsc;
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut bytes = [0; 8192];
        stream.read(&mut bytes).unwrap();
        seen_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
    });
    let port = free_port();
    let mut fixture = Fixture::new("drain-completion", &format!("http://127.0.0.1:{upstream_port}"), port);
    fixture.gateway.start().unwrap();
    let client = std::thread::spawn(move || request(port, "/hold"));
    seen_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let release = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let body = ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", port).unwrap();
            let status: serde_json::Value = serde_json::from_str(&body).unwrap();
            if status["draining"] == true { break; }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        release_tx.send(()).unwrap();
    });
    assert!(fixture.gateway.safe_shutdown(Duration::from_secs(5)).unwrap());
    assert!(client.join().unwrap().contains("{\"ok\":true}"));
    release.join().unwrap();
    server.join().unwrap();
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
}

#[test]
fn 安全停机_响应中断不能当作退出成功() {
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port());
    let port = free_port();
    let mut fixture = Fixture::new("broken-response", &url, port);
    fixture.gateway.start().unwrap();
    let body = ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", port).unwrap();
    let broken = TcpListener::bind("127.0.0.1:0").unwrap();
    let broken_port = broken.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for index in 0..2 {
            let (mut stream, _) = broken.accept().unwrap();
            let mut bytes = [0; 8192];
            stream.read(&mut bytes).unwrap();
            if index == 0 {
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            } else {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 200\r\nConnection: close\r\n\r\n{\"drained\":").unwrap();
            }
        }
    });
    assert!(ag_relay_lib::gateway_safe_shutdown::safe_shutdown_at(broken_port, fixture.gateway.managed_supervisor_pid(), Duration::from_millis(200)).is_err());
    server.join().unwrap();
    assert!(fixture.gateway.holds_managed());
    assert!(ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", port).is_ok());
    assert!(fixture.gateway.safe_shutdown(Duration::from_secs(3)).unwrap());
}

#[test]
fn 安全停机_外部实例也等待完整父链退出_新端口冲突不停止旧实例() {
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port());
    let port = free_port();
    let mut owner = Fixture::new("external-owner", &url, port);
    owner.gateway.start().unwrap();
    let original = owner.gateway.managed_supervisor_pid();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    owner
        .gateway
        .save(&url, occupied.local_addr().unwrap().port().into())
        .unwrap();
    assert!(owner.gateway.restart(Duration::from_millis(200)).is_err());
    assert_eq!(owner.gateway.managed_supervisor_pid(), original);
    assert!(ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", port).is_ok());
    let mut external = Fixture::new("external-observer", &url, port);
    assert!(!external.gateway.holds_managed());
    assert!(external
        .gateway
        .safe_shutdown(Duration::from_secs(3))
        .unwrap());
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    assert!(!owner.gateway.safe_shutdown(Duration::from_secs(3)).unwrap());
}

#[test]
fn 安全排空_控制请求断连撤销排空_在途响应保持完整() {
    use std::sync::mpsc;
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut bytes = [0; 8192];
        stream.read(&mut bytes).unwrap();
        seen_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
    });
    let port = free_port();
    let mut fixture = Fixture::new(
        "disconnect",
        &format!("http://127.0.0.1:{upstream_port}"),
        port,
    );
    fixture.gateway.start().unwrap();
    let client = std::thread::spawn(move || request(port, "/hold"));
    seen_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let status = || -> serde_json::Value {
        serde_json::from_str(
            &ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", port).unwrap(),
        )
        .unwrap()
    };
    let pid = status()["pid"].as_u64().unwrap();
    let mut control = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(control, "POST /_relay/shutdown HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nx-relay-control: 1\r\nx-relay-pid: {pid}\r\nx-relay-timeout-ms: 5000\r\nContent-Length: 0\r\n\r\n").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while status()["draining"] != true {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        request(port, "/new-request").contains("503"),
        "排空期间不得接入新请求"
    );
    drop(control);
    while status()["draining"] != false {
        assert!(
            std::time::Instant::now() < deadline,
            "调用者断连后应撤销排空"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    release_tx.send(()).unwrap();
    assert!(client.join().unwrap().contains("{\"ok\":true}"));
    server.join().unwrap();
    assert_eq!(status()["pid"], pid);
    assert!(fixture
        .gateway
        .safe_shutdown(Duration::from_secs(3))
        .unwrap());
}

#[test]
fn 安全排空_在途请求超时不强杀_恢复接单且不会稍后偷偷退出() {
    use std::sync::mpsc;
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for index in 0..2 {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut bytes = [0; 8192];
            stream.read(&mut bytes).unwrap();
            if index == 0 {
                seen_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
        }
    });
    let port = free_port();
    let mut fixture = Fixture::new(
        "timeout",
        &format!("http://127.0.0.1:{upstream_port}"),
        port,
    );
    fixture.gateway.start().unwrap();
    let original_pid = fixture.gateway.managed_supervisor_pid();
    let client = std::thread::spawn(move || request(port, "/hold"));
    seen_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let outcome = fixture.gateway.restart(Duration::from_millis(150));
    assert_eq!(
        outcome,
        Err(ag_relay_lib::gateway_connection::ConnectionError::Timeout)
    );
    assert_eq!(
        fixture.gateway.managed_supervisor_pid(),
        original_pid,
        "超时不能丢失或换掉句柄"
    );
    let status: serde_json::Value = serde_json::from_str(
        &ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", port).unwrap(),
    )
    .unwrap();
    assert_eq!(status["draining"], false);
    assert_eq!(status["activeRequests"], 1);
    release_tx.send(()).unwrap();
    assert!(
        client.join().unwrap().contains("{\"ok\":true}"),
        "原请求必须完整完成"
    );
    assert!(
        request(port, "/health").contains("{\"ok\":true}"),
        "完成后仍接受新请求，不能延迟退出"
    );
    server.join().unwrap();
    assert!(
        fixture
            .gateway
            .safe_shutdown(Duration::from_secs(3))
            .unwrap(),
        "稍后可再次安全停机"
    );
}

#[test]
fn 连接切换_保存不打断旧实例_明确重启才注入并释放旧端口() {
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for expected_path in ["/health", "/replacement/health"] {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut bytes = [0; 8192];
            let count = stream.read(&mut bytes).unwrap();
            let received = String::from_utf8_lossy(&bytes[..count]);
            assert!(received.contains("fixture-only-not-a-key"));
            assert!(received.starts_with(&format!("GET {expected_path} HTTP/")));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
        }
    });
    let first = free_port();
    let second = free_port();
    let upstream_url = format!("http://127.0.0.1:{upstream_port}");
    let mut fixture = Fixture::new("switch", &upstream_url, first);
    fixture.gateway.start().unwrap();
    let saved = fixture.gateway.save(&format!("{upstream_url}/replacement"), second.into()).unwrap();
    assert!(saved.restart_required);
    assert_eq!(saved.endpoint, format!("http://127.0.0.1:{first}"));
    assert!(request(first, "/health").contains("{\"ok\":true}"));
    assert!(fixture.gateway.restart(Duration::from_secs(3)).unwrap());
    assert!(!fixture.gateway.settings().unwrap().restart_required);
    assert!(TcpListener::bind(("127.0.0.1", first)).is_ok());
    assert!(request(second, "/health").contains("{\"ok\":true}"));
    assert!(fixture
        .gateway
        .safe_shutdown(Duration::from_secs(3))
        .unwrap());
    assert!(!fixture.gateway.holds_managed());
    server.join().unwrap();
    let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
    assert!(!diagnostics.contains("fixture-only-not-a-key"));
    let settings = std::fs::read_to_string(fixture.dir.join("connection.json")).unwrap();
    assert!(!settings.contains("fixture-only-not-a-key"));
}

// 重试回归共用既有真实核心/假上游接缝。只绑定动态本机端口，清理自建监听器和受管进程。
struct RetryUpstream {
    url: String,
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    bodies: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl RetryUpstream {
    fn new(replies: Vec<String>) -> Self {
        use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        listener.set_nonblocking(true).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, done) = (count.clone(), stop.clone());
        let captured = bodies.clone();
        let worker = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                        stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                        let mut request = Vec::new();
                        let mut chunk = [0; 4096];
                        // 读取完整 HTTP 请求，便于验证 POST 的兼容修复及保留原始历史。
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => request.extend_from_slice(&chunk[..n]),
                            }
                        }
                        if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n").map(|n| n + 4) {
                            let headers = String::from_utf8_lossy(&request[..header_end]);
                            let length = headers.lines().find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
                            }).unwrap_or(0);
                            while request.len() < header_end + length {
                                match stream.read(&mut chunk) {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => request.extend_from_slice(&chunk[..n]),
                                }
                            }
                            captured.lock().unwrap().push(request[header_end..].to_vec());
                        }
                        let index = seen.fetch_add(1, Ordering::SeqCst);
                        let reply = &replies[index.min(replies.len() - 1)];
                        let _ = stream.write_all(reply.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)),
                    Err(e) => panic!("自建假上游监听失败：{e}"),
                }
            }
        });
        Self { url, count, bodies, stop, worker: Some(worker) }
    }
    fn attempts(&self) -> usize { self.count.load(std::sync::atomic::Ordering::SeqCst) }
    fn bodies(&self) -> Vec<Vec<u8>> { self.bodies.lock().unwrap().clone() }
}
impl Drop for RetryUpstream {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn upstream_reply(status: u16, content_type: &str, body: &str) -> String {
    format!("HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}
fn retry_status(port: u16) -> serde_json::Value {
    serde_json::from_str(&ag_relay_lib::gateway_status::fetch_status_body("127.0.0.1", port).unwrap()).unwrap()
}
fn wait_retry_status(port: u16, check: impl Fn(&serde_json::Value) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let status = retry_status(port);
        if check(&status) { return; }
        assert!(std::time::Instant::now() < deadline, "未出现预期的隔离请求状态：{status}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn responses_request(port: u16, body: &str, client: &str) -> String {
    responses_request_with_headers(port, body, &[("X-Relay-Client", client)])
}

fn responses_request_with_headers(port: u16, body: &str, headers: &[(&str, &str)]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    write!(stream, "POST /v1/responses HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer fixture-only-not-a-key\r\nContent-Type: application/json\r\n").unwrap();
    for (key, value) in headers {
        assert!(!key.contains(['\r', '\n']) && !value.contains(['\r', '\n']));
        write!(stream, "{key}: {value}\r\n").unwrap();
    }
    write!(stream, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// 首发允许清理可选 id；含搜索时，推理编号参与隐式依赖，必须保留。
fn codex_stripped(body: &str) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_str(body).unwrap();
    if let Some(items) = value.get_mut("input").and_then(|value| value.as_array_mut()) {
        let has_web_search = items.iter().any(|item| item["type"] == "web_search_call");
        for item in items.iter_mut() {
            let keep = match item.get("type").and_then(|value| value.as_str()) {
                Some("reasoning") => has_web_search,
                Some("message" | "function_call" | "function_call_output"
                    | "custom_tool_call" | "custom_tool_call_output") => false,
                Some(_) => true,
                None => item.get("role").and_then(|value| value.as_str()).is_none(),
            };
            if !keep {
                item.as_object_mut().unwrap().remove("id");
            }
        }
    }
    value
}

#[test]
fn 重试边界_codex首次同类历史错误即修复() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#;
    let body = r#" {"model":"gpt-6.1-sol","store":false,"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"}]} "#;
    let expected = serde_json::json!({
        "model": "gpt-6.1-sol", "store": false,
        "input": [{"type": "message", "role": "user", "content": "fixture prompt"}]
    });
    let failure = upstream_reply(400, "application/json", error);
    let upstream = RetryUpstream::new(vec![failure, upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
    let port = free_port();
    let mut fixture = Fixture::new("codex-first-error-repair", &upstream.url, port);
    fixture.gateway.start().unwrap();
    let response = responses_request(port, body, "codex");
    assert_eq!(upstream.attempts(), 2, "首次同类历史错误即修复并重试：{response}");
    assert!(response.contains(r#"{"ok":true}"#));
    let captured = upstream.bodies();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body), "首发剥离可省略 id");
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[1]).unwrap(), expected, "修复版删除失效推理");
    let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
    let repairs = diagnostics.lines().filter(|line| {
        serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "repair_applied"
    }).count();
    assert_eq!(repairs, 1, "当次请求只有一次修复");
}

#[test]
fn 重试边界_codex网页搜索首发保留配套推理及编号() {
    let body = r#" {"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"fixture ciphertext"},{"type":"web_search_call","id":"ws_fixture","status":"completed","action":{"type":"search","query":"fixture query"}}]} "#;
    let expected = serde_json::json!({"input": [
        {"type": "message", "role": "user", "content": "fixture prompt"},
        {"type": "reasoning", "id": "rs_fixture", "summary": [], "encrypted_content": "fixture ciphertext"},
        {"type": "web_search_call", "id": "ws_fixture", "status": "completed", "action": {"type": "search", "query": "fixture query"}}
    ]});
    let upstream = RetryUpstream::new(vec![upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
    let port = free_port();
    let mut fixture = Fixture::new("codex-web-search-reasoning-id", &upstream.url, port);
    fixture.gateway.start().unwrap();
    assert!(responses_request(port, body, "codex").contains(r#"{"ok":true}"#));
    assert_eq!(upstream.attempts(), 1);
    let captured = upstream.bodies();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), expected,
        "网页搜索的必需 id 会关联配套 reasoning，首发不能拆掉推理编号");
}

#[test]
fn 重试边界_codex跨资源网页搜索首次报错后去推理和编号并保留完整历史() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"The requested item was created under a different Azure OpenAI resource"}}"#;
    let body = r#" {"model":"gpt-6.1-sol","store":false,"reasoning":{"effort":"high"},"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"},{"type":"web_search_call","id":"ws_search","status":"completed","action":{"type":"search","query":"fixture query","sources":[{"type":"url","url":"https://example.invalid/fixture"}]}},{"type":"web_search_call","id":"ws_open","status":"in_progress","action":{"type":"open_page","url":"https://example.invalid/fixture"}},{"type":"web_search_call","id":"ws_find","status":"searching","action":{"type":"find_in_page","url":"https://example.invalid/fixture","pattern":"fixture"}},{"type":"web_search_call","id":"ws_failed","status":"failed","action":{"type":"search","query":"fixture failed query"}},{"type":"function_call","id":"fc_fixture","call_id":"call_function","name":"fixture_function","arguments":"{\"id\":\"nested_id\"}"},{"type":"function_call_output","id":"fco_fixture","call_id":"call_function","output":"fixture function result"},{"type":"custom_tool_call","id":"ct_fixture","call_id":"call_custom","name":"fixture_tool","input":"{\"encrypted_content\":\"nested_keep\"}"},{"type":"custom_tool_call_output","id":"cto_fixture","call_id":"call_custom","output":[{"type":"input_text","text":"fixture tool result"}]}]} "#;
    let expected_first = serde_json::json!({
        "model": "gpt-6.1-sol", "store": false, "reasoning": {"effort": "high"},
        "input": [
            {"type": "message", "role": "user", "content": "fixture prompt"},
            {"type": "reasoning", "id": "rs_fixture", "encrypted_content": "fixture ciphertext"},
            {"type": "web_search_call", "id": "ws_search", "status": "completed",
             "action": {"type": "search", "query": "fixture query", "sources": [{"type": "url", "url": "https://example.invalid/fixture"}]}},
            {"type": "web_search_call", "id": "ws_open", "status": "in_progress",
             "action": {"type": "open_page", "url": "https://example.invalid/fixture"}},
            {"type": "web_search_call", "id": "ws_find", "status": "searching",
             "action": {"type": "find_in_page", "url": "https://example.invalid/fixture", "pattern": "fixture"}},
            {"type": "web_search_call", "id": "ws_failed", "status": "failed",
             "action": {"type": "search", "query": "fixture failed query"}},
            {"type": "function_call", "call_id": "call_function", "name": "fixture_function", "arguments": "{\"id\":\"nested_id\"}"},
            {"type": "function_call_output", "call_id": "call_function", "output": "fixture function result"},
            {"type": "custom_tool_call", "call_id": "call_custom", "name": "fixture_tool", "input": "{\"encrypted_content\":\"nested_keep\"}"},
            {"type": "custom_tool_call_output", "call_id": "call_custom", "output": [{"type": "input_text", "text": "fixture tool result"}]}
        ]
    });
    let expected_repaired = serde_json::json!({
        "model": "gpt-6.1-sol", "store": false, "reasoning": {"effort": "high"},
        "input": [
            {"type": "message", "role": "user", "content": "fixture prompt"},
            {"type": "web_search_call", "status": "completed",
             "action": {"type": "search", "query": "fixture query", "sources": [{"type": "url", "url": "https://example.invalid/fixture"}]}},
            {"type": "web_search_call", "status": "in_progress",
             "action": {"type": "open_page", "url": "https://example.invalid/fixture"}},
            {"type": "web_search_call", "status": "searching",
             "action": {"type": "find_in_page", "url": "https://example.invalid/fixture", "pattern": "fixture"}},
            {"type": "web_search_call", "status": "failed",
             "action": {"type": "search", "query": "fixture failed query"}},
            {"type": "function_call", "call_id": "call_function", "name": "fixture_function", "arguments": "{\"id\":\"nested_id\"}"},
            {"type": "function_call_output", "call_id": "call_function", "output": "fixture function result"},
            {"type": "custom_tool_call", "call_id": "call_custom", "name": "fixture_tool", "input": "{\"encrypted_content\":\"nested_keep\"}"},
            {"type": "custom_tool_call_output", "call_id": "call_custom", "output": [{"type": "input_text", "text": "fixture tool result"}]}
        ]
    });
    for (carrier, status, ct, error_body) in [
        ("http", 400, "application/json", error.to_string()),
        ("json", 200, "application/json", error.to_string()),
        ("sse", 200, "text/event-stream", format!("data: {error}\n\n")),
    ] {
        let failure = upstream_reply(status, ct, &error_body);
        let upstream = RetryUpstream::new(vec![failure.clone(), failure, upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("codex-web-search-{carrier}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = responses_request(port, body, "codex");
        assert_eq!(upstream.attempts(), 3, "{carrier} 首次历史错误即修复，后续重试沿用冻结正文：{response}");
        assert!(response.contains(r#"{"ok":true}"#));
        let captured = upstream.bodies();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), expected_first, "{carrier} 首发保留推理和完整搜索项");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[1]).unwrap(), expected_repaired, "{carrier} 修复仅移除推理项和顶层编号，搜索内容及普通/自定义工具输入输出与 call_id 原样保留");
        assert_eq!(captured[2], captured[1], "{carrier} 修复后的后续重试必须逐字节复用冻结正文");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        let repairs = diagnostics.lines().filter(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "repair_applied"
        }).count();
        assert_eq!(repairs, 1, "{carrier} 当次请求只执行一次历史修复");
        let enabled = diagnostics.lines().filter(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "session_compat_enabled"
        }).count();
        assert_eq!(enabled, 1, "{carrier} 修复后登记同会话后续请求的预清理");
        for sensitive in ["fixture-only-not-a-key", "fixture ciphertext", "fixture prompt", "fixture query"] {
            assert!(!diagnostics.contains(sensitive));
        }
    }
}

#[test]
fn 重试边界_codex修复后的普通临时错误也计入三次发送上限() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#;
    let body = r#" {"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"}]} "#;
    let expected = serde_json::json!({"input": [
        {"type": "message", "role": "user", "content": "fixture prompt"}
    ]});
    for (name, temporary) in [
        ("503", upstream_reply(503, "application/json", r#"{"error":{"type":"server_error","message":"fixture temporary outage"}}"#)),
        ("429", upstream_reply(429, "application/json", r#"{"error":{"code":"rate_limit_exceeded","message":"fixture rate limit"}}"#)),
    ] {
        let history = upstream_reply(400, "application/json", error);
        let upstream = RetryUpstream::new(vec![
            history, temporary.clone(), temporary.clone(), temporary,
            upstream_reply(200, "application/json", r#"{"ok":true}"#),
        ]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("codex-postrepair-{name}-cap"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = responses_request(port, body, "codex");
        assert_eq!(upstream.attempts(), 4, "修复后再发送三次，{name} 计入上限且不能借用普通重试继续：{response}");
        assert!(response.starts_with("HTTP/1.1 503"), "应有限结束并返回可重试的耗尽错误，而非读取第六条成功哨兵：{response}");
        assert!(!response.contains(r#"{"ok":true}"#));
        let captured = upstream.bodies();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body));
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[1]).unwrap(), expected);
        assert!(captured[1..].iter().all(|sent| sent == &captured[1]), "三次修复版发送必须使用同一份冻结字节");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = diagnostics.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.iter().filter(|r| r["event"] == "repair_applied").count(), 1);
        assert!(rows.iter().any(|r| r["event"] == "codex_resource_exhausted" && r["refusal"] == "resource_attempt_limit"));
    }
}

#[test]
fn 重试边界_自定义工具历史可修复失效推理而不丢失调用记录() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#;
    let body = r#" {"model":"gpt-6.1-sol","store":false,"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"},{"type":"custom_tool_call","id":"ct_fixture","call_id":"call_custom","name":"fixture_tool","input":"{\"id\":\"nested\",\"encrypted_content\":\"keep\"}"},{"type":"custom_tool_call_output","id":"cto_fixture","call_id":"call_custom","output":[{"type":"input_text","text":"fixture tool result"}]},{"type":"function_call","id":"fc_fixture","call_id":"call_function","name":"fixture_function","arguments":"{}"},{"type":"function_call_output","id":"fco_fixture","call_id":"call_function","output":"fixture function result"}]} "#;
    let expected: serde_json::Value = serde_json::from_str(r#"{"model":"gpt-6.1-sol","store":false,"input":[{"type":"message","role":"user","content":"fixture prompt"},{"type":"custom_tool_call","call_id":"call_custom","name":"fixture_tool","input":"{\"id\":\"nested\",\"encrypted_content\":\"keep\"}"},{"type":"custom_tool_call_output","call_id":"call_custom","output":[{"type":"input_text","text":"fixture tool result"}]},{"type":"function_call","call_id":"call_function","name":"fixture_function","arguments":"{}"},{"type":"function_call_output","call_id":"call_function","output":"fixture function result"}]}"#).unwrap();
    for (carrier, status, ct, error_body) in [
        ("http", 400, "application/json", error.to_string()),
        ("json", 200, "application/json", error.to_string()),
        ("sse", 200, "text/event-stream", format!("data: {error}\n\n")),
    ] {
        let failure = upstream_reply(status, ct, &error_body);
        let upstream = RetryUpstream::new(vec![failure, upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("custom-reasoning-{carrier}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = responses_request(port, body, "codex");
        assert_eq!(upstream.attempts(), 2, "{carrier} 完整的自定义工具历史应在首次同类错误后修复：{response}");
        assert!(response.contains(r#"{"ok":true}"#));
        let captured = upstream.bodies();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body), "{carrier} 首发剥离可省略 id");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[1]).unwrap(), expected, "{carrier} 只删除失效推理，完整保留两类工具、调用编号、输入与结果");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        assert_eq!(diagnostics.lines().filter(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "repair_applied").count(), 1);
        for sensitive in ["fixture-only-not-a-key", "fixture ciphertext", "fixture prompt", "fixture tool result"] {
            assert!(!diagnostics.contains(sensitive));
        }
    }
}

#[test]
fn 重试边界_codex原样或修复后等待及读流期间取消不重放() {
    let body = r#" {"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"}]} "#;
    let history = upstream_reply(400, "application/json", r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#);
    for (name, replies, sends) in [
        ("original-wait", vec![history.clone()], 1),
        ("repaired-wait", vec![history.clone(), history.clone(), upstream_reply(503, "application/json", r#"{"error":{"code":"server_error"}}"#)], 3),
    ] {
        let upstream = RetryUpstream::new(replies);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("codex-cancel-{name}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(client, "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nX-Relay-Client: codex\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while upstream.attempts() < sends {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(25));
        client.shutdown(std::net::Shutdown::Both).unwrap();
        drop(client);
        wait_retry_status(port, |s| s["activeRequests"] == 0);
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(upstream.attempts(), sends, "{name} 取消后不再访问下一通道");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&upstream.bodies()[0]).unwrap(), codex_stripped(body));
    }
    // 明确 Codex 正在读已交付的流时取消，也应断开自己持有的上游连接。
    use std::sync::mpsc;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        let header_end = loop {
            let count = stream.read(&mut buf).unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buf[..count]);
            if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") { break index + 4; }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let length = headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
        }).unwrap();
        while request.len() < header_end + length {
            let count = stream.read(&mut buf).unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buf[..count]);
        }
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&request[header_end..]).unwrap(), codex_stripped(body));
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture text\"}\n\n").unwrap();
        tx.send(()).unwrap();
        match stream.read(&mut buf) {
            Ok(0) => true,
            Err(e) => matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted),
            _ => false,
        }
    });
    let port = free_port();
    let mut fixture = Fixture::new("codex-cancel-reading-stream", &url, port);
    fixture.gateway.start().unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(client, "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nX-Relay-Client: codex\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    wait_retry_status(port, |s| s["requests"][0]["phase"] == "text_output");
    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(client);
    assert!(server.join().unwrap(), "取消须立即关闭隔离上游流，不等读取超时");
    wait_retry_status(port, |s| s["activeRequests"] == 0);
}

#[test]
fn 重试边界_codex空回不绕过门槛且修复版冻结状态字段() {
    let stale = upstream_reply(400, "application/json", r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#);
    let empty = upstream_reply(200, "text/event-stream", "");
    let created = upstream_reply(200, "text/event-stream", "data: {\"type\":\"response.created\"}\n\n");
    let success = upstream_reply(200, "application/json", r#"{"ok":true}"#);
    let body = r#" {"input":[{"type":"message","id":"msg_fixture","status":"completed","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"}]} "#;
    let expected = serde_json::json!({"input": [{"type": "message", "status": "completed", "role": "user", "content": "fixture prompt"}]});
    for (name, replies, sends, mode, expected_status) in [
        ("empty-only", vec![created.clone(), created, success.clone()], 3, "untouched", "ok"),
        ("empty-after-repair", vec![stale.clone(), stale.clone(), empty.clone(), empty, success.clone()], 4, "repaired", "503"),
        ("status-after-repair", vec![stale.clone(), stale, upstream_reply(400, "application/json", r#"{"error":{"type":"invalid_request_error","message":"Unknown parameter: 'input[0].status'."}}"#), success], 3, "repaired", "400"),
    ] {
        let upstream = RetryUpstream::new(replies);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("codex-frozen-body-{name}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = responses_request(port, body, "codex");
        assert_eq!(upstream.attempts(), sends, "{name} 不能追加形状或状态剥离：{response}");
        let captured = upstream.bodies();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body), "{name} 首发剥离可省略 id");
        if mode == "untouched" {
            assert!(captured.iter().all(|sent| {
                serde_json::from_slice::<serde_json::Value>(sent).unwrap() == codex_stripped(body)
            }), "{name} 空响应不触发 Codex 历史剥离");
        } else {
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[1]).unwrap(), expected);
            assert!(captured[1..].iter().all(|sent| sent == &captured[1]), "修复后冻结，不得再移 message id/type/status");
        }
        if expected_status == "ok" {
            assert!(response.contains(r#"{"ok":true}"#), "{name}: {response}");
        } else {
            assert!(response.starts_with(&format!("HTTP/1.1 {expected_status}")), "{name}: {response}");
            assert!(!response.contains(r#"{"ok":true}"#), "{name}: {response}");
        }
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = diagnostics.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let expected_repairs = if mode == "untouched" { 0 } else { 1 };
        assert_eq!(rows.iter().filter(|r| r["event"] == "repair_applied").count(), expected_repairs, "{name} 修复次数固定");
        assert!(!rows.iter().any(|r| r["event"] == "status_repair_applied"));
        assert_eq!(rows.iter().filter(|r| r["event"] == "session_compat_enabled").count(), expected_repairs,
            "只有确认的历史错误修复成功才登记会话，空回不能登记");
    }
    // 预算只改自建副本，首个历史错误后不足以支持下一次，不能延长原预算。
    let upstream = RetryUpstream::new(vec![upstream_reply(400, "application/json", r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#)]);
    let port = free_port();
    let mut fixture = Fixture::new("codex-short-history-budget", &upstream.url, port);
    let script = fixture.dir.join("gateway/relay-retry-proxy.mjs");
    let original = std::fs::read_to_string(&script).unwrap();
    std::fs::write(script, original.replacen('\n', "\nprocess.env.PROXY_TOTAL_BUDGET_MS = '1000';\n", 1)).unwrap();
    fixture.gateway.start().unwrap();
    let response = responses_request(port, body, "codex");
    assert!(response.starts_with("HTTP/1.1 503"));
    assert_eq!(upstream.attempts(), 1);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&upstream.bodies()[0]).unwrap(), codex_stripped(body));
    let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
    assert!(!diagnostics.lines().any(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "repair_applied"));
}

#[test]
fn 重试边界_codex网页搜索缺失内容字段或未知动作状态时原样重试() {
    let valid = serde_json::json!({"type": "web_search_call", "id": "ws_fixture", "status": "completed", "action": {"type": "search", "query": "fixture query"}});
    let mut cases = Vec::new();
    for field in ["status", "action", "type"] {
        let mut item = valid.clone();
        item.as_object_mut().unwrap().remove(field);
        cases.push((format!("missing-{field}"), item));
    }
    for (name, action) in [
        ("missing-action-type", serde_json::json!({"query": "fixture query"})),
        ("missing-search-query", serde_json::json!({"type": "search"})),
        ("missing-open-url", serde_json::json!({"type": "open_page"})),
        ("missing-find-pattern", serde_json::json!({"type": "find_in_page", "url": "https://example.invalid/fixture"})),
        ("missing-find-url", serde_json::json!({"type": "find_in_page", "pattern": "fixture"})),
        ("empty-search-query", serde_json::json!({"type": "search", "query": ""})),
        ("unknown-action", serde_json::json!({"type": "future_action", "query": "fixture query"})),
    ] {
        let mut item = valid.clone();
        item["action"] = action;
        cases.push((name.to_string(), item));
    }
    let mut item = valid;
    item["status"] = serde_json::json!("future_status");
    cases.push(("unknown-status".to_string(), item));
    let failure = upstream_reply(400, "application/json", r#"{"error":{"type":"invalid_request_error","message":"The requested item was created under a different Azure OpenAI resource"}}"#);
    let success = upstream_reply(200, "application/json", r#"{"ok":true}"#);
    let replies = cases.iter().flat_map(|_| [failure.clone(), failure.clone(), success.clone()]).collect();
    let upstream = RetryUpstream::new(replies);
    let port = free_port();
    let mut fixture = Fixture::new("codex-incomplete-web-search", &upstream.url, port);
    fixture.gateway.start().unwrap();
    for (index, (name, item)) in cases.into_iter().enumerate() {
        let obj = serde_json::json!({"input": [
            {"type": "message", "id": "msg_fixture", "role": "user", "content": "fixture prompt"},
            {"type": "reasoning", "id": "rs_fixture", "encrypted_content": "fixture ciphertext"}, item
        ]});
        let body = format!(" {obj} ");
        assert!(responses_request(port, &body, "codex").contains(r#"{"ok":true}"#), "{name} 安全检查拒绝仍可换通道成功");
        assert_eq!(upstream.attempts(), (index + 1) * 3);
        let captured = upstream.bodies();
        assert!(captured[index * 3..].iter().all(|sent| {
            serde_json::from_slice::<serde_json::Value>(sent).unwrap() == codex_stripped(&body)
        }), "{name} 不得补字段、转成正文或删除必需搜索字段");
    }
    let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
    let rows: Vec<serde_json::Value> = diagnostics.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(!rows.iter().any(|r| r["event"] == "repair_applied"), "网页搜索不安全时不得做有损修复");
    assert!(rows.iter()
        .filter(|r| r["event"] == "session_compat_applied")
        .all(|r| r["removedReasoning"] == 0), "只允许首发的无损 id 剥离");
}

#[test]
fn 重试边界_codex与其他客户端同会话新连接沿用兼容预清理() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"The requested item was created under a different Azure OpenAI resource"}}"#;
    let body = r#" {"model":"gpt-6.1-sol","input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"}]} "#;
    let cleaned = serde_json::json!({"model": "gpt-6.1-sol", "input": [{"type": "message", "role": "user", "content": "fixture prompt"}]});
    for (name, marker, codex) in [
        ("explicit-codex", ("X-Relay-Client", "codex"), true),
        ("ua-codex", ("User-Agent", "codex_cli_rs/0.100.0"), true),
        ("claude", ("X-Relay-Client", "claude"), false),
        ("zcode", ("X-Relay-Client", "zcode"), false),
        ("unknown", ("User-Agent", "fixture-unknown/1.0"), false),
    ] {
        let failure = upstream_reply(400, "application/json", error);
        let success = upstream_reply(200, "application/json", r#"{"ok":true}"#);
        let replies = vec![failure, success.clone(), success];
        let upstream = RetryUpstream::new(replies);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("codex-session-isolation-{name}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let headers = [marker, ("X-Relay-Session-Id", "fixture-shared-session")];
        assert!(responses_request_with_headers(port, body, &headers).contains(r#"{"ok":true}"#));
        let first_sends = 2;
        assert_eq!(upstream.attempts(), first_sends);
        // 每次 helper 都创建新的客户端 TCP 连接，但保持会话名与鉴权完全相同。
        assert!(responses_request_with_headers(port, body, &headers).contains(r#"{"ok":true}"#));
        assert_eq!(upstream.attempts(), first_sends + 1);
        let captured = upstream.bodies();
        let expected_first = if codex { codex_stripped(body) } else { serde_json::from_str(body).unwrap() };
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), expected_first, "{name} 首发仅 Codex 剥可省略 id");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[1]).unwrap(), cleaned, "{name} 首次跨资源错误后修复");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[first_sends]).unwrap(), cleaned, "{name} 后续请求首发前预清理");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = diagnostics.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let cached = rows.iter().filter(|r| r["event"] == "session_compat_applied").count();
        let enabled = rows.iter().filter(|r| r["event"] == "session_compat_enabled").count();
        // Codex 每次首发剥 id 各记一条，第二次再叠加预清理；其他客户端只有预清理一条。
        assert_eq!(cached, if codex { 3 } else { 1 }, "{name} 预清理诊断数");
        assert_eq!(enabled, 1, "{name} 跨资源修复登记会话");
        assert!(!diagnostics.contains("fixture-shared-session"));
        assert!(!diagnostics.contains("fixture-only-not-a-key"));
    }
}

#[test]
fn 重试边界_codex冲突标记不进入新阶段但保留既有修复行为() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"The requested item was created under a different Azure OpenAI resource"}}"#;
    let simple = r#" {"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","encrypted_content":"fixture ciphertext"}]} "#;
    let web = r#" {"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","encrypted_content":"fixture ciphertext"},{"type":"web_search_call","id":"ws_fixture","status":"completed","action":{"type":"search","query":"fixture query"}}]} "#;
    for (name, headers, legacy_codex) in [
        ("codex-claude", [("X-Relay-Client", "codex"), ("User-Agent", "claude-desktop/1.0")], true),
        ("codex-zcode", [("X-Relay-Client", "codex"), ("User-Agent", "zcode/1.0")], true),
        ("claude-codex", [("X-Relay-Client", "claude"), ("User-Agent", "codex_cli_rs/0.100.0")], false),
        ("ambiguous-ua", [("X-Relay-Client", "codex"), ("User-Agent", "codex_cli_rs/0.100.0 claude/1.0")], true),
        ("unknown-codex", [("X-Relay-Client", "unknown"), ("User-Agent", "codex_cli_rs/0.100.0")], true),
    ] {
        for (history, body) in [("simple", simple), ("web", web)] {
            let failure = upstream_reply(400, "application/json", error);
            let success = upstream_reply(200, "application/json", r#"{"ok":true}"#);
            let replies = if history == "simple" { vec![failure, success] }
                else { vec![failure.clone(), failure, success] };
            let upstream = RetryUpstream::new(replies);
            let port = free_port();
            let mut fixture = Fixture::new(&format!("codex-conflict-{name}-{history}"), &upstream.url, port);
            fixture.gateway.start().unwrap();
            let response = responses_request_with_headers(port, body, &headers);
            let captured = upstream.bodies();
            let expected_first: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(captured[0], body.as_bytes(), "冲突或未知标记的首发必须原样保留");
            if history == "simple" {
                assert_eq!(upstream.attempts(), 2, "{name} 保留既有即时修复行为");
                assert!(response.contains(r#"{"ok":true}"#));
                assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[1]).unwrap(), serde_json::json!({
                    "input": [{"type": "message", "role": "user", "content": "fixture prompt"}]
                }), "冲突标记允许旧分支即时删除可省略 id，不承诺完全禁止旧剥离");
            } else {
                assert_eq!(upstream.attempts(), if legacy_codex { 3 } else { 1 });
                assert!(captured.iter().all(|sent| {
                    serde_json::from_slice::<serde_json::Value>(sent).unwrap() == expected_first
                }), "{name} 不得进入新网页搜索安全门");
                let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
                assert!(!diagnostics.lines().any(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "repair_applied"));
            }
        }
    }
}

#[test]
fn 重试边界_codex失效推理修复后同会话首发预清理且不污染其他会话() {
    let body = r#"{"model":"fixture-model","input":[{"type":"message","id":"msg_first","role":"user","content":"first prompt"},{"type":"reasoning","id":"rs_first","encrypted_content":"first ciphertext"}]}"#;
    let unrelated = r#"{"model":"fixture-model","input":[{"type":"message","id":"msg_other","role":"user","content":"other prompt"},{"type":"reasoning","id":"rs_other","encrypted_content":"other ciphertext"}]}"#;
    let error = r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#;
    for explicit in [false, true] {
        let success = upstream_reply(200, "application/json", r#"{"ok":true}"#);
        let upstream = RetryUpstream::new(vec![upstream_reply(400, "application/json", error), success.clone(), success.clone(), success]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("stale-session-memory-{explicit}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let mut headers = vec![("X-Relay-Client", "codex")];
        if explicit { headers.push(("X-Relay-Session-Id", "fixture-first-session")); }
        assert!(responses_request_with_headers(port, body, &headers).contains(r#"{"ok":true}"#));
        assert_eq!(upstream.attempts(), 2, "首次失效推理错误即修复");
        assert!(responses_request_with_headers(port, body, &headers).contains(r#"{"ok":true}"#));
        assert_eq!(upstream.attempts(), 3, "同会话新连接不再先发送失效 reasoning");
        if explicit { headers[1].1 = "fixture-other-session"; }
        assert!(responses_request_with_headers(port, unrelated, &headers).contains(r#"{"ok":true}"#));
        let captured = upstream.bodies();
        let cleaned = serde_json::json!({"model":"fixture-model","input":[{"type":"message","role":"user","content":"first prompt"}]});
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body));
        for sent in &captured[1..3] {
            assert_eq!(serde_json::from_slice::<serde_json::Value>(sent).unwrap(), cleaned);
        }
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[3]).unwrap(), codex_stripped(unrelated), "未报错的其他会话必须保留 reasoning");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = diagnostics.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let registered: Vec<_> = rows.iter().filter(|r| r["event"] == "session_compat_enabled").collect();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0]["kind"], "stale_reasoning_item");
        assert_eq!(registered[0]["source"], if explicit { "explicit_session" } else { "resource_lineage" });
        assert!(!diagnostics.contains("first ciphertext"));
        assert!(!diagnostics.contains("fixture-first-session"));
    }
}

#[test]
fn 重试边界_codex限流与永久错误不触发二次历史降级() {
    let stale = upstream_reply(400, "application/json", r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#);
    let body = r#" {"input":[{"type":"message","id":"msg_fixture","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"}]} "#;
    let hinted_limit = r#"{"error":{"type":"invalid_request_error","code":"rate_limit_exceeded","message":"encrypted content could not be verified"}}"#;
    let upstream = RetryUpstream::new(vec![
        stale.clone(), upstream_reply(429, "application/json", hinted_limit),
        upstream_reply(200, "application/json", r#"{"ok":true}"#),
    ]);
    let port = free_port();
    let mut fixture = Fixture::new("codex-429-history-text-reset", &upstream.url, port);
    fixture.gateway.start().unwrap();
    let response = responses_request(port, body, "codex");
    assert_eq!(upstream.attempts(), 3, "带历史错误文案的429仍按限流处理，不再改写请求体：{response}");
    assert!(response.contains(r#"{"ok":true}"#));
    let repaired = serde_json::json!({"input": [{"type": "message", "role": "user", "content": "fixture prompt"}]});
    let captured = upstream.bodies();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body));
    assert!(captured[1..].iter().all(|sent| {
        serde_json::from_slice::<serde_json::Value>(sent).unwrap() == repaired
    }), "首次历史错误修复后，限流重试沿用同一修复版");
    let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
    assert_eq!(diagnostics.lines().filter(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "repair_applied").count(), 1);
    for (carrier, ct) in [("json", "application/json"), ("sse", "text/event-stream")] {
        for (code, expected_sends, final_status) in [
            ("rate_limit_exceeded", 3, 200),
            ("server_error", 3, 200),
            ("insufficient_quota", 2, 402),
            ("invalid_api_key", 2, 400),
        ] {
            let envelope = serde_json::json!({"error": {
                "type": "invalid_request_error", "code": code,
                "message": "encrypted content could not be verified"
            }}).to_string();
            let error_body = if carrier == "sse" { format!("data: {envelope}\n\n") } else { envelope };
            let upstream = RetryUpstream::new(vec![stale.clone(), upstream_reply(200, ct, &error_body), upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
            let port = free_port();
            let mut fixture = Fixture::new(&format!("codex-error-code-streak-{carrier}-{code}"), &upstream.url, port);
            fixture.gateway.start().unwrap();
            let response = responses_request(port, body, "codex");
            assert_eq!(upstream.attempts(), expected_sends, "{carrier}/{code} 明确错误码不能被历史文案覆盖：{response}");
            assert!(response.starts_with(&format!("HTTP/1.1 {final_status}")), "{carrier}/{code}: {response}");
            assert_eq!(response.contains(r#"{"ok":true}"#), expected_sends == 3);
            let captured = upstream.bodies();
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body), "{carrier}/{code} 首发只剥离可省略 id");
            assert!(captured[1..].iter().all(|sent| {
                serde_json::from_slice::<serde_json::Value>(sent).unwrap() == repaired
            }), "{carrier}/{code} 明确错误码不能触发再次历史改写");
            let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
            assert_eq!(diagnostics.lines().filter(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "repair_applied").count(), 1);
        }
    }
    for (name, interruption) in [
        ("different-kind", upstream_reply(400, "application/json", r#"{"error":{"type":"invalid_request_error","message":"The requested item was created under a different Azure OpenAI resource"}}"#)),
        ("429", upstream_reply(429, "application/json", r#"{"error":{"code":"rate_limit_exceeded"}}"#)),
        ("network-close", String::new()), // 假上游已收完整请求后直接断连，不碰系统网络。
        ("empty-json", upstream_reply(200, "application/json", r#"{"choices":[{"message":{"content":""}}]}"#)),
        ("empty-sse", upstream_reply(200, "text/event-stream", "")),
    ] {
        let upstream = RetryUpstream::new(vec![stale.clone(), interruption, stale.clone(), stale.clone(), upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("codex-streak-reset-{name}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = responses_request(port, body, "codex");
        assert_eq!(upstream.attempts(), 4, "{name} 修复后最多再发三次，不能继续第六次：{response}");
        assert!(response.starts_with("HTTP/1.1 503"), "{name} 上限耗尽返回可重试错误：{response}");
        let captured = upstream.bodies();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body), "{name} 首发剥离 id");
        assert!(captured[1..].iter().all(|sent| {
            serde_json::from_slice::<serde_json::Value>(sent).unwrap() == repaired
        }), "{name} 修复后不得再次改写");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        assert_eq!(diagnostics.lines().filter(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "repair_applied").count(), 1);
    }
}

#[test]
fn 重试边界_codex缺失或重复工具结果时拒绝历史改写() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#;
    for (name, tools) in [
        ("missing-output", serde_json::json!([
            {"type": "custom_tool_call", "id": "ct_fixture", "call_id": "call_fixture", "name": "fixture_tool", "input": "fixture input"}
        ])),
        ("duplicate-output", serde_json::json!([
            {"type": "custom_tool_call", "id": "ct_fixture", "call_id": "call_fixture", "name": "fixture_tool", "input": "fixture input"},
            {"type": "custom_tool_call_output", "call_id": "call_fixture", "output": "fixture first result"},
            {"type": "custom_tool_call_output", "call_id": "call_fixture", "output": "fixture duplicate result"}
        ])),
        ("compaction", serde_json::json!([{"type": "compaction", "encrypted_content": "fixture opaque history"}])),
        ("item-reference", serde_json::json!([{"type": "item_reference", "id": "msg_remote"}])),
        ("nonreasoning-ciphertext", serde_json::json!([{"type": "message", "role": "assistant", "content": "fixture history", "encrypted_content": "fixture opaque history"}])),
        ("orphan-output", serde_json::json!([{"type": "custom_tool_call_output", "call_id": "missing_call", "output": "fixture result"}])),
        ("wrong-tool-kind", serde_json::json!([
            {"type": "function_call", "call_id": "call_fixture", "name": "fixture_function", "arguments": "{}"},
            {"type": "custom_tool_call_output", "call_id": "call_fixture", "output": "fixture result"}
        ])),
    ] {
        let body = format!(" {{\"model\":\"gpt-6.1-sol\",\"input\":[{{\"type\":\"message\",\"id\":\"msg_fixture\",\"role\":\"user\",\"content\":\"fixture prompt\"}},{{\"type\":\"reasoning\",\"id\":\"rs_fixture\",\"encrypted_content\":\"fixture ciphertext\"}},{}]}} ",
            tools.as_array().unwrap().iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join(","));
        let failure = upstream_reply(400, "application/json", error);
        let upstream = RetryUpstream::new(vec![failure.clone(), failure, upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("codex-incomplete-tools-{name}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = responses_request(port, &body, "codex");
        assert_eq!(upstream.attempts(), 3, "{name} 不安全历史仍可有限原样重试：{response}");
        assert!(response.contains(r#"{"ok":true}"#));
        assert!(upstream.bodies().iter().all(|sent| {
            serde_json::from_slice::<serde_json::Value>(sent).unwrap() == codex_stripped(&body)
        }), "{name} 工具结果缺失或重复时，不得删除推理或任何其他字段");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = diagnostics.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(!rows.iter().any(|r| r["event"] == "repair_applied"), "{name} 不得做有损历史改写");
        assert!(rows.iter()
            .filter(|r| r["event"] == "session_compat_applied")
            .all(|r| r["removedReasoning"] == 0), "{name} 只允许首发的无损 id 剥离");
        for sensitive in ["fixture-only-not-a-key", "fixture ciphertext", "fixture input", "fixture prompt", "fixture first result", "fixture duplicate result"] {
            assert!(!diagnostics.contains(sensitive));
        }
    }
}

#[test]
fn 重试边界_失效推理无法安全修复时保留完整历史换通道() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"Item with id 'rs_fixture' not found. Items are not persisted when store is set to false."}}"#;
    for (history, body) in [
        ("opaque", r#" {"model":"gpt-6.1-sol","store":false,"input":[{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture reasoning"},{"type":"compaction","id":"cmp_fixture","encrypted_content":"fixture opaque history"},{"role":"user","content":"fixture prompt"}]} "#),
        ("reference", r#" {"model":"gpt-6.1-sol","previous_response_id":"resp_fixture","input":[{"type":"item_reference","id":"rs_fixture"},{"role":"user","content":"fixture prompt"}]} "#),
        ("orphan-tool-output", r#" {"model":"gpt-6.1-sol","input":[{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture reasoning"},{"type":"custom_tool_call_output","call_id":"missing_call","output":"fixture tool result"},{"role":"user","content":"fixture prompt"}]} "#),
        ("wrong-tool-kind", r#" {"model":"gpt-6.1-sol","input":[{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture reasoning"},{"type":"function_call","call_id":"call_fixture","name":"fixture_function","arguments":"{}"},{"type":"custom_tool_call_output","call_id":"call_fixture","output":"fixture tool result"},{"role":"user","content":"fixture prompt"}]} "#),
    ] {
        for (carrier, status, ct, error_body) in [
            ("http", 400, "application/json", error.to_string()),
            ("json", 200, "application/json", error.to_string()),
            ("sse", 200, "text/event-stream", format!("data: {error}\n\n")),
        ] {
            let upstream = RetryUpstream::new(vec![upstream_reply(status, ct, &error_body), upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
            let port = free_port();
            let mut fixture = Fixture::new(&format!("stale-retain-{history}-{carrier}"), &upstream.url, port);
            fixture.gateway.start().unwrap();
            let response = responses_request(port, body, "codex");
            assert_eq!(upstream.attempts(), 2, "{history}/{carrier} 不安全改写不等于下一条通道也无法恢复：{response}");
            assert!(response.contains(r#"{"ok":true}"#));
            assert!(upstream.bodies().iter().all(|sent| {
                serde_json::from_slice::<serde_json::Value>(sent).unwrap() == codex_stripped(body)
            }), "{history}/{carrier} 未识别项、密文、引用必须保留，只允许剥离可省略 id");
            let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
            let rows: Vec<serde_json::Value> = diagnostics.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            assert!(rows.iter().any(|r| r["event"] == "codex_resource_retry" && r["kind"] == "stale_reasoning_item"));
            assert!(!rows.iter().any(|r| r["event"] == "repair_applied"), "{history}/{carrier} 不安全历史不得做有损修复");
            assert!(rows.iter()
                .filter(|r| r["event"] == "session_compat_applied")
                .all(|r| r["removedReasoning"] == 0), "{history}/{carrier} 只允许首发的无损 id 剥离");
            for sensitive in ["fixture-only-not-a-key", "fixture reasoning", "fixture opaque history", "fixture prompt"] {
                assert!(!diagnostics.contains(sensitive));
            }
        }
    }
}

#[test]
fn 重试边界_退避期间取消后不再访问上游() {
    let upstream = RetryUpstream::new(vec![
        upstream_reply(503, "application/json", r#"{"error":{"type":"server_error","message":"fixture temporary outage"}}"#),
        upstream_reply(200, "application/json", r#"{"ok":true}"#),
    ]);
    let port = free_port();
    let mut fixture = Fixture::new("cancel-backoff", &upstream.url, port);
    fixture.gateway.start().unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(client, "GET /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer fixture-only-not-a-key\r\n\r\n").unwrap();
    // HTTP 错误响应已读到，第一轮尚未退出；25ms 远小于既有首次退避最短 500ms。
    wait_retry_status(port, |s| s["requests"][0]["phase"] == "reading_body");
    std::thread::sleep(Duration::from_millis(25));
    assert_eq!(upstream.attempts(), 1);
    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(client);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(retry_status(port)["activeRequests"], 0, "取消后应立即释放请求，不继续占着退避计时器");
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(upstream.attempts(), 1, "客户端已取消，不能再发起上游请求");
    wait_retry_status(port, |s| s["activeRequests"] == 0);
}

#[test]
fn 重试边界_失效推理保留历史重试受上限约束() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be decrypted"}}"#;
    for (history, body, repairable) in [
        ("opaque", r#"{"model":"gpt-6.1-sol","input":[{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture reasoning"},{"type":"compaction","encrypted_content":"fixture opaque history"},{"role":"user","content":"fixture"}]}"#, false),
        ("inline", r#" {"model":"gpt-6.1-sol","input":[{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture reasoning"},{"type":"message","id":"msg_fixture","role":"user","content":"fixture"}]} "#, true),
    ] {
        for (carrier, status, ct, error_body) in [
            ("http", 400, "application/json", error.to_string()),
            ("json", 200, "application/json", error.to_string()),
            ("sse", 200, "text/event-stream", format!("data: {error}\n\n")),
        ] {
            let upstream = RetryUpstream::new(vec![upstream_reply(status, ct, &error_body)]);
            let port = free_port();
            let mut fixture = Fixture::new(&format!("stale-limit-{history}-{carrier}"), &upstream.url, port);
            fixture.gateway.start().unwrap();
            let response = responses_request(port, body, "codex");
            let attempts = if repairable { 4 } else { 5 };
            assert_eq!(upstream.attempts(), attempts, "{history}/{carrier} 可修复历史受三次重发上限，不可修复历史受五次资源上限");
            if repairable {
                assert!(response.starts_with("HTTP/1.1 503"), "{history}/{carrier} 修复版三次发送耗尽：{response}");
            } else {
                assert!(response.starts_with("HTTP/1.1 400"));
                assert!(response.contains("stopping after 5 resource attempts"));
            }
            let captured = upstream.bodies();
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[0]).unwrap(), codex_stripped(body));
            if repairable {
                assert_eq!(serde_json::from_slice::<serde_json::Value>(&captured[1]).unwrap(), serde_json::json!({
                    "model": "gpt-6.1-sol", "input": [{"type": "message", "role": "user", "content": "fixture"}]
                }));
                assert!(captured[1..].iter().all(|sent| sent == &captured[1]), "首次修复后保持同一修复版");
            } else {
                assert!(captured.iter().all(|sent| {
                    serde_json::from_slice::<serde_json::Value>(sent).unwrap() == codex_stripped(body)
                }));
            }
            let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
            let rows: Vec<serde_json::Value> = diagnostics.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            let expected_failures = if repairable { 4 } else { 5 };
            assert!(rows.iter().any(|r| r["event"] == "codex_resource_exhausted" && r["kind"] == "stale_reasoning_item" && r["resourceFailures"] == expected_failures && r["refusal"] == "resource_attempt_limit"));
            assert_eq!(rows.iter().filter(|r| r["event"] == "repair_applied").count(), usize::from(repairable));
            if !repairable {
                assert_eq!(rows.iter().filter(|r| r["event"] == "repair_refused" && r["refusal"] == "unsafe_inline_history").count(), 1, "安全检查拒绝后当次不反复尝试改写");
            }
            assert!(!diagnostics.contains("fixture reasoning"));
        }
    }
}

#[test]
fn 重试边界_失效推理换通道等待期间取消不再发请求() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#;
    let body = r#"{"input":[{"type":"compaction","encrypted_content":"fixture opaque history"},{"role":"user","content":"fixture"}]}"#;
    let upstream = RetryUpstream::new(vec![upstream_reply(400, "application/json", error)]);
    let port = free_port();
    let mut fixture = Fixture::new("stale-cancel", &upstream.url, port);
    fixture.gateway.start().unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(client, "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nX-Relay-Client: codex\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while upstream.attempts() == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(25));
    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(client);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(retry_status(port)["activeRequests"], 0);
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(upstream.attempts(), 1, "取消后不允许再试下一条通道");
}

#[test]
fn 重试边界_失效推理保留历史重试只对启用兼容的codex请求生效() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#;
    let body = r#"{"input":[{"type":"compaction","encrypted_content":"fixture opaque history"},{"role":"user","content":"fixture"}]}"#;
    for (name, client, disabled) in [("zcode", "zcode", false), ("disabled", "codex", true)] {
        let upstream = RetryUpstream::new(vec![upstream_reply(400, "application/json", error)]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("stale-scope-{name}"), &upstream.url, port);
        if disabled {
            // 只改自建临时副本；不写全局环境变量，不影响其他测试或现用实例。
            let script = fixture.dir.join("gateway/relay-retry-proxy.mjs");
            let original = std::fs::read_to_string(&script).unwrap();
            // 保留首行 shebang，环境覆盖在模块读取配置之前执行。
            std::fs::write(script, original.replacen('\n', "\nprocess.env.PROXY_CODEX_RESOURCE_RETRY = '0';\n", 1)).unwrap();
        }
        fixture.gateway.start().unwrap();
        let response = responses_request(port, body, client);
        assert!(response.starts_with("HTTP/1.1 400"));
        assert_eq!(upstream.attempts(), 1);
        assert_eq!(upstream.bodies()[0], body.as_bytes());
    }
}


#[test]
fn 重试边界_确定性错误在流式与普通响应中都不重试() {
    let error = r#"{"error":{"code":"context_length_exceeded","message":"maximum context length exceeded"}}"#;
    for (name, status, content_type, body) in [
        ("http", 400, "application/json", error.to_string()),
        ("sse", 200, "text/event-stream", format!("data: {error}\n\n")),
    ] {
        let upstream = RetryUpstream::new(vec![
            upstream_reply(status, content_type, &body),
            upstream_reply(200, "application/json", r#"{"ok":true}"#),
        ]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("permanent-{name}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = request(port, "/v1/chat/completions");
        assert_eq!(upstream.attempts(), 1, "{name} 确定性错误不能重复发送");
        assert!(response.starts_with("HTTP/1.1 400"), "{name}: {response}");
        assert!(!response.contains(r#"{"ok":true}"#));
        if name == "sse" {
            let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
            let failure: serde_json::Value = diagnostics.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()).find(|r| r["event"] == "precommit_failure").unwrap();
            assert_eq!(failure["retryable"], false);
            assert!(!diagnostics.contains("maximum context length exceeded"));
        }
    }
}


#[test]
fn 重试边界_确定性错误码与文案保持一致() {
    let errors = [
        ("context", r#"{"error":{"code":"context_length_exceeded","message":"provider error"}}"#, 400),
        ("auth", r#"{"error":{"code":"invalid_api_key"}}"#, 400),
        ("quota", r#"{"error":{"code":"insufficient_quota"}}"#, 402),
        ("model", r#"{"error":{"type":"invalid_request_error","message":"model not found"}}"#, 400),
        ("policy", r#"{"error":{"message":"content filter blocked the request"}}"#, 400),
    ];
    for (name, error, expected) in errors {
        for (carrier, ct, body) in [
            ("sse", "text/event-stream", format!("data: {error}\n\n")),
            ("json", "application/json", error.to_string()),
        ] {
            let upstream = RetryUpstream::new(vec![upstream_reply(200, ct, &body), upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
            let port = free_port();
            let mut fixture = Fixture::new(&format!("codes-{name}-{carrier}"), &upstream.url, port);
            fixture.gateway.start().unwrap();
            let response = request(port, "/v1/chat/completions");
            assert_eq!(upstream.attempts(), 1, "{name}/{carrier} 不应重试");
            assert!(response.starts_with(&format!("HTTP/1.1 {expected}")), "{name}/{carrier}: {response}");
        }
    }
}

#[test]
fn 重试边界_临时错误与部署缺失仍可恢复_未知流错误保留原策略() {
    for (name, status, ct, body) in [
        ("http-429", 429, "application/json", r#"{"error":{"code":"rate_limit_exceeded","message":"try again"}}"#.to_string()),
        ("http-503", 503, "application/json", r#"{"error":{"message":"temporary outage"}}"#.to_string()),
        ("sse-rate", 200, "text/event-stream", "data: {\"error\":{\"code\":\"rate_limit_exceeded\"}}\n\n".to_string()),
        ("sse-unknown", 200, "text/event-stream", "data: {\"error\":{\"code\":\"future_provider_error\",\"message\":\"unclassified fixture\"}}\n\n".to_string()),
        ("sse-generic", 200, "text/event-stream", "data: {\"error\":{\"type\":\"invalid_request_error\",\"message\":\"provider error\"}}\n\n".to_string()),
        ("sse-wrapped-rate", 200, "text/event-stream", "data: {\"error\":{\"type\":\"invalid_request_error\",\"message\":\"rate limit exceeded\"}}\n\n".to_string()),
        ("sse-deployment", 200, "text/event-stream", "data: {\"error\":{\"type\":\"invalid_request_error\",\"message\":\"no deployment to match this model\"}}\n\n".to_string()),
        ("json-deployment", 200, "application/json", r#"{"error":{"type":"invalid_request_error","message":"no deployment to match this model"}}"#.to_string()),
        ("http-deployment", 400, "application/json", r#"{"error":{"type":"invalid_request_error","message":"no deployment to match this model"}}"#.to_string()),
        ("json-temporary", 200, "application/json", r#"{"error":{"code":"server_error","message":"temporary outage"}}"#.to_string()),
    ] {
        let upstream = RetryUpstream::new(vec![upstream_reply(status, ct, &body), upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
        let port = free_port();
        let mut fixture = Fixture::new(name, &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = request(port, "/v1/chat/completions");
        assert_eq!(upstream.attempts(), 2, "{name} 应保留可恢复重试");
        assert!(response.contains(r#"{"ok":true}"#), "{name}: {response}");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        assert!(!diagnostics.contains("fixture-only-not-a-key"));
        assert!(!diagnostics.contains("unclassified fixture"));
    }
}

#[test]
fn 重试边界_错误编号与无关字段不改变暂时错误分类() {
    for (name, status, ct, body) in [
        ("http-id", 400, "application/json", r#"{"error":{"code":"server_error","message":"provider error [trace_id=balance_502_unauthorized]"},"debug":"invalid_api_key insufficient_quota"}"#.to_string()),
        ("json-id", 200, "application/json", r#"{"error":{"code":"server_error","message":"provider error [trace_id=balance_502_unauthorized]"},"debug":"invalid_api_key insufficient_quota"}"#.to_string()),
        ("sse-id", 200, "text/event-stream", "data: {\"error\":{\"code\":\"server_error\",\"message\":\"provider error [trace_id=balance_502_unauthorized]\"},\"debug\":\"invalid_api_key insufficient_quota\"}\n\n".to_string()),
        ("http-unrelated", 503, "application/json", r#"{"error":{"type":"invalid_request_error","message":"provider error"},"debug":"invalid_api_key insufficient_quota"}"#.to_string()),
    ] {
        let upstream = RetryUpstream::new(vec![
            upstream_reply(status, ct, &body),
            upstream_reply(200, "application/json", r#"{"ok":true}"#),
        ]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("classify-{name}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = request(port, "/v1/chat/completions");
        assert_eq!(upstream.attempts(), 2, "{name} 应按真正错误恢复");
        assert!(response.contains(r#"{"ok":true}"#), "{name}: {response}");
        let bodies = upstream.bodies();
        assert_eq!(bodies[0], bodies[1], "编号与调试字段不能触发改写");
    }
}

#[test]
fn 重试边界_加密历史错误的请求编号不能阻止恢复() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"The encrypted content for item rs_gateway_balance_502 could not be verified. Reason: Encrypted content could not be decrypted or parsed. (request id: 2026102203531550247527f9nrqKAP) [trace_id=503]"}}"#;
    let upstream = RetryUpstream::new(vec![
        upstream_reply(400, "application/json", error),
        upstream_reply(200, "application/json", r#"{"ok":true}"#),
    ]);
    let port = free_port();
    let mut fixture = Fixture::new("classify-encrypted-history", &upstream.url, port);
    fixture.gateway.start().unwrap();
    let body = r#"{"input":[{"type":"message","role":"user","content":"fixture prompt"},{"type":"reasoning","id":"rs_fixture","encrypted_content":"fixture ciphertext"}]}"#;
    let response = responses_request(port, body, "claude");
    assert!(response.contains(r#"{"ok":true}"#), "{response}");
    assert_eq!(upstream.attempts(), 2);
    let bodies = upstream.bodies();
    assert_eq!(bodies[0], body.as_bytes());
    let repaired: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap();
    assert_eq!(repaired, serde_json::json!({"input":[{"type":"message","role":"user","content":"fixture prompt"}]}));
    let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
    assert!(!diagnostics.contains("rs_gateway_balance_502"));
    assert!(!diagnostics.contains("fixture ciphertext"));
}

#[test]
fn 重试边界_内容下发后不重新生成_成功流不改写内容() {
    for (name, ending) in [
        ("complete", "data: [DONE]\n\n"),
        ("failed", "data: {\"type\":\"error\",\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"maximum context length exceeded\"}}\n\n"),
        ("stale", "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"encrypted content could not be verified\"}}\n\n"),
    ] {
        let body = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"fixture text\"}}}}]}}\n\n{ending}");
        let upstream = RetryUpstream::new(vec![upstream_reply(200, "text/event-stream", &body)]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("committed-{name}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let response = request(port, "/v1/chat/completions");
        assert!(response.contains(&body), "{name} 必须保留流内容和终止事件");
        assert_eq!(upstream.attempts(), 1, "已下发内容后不得重放整个请求");
    }
    let ending = r#"{"type":"error","error":{"type":"invalid_request_error","message":"encrypted content could not be verified"}}"#;
    let stream_body = format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"fixture text\"}}\n\ndata: {ending}\n\n");
    let upstream = RetryUpstream::new(vec![upstream_reply(200, "text/event-stream", &stream_body)]);
    let port = free_port();
    let mut fixture = Fixture::new("codex-committed-history-error", &upstream.url, port);
    fixture.gateway.start().unwrap();
    let body = r#" {"input":[{"type":"message","role":"user","content":"fixture prompt"},{"type":"reasoning","encrypted_content":"fixture ciphertext"}]} "#;
    let response = responses_request(port, body, "codex");
    assert!(response.contains(&stream_body), "Codex内容交付后的历史错误必须随原流传递");
    assert_eq!(upstream.attempts(), 1, "新增Codex分支不能重放已交付内容");
    assert_eq!(upstream.bodies()[0], body.as_bytes());
}


#[test]
fn 重试边界_流式退避与快速换通道等待期间均可取消() {
    for (name, error) in [
        ("sse-cancel", r#"{"error":{"code":"rate_limit_exceeded"}}"#),
        ("deployment-cancel", r#"{"error":{"message":"no deployment to match"}}"#),
    ] {
        let upstream = RetryUpstream::new(vec![
            upstream_reply(200, "text/event-stream", &format!("data: {error}\n\n")),
            upstream_reply(200, "application/json", r#"{"ok":true}"#),
        ]);
        let port = free_port();
        let mut fixture = Fixture::new(name, &upstream.url, port);
        fixture.gateway.start().unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(client, "GET /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        wait_retry_status(port, |s| s["requests"][0]["phase"] == "waiting_first_content");
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(upstream.attempts(), 1);
        client.shutdown(std::net::Shutdown::Both).unwrap();
        drop(client);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(retry_status(port)["activeRequests"], 0, "{name} 取消后立即释放在途请求");
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(upstream.attempts(), 1, "{name} 取消后不得建立新尝试");
    }
}

#[test]
fn 重试边界_等待响应头及流式读取期间取消会中断当前上游() {
    for streaming in [false, true] {
        use std::sync::mpsc;
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", upstream.local_addr().unwrap().port());
        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 8192];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            if streaming {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"fixture\"}}]}\n\n").unwrap();
            }
            tx.send(()).unwrap();
            match stream.read(&mut buf) {
                Ok(0) => true,
                Err(e) => matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted),
                _ => false,
            }
        });
        let port = free_port();
        let mut fixture = Fixture::new(&format!("cancel-inflight-{streaming}"), &url, port);
        fixture.gateway.start().unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(client, "GET /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        wait_retry_status(port, |s| s["requests"][0]["phase"] == if streaming { "text_output" } else { "waiting_headers" });
        client.shutdown(std::net::Shutdown::Both).unwrap();
        drop(client);
        assert!(server.join().unwrap(), "取消必须断开当前上游，不等待读取超时");
        wait_retry_status(port, |s| s["activeRequests"] == 0);
    }
}


#[test]
fn 重试边界_精确匹配的状态兼容修复优先于确定性分类() {
    let error = r#"{"error":{"type":"invalid_request_error","message":"Unknown parameter: 'input[0].status'."}}"#;
    for (carrier, status, ct, error_body) in [
        ("http", 400, "application/json", error.to_string()),
        ("json", 200, "application/json", error.to_string()),
        ("sse", 200, "text/event-stream", format!("data: {error}\n\n")),
    ] {
        let upstream = RetryUpstream::new(vec![upstream_reply(status, ct, &error_body), upstream_reply(200, "application/json", r#"{"ok":true}"#)]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("status-repair-{carrier}"), &upstream.url, port);
        fixture.gateway.start().unwrap();
        let body = r#"{"input":[{"type":"message","role":"user","content":"fixture text","status":"completed"}]}"#;
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(client, "POST /v1/responses HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert_eq!(upstream.attempts(), 2, "{carrier} 应先尝试已有的安全兼容修复");
        assert!(response.contains(r#"{"ok":true}"#), "{carrier}: {response}");
        let diagnostics = std::fs::read_to_string(fixture.dir.join("logs/diagnostics.jsonl")).unwrap();
        assert!(diagnostics.contains("status_repair_applied"));
        assert!(!diagnostics.contains("fixture text"));
    }
}

/// 手动性能观察：只发成功请求，基线脚本必须显式提供，绝不连接现用实例。
#[test]
#[ignore = "手动隔离性能对照，不属于默认回归"]
fn 速度观察_成功转发基线对照() {
    let baseline = std::fs::read_to_string(std::env::var_os("AG_RELAY_BENCH_BASELINE").expect("提供基线脚本路径")).unwrap();
    let output = "x".repeat(65536);
    let body = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{output}\"}}}}]}}\n\ndata: [DONE]\n\n");
    for (round, use_baseline) in [true, false, false, true].into_iter().enumerate() {
        let upstream = RetryUpstream::new(vec![upstream_reply(200, "text/event-stream", &body)]);
        let port = free_port();
        let mut fixture = Fixture::new(&format!("speed-{round}"), &upstream.url, port);
        if use_baseline { std::fs::write(fixture.dir.join("gateway/relay-retry-proxy.mjs"), &baseline).unwrap(); }
        fixture.gateway.start().unwrap();
        let mut samples = Vec::new();
        for n in 0..45 {
            let start = std::time::Instant::now();
            let response = request(port, "/v1/chat/completions");
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert!(response.contains(&body));
            if n >= 5 { samples.push(elapsed); }
        }
        samples.sort_by(f64::total_cmp);
        println!("{} round={} n=40 median_ms={:.3} p95_ms={:.3}", if use_baseline { "baseline" } else { "candidate" }, round + 1, samples[20], samples[38]);
        assert_eq!(upstream.attempts(), 45, "成功请求不得产生额外上游尝试");
    }
}
