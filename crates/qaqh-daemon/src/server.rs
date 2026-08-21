use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use qaqh_proto::{CONTROL_PROTOCOL_VERSION, DaemonDiscovery};
use qaqh_runtime::QaqhService;
use qaqh_runtime::RingingHub;
use qaqh_runtime::{WorkspaceMode, WorkspaceSupervisor};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

/// 并发 TCP 连接上限：1 个客户端常态占 5+ 连接（open/renew 短连 +
/// 3 通道 SSE + timeline SSE 长连）。曾因 32 过紧 + 壳 rebuild 风暴
/// 打满后静默 drop 新连接，导致 lease 无法续期、SSE 全断死循环。
const MAX_CONNECTIONS: usize = 128;

fn daemon_channel() -> String {
    std::env::var("QAQH_CHANNEL").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "dev".into()
        } else {
            "stable".into()
        }
    })
}

/// `qaqh-daemon server` 的网络配置（临时跨端模式，不做任何安全加固）。
#[derive(Debug, Clone)]
pub struct ServerNetworkConfig {
    /// 监听 IP；`0.0.0.0` = 局域网可访问。
    pub bind_ip: std::net::IpAddr,
    /// 固定端口（远端客户端需要可预测的地址）。
    pub port: u16,
    /// Bearer token；缺省时随机生成并打印到 stderr。
    pub token: Option<String>,
}

impl Default for ServerNetworkConfig {
    fn default() -> Self {
        Self {
            bind_ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 0,
            token: None,
        }
    }
}

impl ServerNetworkConfig {
    /// `server --bind <ip> --port <port> --token <token>`；token 也接受
    /// `QAQH_SERVER_TOKEN` 环境变量（避免出现在进程命令行里）。
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut config = Self {
            bind_ip: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            port: 64413,
            token: std::env::var("QAQH_SERVER_TOKEN")
                .ok()
                .filter(|v| !v.is_empty()),
        };
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--bind" => {
                    index += 1;
                    let value = args.get(index).ok_or("--bind requires a value")?;
                    config.bind_ip = value
                        .parse()
                        .map_err(|_| format!("invalid --bind ip: {value}"))?;
                }
                "--port" => {
                    index += 1;
                    let value = args.get(index).ok_or("--port requires a value")?;
                    config.port = value
                        .parse()
                        .map_err(|_| format!("invalid --port: {value}"))?;
                }
                "--token" => {
                    index += 1;
                    let value = args.get(index).ok_or("--token requires a value")?;
                    config.token = Some(value.clone());
                }
                other => return Err(format!("unknown server flag: {other}")),
            }
            index += 1;
        }
        Ok(config)
    }
}

/// 尽力猜测本机局域网 IP：UDP connect 不真正发包，只让内核选出口网卡。
/// 连的是 TEST-NET 保留地址，不会产生网络流量。
fn guess_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket
        .connect((std::net::Ipv4Addr::new(192, 0, 2, 1), 9))
        .ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

pub async fn run() -> Result<(), String> {
    run_with(ServerNetworkConfig::default()).await
}

pub async fn run_with(config: ServerNetworkConfig) -> Result<(), String> {
    let data_root = qaqh_types::platform::ensure_data_root().map_err(stringify)?;
    let _lock = acquire_single_instance()?;
    let token = config.token.clone().unwrap_or_else(random_hex);
    if config.token.is_none() && !config.bind_ip.is_loopback() {
        // 临时跨端模式：没显式给 key 时把生成值打出来，方便手动填写。
        eprintln!("[qaqh-daemon] generated server token: {token}");
    }
    let epoch = random_hex();
    let listener = TcpListener::bind((config.bind_ip, config.port))
        .await
        .map_err(stringify)?;
    let address = listener.local_addr().map_err(stringify)?;
    // 0.0.0.0 不可被远端直连：discovery 与 display_host 换成可路由的局域网 IP。
    let advertise_ip = if config.bind_ip.is_unspecified() {
        guess_lan_ip().unwrap_or(config.bind_ip)
    } else {
        config.bind_ip
    };
    let discovery = DaemonDiscovery {
        endpoint: format!("ws://{advertise_ip}:{}/control/v1", address.port()),
        token: token.clone(),
        pid: std::process::id(),
        server_epoch: epoch.clone(),
        protocol_version: CONTROL_PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        build_id: env!("QAQH_BUILD_ID").into(),
        channel: daemon_channel(),
        executable: std::env::current_exe()
            .ok()
            .and_then(|path| path.canonicalize().ok().or(Some(path)))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    if !config.bind_ip.is_loopback() {
        log::warn!(
            "[qaqh-daemon] lan server mode on {advertise_ip}:{} — temporary build, no transport security",
            address.port()
        );
    }
    let hub = Arc::new(RingingHub::with_persistence(
        epoch.clone(),
        data_root.join("ringing"),
    ));
    let service = QaqhService::init();
    service.attach_ringing(hub.clone());
    // 宿主直连：`spawn_subagent` 工具此后经进程内宿主句柄运行，不再回连
    // daemon HTTP/SSE（Knife-1 step-2 收尾）。service 已含 registry 与 hub。
    qaqh_subagent::install_host(Arc::new(service.clone()));
    // 工具套件运行环境：config `[workspace] mode`（local 默认 / wsl 可选）。
    // 拉起失败不阻塞 daemon——worker 回退进程内工具执行。
    let workspace_mode = qaqh_config::Config::load()
        .map(|config| WorkspaceMode::parse(&config.workspace.mode))
        .unwrap_or(WorkspaceMode::Local);
    let workspace_service = service.clone();
    let workspace = WorkspaceSupervisor::start(
        workspace_mode,
        Arc::new(move |connection| {
            // 新 endpoint/token 仅经 daemon 内存注入后续 worker，绝不进日志或 IPC。
            workspace_service.attach_workspace(
                connection.endpoint.clone(),
                connection.token,
                workspace_mode.label(),
            );
            workspace_service.attach_workspace_state(qaqh_runtime::WorkspaceRuntimeState {
                configured_mode: workspace_mode.label().into(),
                active_mode: connection.mode.label().into(),
                endpoint: connection.endpoint,
                generation: connection.generation,
            });
        }),
    )
    .ok();
    if let Some(ref ws) = workspace {
        let connection = ws.connection();
        service.attach_workspace(
            connection.endpoint.clone(),
            connection.token,
            workspace_mode.label(),
        );
        service.attach_workspace_state(qaqh_runtime::WorkspaceRuntimeState {
            configured_mode: workspace_mode.label().into(),
            active_mode: connection.mode.label().into(),
            endpoint: connection.endpoint,
            generation: connection.generation,
        });
    } else {
        service.attach_workspace_state(qaqh_runtime::WorkspaceRuntimeState {
            configured_mode: workspace_mode.label().into(),
            active_mode: "disabled".into(),
            endpoint: String::new(),
            generation: 0,
        });
    }
    let ringing_leases = Arc::new(Mutex::new(crate::ringing_http::RingingLeaseStore::new()));
    let pending_commands = Arc::new(Mutex::new(
        crate::ringing_http::PendingCommandStore::new_persistent(),
    ));
    // Fold causally-linked business terminal events into persistent command
    // receipts. One observer per physical channel preserves channel isolation.
    for channel in [
        qaqh_domain::RingingChannel::Control,
        qaqh_domain::RingingChannel::Conversation,
        qaqh_domain::RingingChannel::Tool,
    ] {
        let mut receiver = hub.subscribe(channel);
        let receipts = pending_commands.clone();
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(envelope) => receipts
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .observe_terminal_event(&envelope),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        log::warn!(
                            "[ringing] command receipt observer lagged on {} by {} events",
                            channel.as_str(),
                            skipped
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    // F4: worker reader 线程 panic/崩溃时，registry 会把死实例标记为可重生；
    // 此周期任务负责真正重新拉起，避免单条事件流故障永久饿死会话。
    {
        let service = service.clone();
        let mut shutdown_rx = shutdown.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                tokio::select! {
                    _ = interval.tick() => service.respawn_dead_agents(),
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }
    // 发布 discovery：HTTP accept 循环即将开始（listener 早已 bind，但
    // 端口在 service 初始化完成前不可服务）。延迟发布避免客户端拿到
    // "已写 daemon.json 但 HTTP 未就绪"的假端口而导航失败（白屏/错误页），
    // 也让 ensure_daemon_running 的轮询与真实就绪时刻对齐。
    write_discovery(&discovery)?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let Ok(permit) = connections.clone().try_acquire_owned() else {
                    log::warn!(
                        "[qaqh-daemon] connection rejected: {} concurrent connections (max {MAX_CONNECTIONS}); client should back off",
                        MAX_CONNECTIONS - connections.available_permits()
                    );
                    drop(stream);
                    continue;
                };
                let service = service.clone(); let token = token.clone();
                let hub = hub.clone(); let ringing_leases = ringing_leases.clone();
                let pending_commands = pending_commands.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error)=handle_connection(stream,token,service,shutdown,hub,ringing_leases,pending_commands).await { log::warn!("control connection: {error}"); }
                });
            }
            changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { break },
        }
    }
    service.shutdown();
    // 退出前主动收尾孤儿（stop 协议已在 handler 做过；此处兜底其他退出
    // 路径，如生命周期接管/信号退出。幂等：已 seal 的 turn 跳过）。
    hub.seal_all_orphans();
    // F2: timeline 持久化是异步合并 checkpoint；退出前同步落盘全部 pending
    // seed，缩小子进程被杀时 transcript 尾部的丢失窗口。
    hub.flush_timeline_persistence();
    if let Some(ref ws) = workspace {
        ws.stop();
    }
    let _ = std::fs::remove_file(qaqh_types::platform::daemon_discovery_path());
    let _ = std::fs::remove_file(qaqh_types::platform::daemon_lock_path());
    Ok(())
}

async fn handle_connection(
    mut stream: TcpStream,
    token: String,
    service: QaqhService,
    shutdown: watch::Sender<bool>,
    hub: Arc<RingingHub>,
    ringing_leases: Arc<Mutex<crate::ringing_http::RingingLeaseStore>>,
    pending_commands: Arc<Mutex<crate::ringing_http::PendingCommandStore>>,
) -> Result<(), String> {
    let mut peek = [0_u8; 2048];
    let count = stream.peek(&mut peek).await.map_err(stringify)?;
    let preview = String::from_utf8_lossy(&peek[..count]);
    if preview.starts_with("POST /control/v1/stop ")
        || preview.starts_with("POST /control/v1/stop-if-idle ")
    {
        use tokio::io::AsyncWriteExt;
        let authorized = preview
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("Authorization: Bearer {token}")));
        let idle_required = preview.starts_with("POST /control/v1/stop-if-idle ");
        let busy = idle_required && service.has_active_work();
        if !authorized {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .map_err(stringify)?;
            return Ok(());
        }
        if busy {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .map_err(stringify)?;
            return Ok(());
        }
        // Windows 95 语义：收尾完成后才返回 200（"现在可以安全关闭电源了"）。
        // ① worker 优雅退出（SessionShutdown 帧 + 等进程退出 + join stdout
        //    消费线程排空管道——尾部 intent 含 seal_turn 已 publish/落盘）；
        // ② seal_all_orphans 兜底：worker 超时被杀时不留未 seal turn；
        // ③ flush 异步 timeline checkpoint（此时数据已齐）。
        // 安装器收到 200 即确认可安全关闭；超时降级强杀（win_process.rs）。
        service.shutdown();
        hub.seal_all_orphans();
        hub.flush_timeline_persistence();
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .map_err(stringify)?;
        let _ = shutdown.send(true);
        return Ok(());
    }

    // Ringing HTTP/SSE 分流（PLAN：legacy WS 与 Ringing HTTP/SSE 并行、互不嵌套）
    if preview.starts_with("POST /ringing/") || preview.starts_with("GET /ringing/") {
        return crate::ringing_http::handle_ringing_http(
            stream,
            &preview,
            &token,
            hub,
            ringing_leases,
            service,
            pending_commands,
        )
        .await;
    }

    // Debug 只读页：静态服务前端产物（浏览器调试入口，无需 Electron）
    if preview.starts_with("GET /debug") {
        return crate::debug_http::handle_debug_http(stream, &preview, &token).await;
    }

    // M3：legacy `/control/v1` WS 数据协议已拆除；此处只剩生命周期与
    // Ringing HTTP/SSE 分流。未知请求返回 404 而非直接断连——
    // 浏览器对缺失资源（favicon 等）期望 HTTP 响应，断连表现为
    // ERR_CONNECTION_RESET 刷屏控制台。
    use tokio::io::AsyncWriteExt;
    let _ = stream
        .write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found",
        )
        .await;
    Ok(())
}

pub fn random_hex() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn acquire_single_instance() -> Result<File, String> {
    let path = qaqh_types::platform::daemon_lock_path();
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(mut file) => {
            writeln!(file, "{}", std::process::id()).map_err(stringify)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // 判活以 lock 文件里的 pid 为权威：pid 进程活着即视为已有实例，
            // 无论其 HTTP 是否已就绪（daemon 启动窗口内端口可达但尚未
            // accept —— 此时若按 TCP 判活会把正在初始化的实例误判为 stale
            // 并删锁接管，导致多个 daemon 并存、discovery 端口漂移）。
            // 仅当 lock 持有者确实已退出（pid 失效）才清理并接管。
            //
            // 空/坏锁窗口：`create_new` 与 `writeln(pid)` 之间有空窗，并发
            // 启动的另一个实例可能已创建锁文件但尚未写入 pid。读到空锁时
            // 短等重试（上限 300ms）而非立即接管——否则会把正在初始化的
            // 实例误判为 stale 删锁接管，双 daemon 并存（根因，见
            // acquire_single_instance 竞态分析）。重试后仍读不到 pid 才视为
            // stale（持有者已退出或从未写完）。
            let mut lock_pid = read_lock_pid(&path);
            for _ in 0..10 {
                if lock_pid.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
                lock_pid = read_lock_pid(&path);
            }
            #[cfg(windows)]
            let holder_alive = match lock_pid {
                Some(pid) => qaqh_types::platform::process_is_running(pid),
                None => false,
            };
            // 非 Windows 无 pid 判活实现（stub 恒 true），回退旧行为：
            // 视为 stale 并接管，保证 daemon 可重新启动（桌面端仅 Windows）。
            #[cfg(not(windows))]
            let holder_alive = false;
            if holder_alive {
                return Err("another daemon instance is already running".into());
            }
            std::fs::remove_file(&path).map_err(|e| format!("remove stale daemon lock: {e}"))?;
            let _ = std::fs::remove_file(qaqh_types::platform::daemon_discovery_path());
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(stringify)?;
            writeln!(file, "{}", std::process::id()).map_err(stringify)?;
            Ok(file)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// 读锁文件中的持有者 pid（文件缺失/内容非数字 → None）。
fn read_lock_pid(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
}
fn write_discovery(discovery: &DaemonDiscovery) -> Result<(), String> {
    let target = qaqh_types::platform::daemon_discovery_path();
    let temp = target.with_extension("json.tmp");
    let mut file = File::create(&temp).map_err(stringify)?;
    serde_json::to_writer_pretty(&mut file, discovery).map_err(stringify)?;
    file.flush().map_err(stringify)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(stringify)?;
    }
    if target.exists() {
        std::fs::remove_file(&target).map_err(stringify)?;
    }
    std::fs::rename(temp, &target).map_err(stringify)?;
    restrict_discovery_permissions(&target)
}

#[cfg(windows)]
fn restrict_discovery_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("whoami")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("resolve current Windows identity: {error}"))?;
    if !output.status.success() {
        return Err("resolve current Windows identity: whoami failed".into());
    }
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &format!("{identity}:(F)")])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| format!("restrict daemon discovery ACL: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "restrict daemon discovery ACL: icacls failed".into())
}

#[cfg(not(windows))]
fn restrict_discovery_permissions(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}
