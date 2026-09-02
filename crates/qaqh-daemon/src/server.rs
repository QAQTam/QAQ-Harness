use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};

use qaqh_proto::{CONTROL_PROTOCOL_VERSION, DaemonDiscovery};
use qaqh_runtime::QaqhService;
use qaqh_runtime::RingingHub;
use qaqh_runtime::{WorkspaceMode, WorkspaceSupervisor};
use tokio::net::TcpListener;
use tokio::sync::watch;

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

/// `run` 模式的端口解析：显式 `--port` > `QAQH_SERVER_PORT` 环境变量 > 随机。
///
/// 固定端口是 webUI 开发体验的前置：浏览器书签 / PWA / 反向代理都要求
/// 可预测的地址；随机端口下每次重启 daemon 都要重读 daemon.json。
fn resolve_run_port(configured: u16) -> u16 {
    if configured != 0 {
        return configured;
    }
    std::env::var("QAQH_SERVER_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

pub async fn run_with(config: ServerNetworkConfig) -> Result<(), String> {
    let data_root = qaqh_types::platform::ensure_data_root().map_err(stringify)?;
    // PR-3-1：SessionManager::init 收敛到 daemon main 装配点，全进程经注入
    // 句柄访问会话存储（hub / service / registry 均在此注入）。
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    let sessions = qaqh_session::SessionManager::global();
    let _lock = acquire_single_instance()?;
    let token = config.token.clone().unwrap_or_else(random_hex);
    if config.token.is_none() && !config.bind_ip.is_loopback() {
        // 临时跨端模式：没显式给 key 时把生成值打出来，方便手动填写。
        eprintln!("[qaqh-daemon] generated server token: {token}");
    }
    let epoch = random_hex();
    let listener = TcpListener::bind((config.bind_ip, resolve_run_port(config.port)))
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
        endpoint: format!("http://{advertise_ip}:{}", address.port()),
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
    let hub = Arc::new(
        RingingHub::with_persistence(epoch.clone(), data_root.join("ringing"))
            .with_sessions(sessions.clone()),
    );
    let service = QaqhService::init(sessions);
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
    let ringing_leases = Arc::new(Mutex::new(qaqh_runtime::ringing::RingingLeaseStore::new()));
    let pending_commands = Arc::new(Mutex::new(
        qaqh_runtime::ringing::PendingCommandStore::new_persistent(),
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
    let (shutdown, _) = watch::channel(false);
    // L1 durability: the daemon previously had NO OS signal handling — Ctrl+C
    // or a terminal close killed the process without running the graceful
    // path below, so in-flight turns never reached messages.jsonl (the whole
    // turn's rounds lived only in the worker's in-memory persist queue).
    // Forward SIGINT/SIGTERM into the same watch channel the /control/v1/stop
    // endpoint uses, so every death mode runs the graceful shutdown:
    // service.shutdown() (cancel + SessionShutdown + join) → seal orphans →
    // flush timeline persistence.
    spawn_signal_shutdown(shutdown.clone());
    // E: idle 会话卸载周期任务（docs/memory-governance-plan.md §E）。
    // 每 60s 读一次 config（热生效），对空闲超过阈值的 Session worker 走
    // 优雅 close（join + bundle drop + final flush/drain）。registry.close
    // 是阻塞 join，必须放 spawn_blocking，避免卡死 tokio worker 线程。
    {
        let service = service.clone();
        let mut shutdown_rx = shutdown.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let idle_secs = qaqh_config::watch::authoritative()
                            .map(|config| config.session_idle_unload_secs)
                            .unwrap_or(0);
                        if idle_secs > 0 {
                            let service = service.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                let unloaded = service.unload_idle_sessions(idle_secs);
                                if !unloaded.is_empty() {
                                    log::info!(
                                        "[daemon] idle-unloaded {} session(s): {:?}",
                                        unloaded.len(),
                                        unloaded
                                    );
                                }
                            })
                            .await;
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }

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
    let app_state = crate::axum_server::AppState {
        hub: hub.clone(),
        leases: ringing_leases.clone(),
        pending: pending_commands.clone(),
        service: service.clone(),
        token: token.clone(),
        epoch: epoch.clone(),
        shutdown: shutdown.clone(),
    };
    let app = crate::axum_server::build_router(app_state);
    let mut shutdown_rx = shutdown.subscribe();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown_rx.changed().await;
    })
    .await
    .map_err(stringify)?;
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

pub fn random_hex() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Install SIGINT/SIGTERM handlers that forward into the graceful-shutdown
/// watch channel (same path as `POST /control/v1/stop`). Without this, the
/// default OS disposition killed the daemon instantly and every un-drained
/// turn in the in-process workers was lost.
fn spawn_signal_shutdown(shutdown: watch::Sender<bool>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let ctrl_c = tokio::signal::ctrl_c();
            let sigterm = async move {
                match signal(SignalKind::terminate()) {
                    Ok(mut stream) => {
                        stream.recv().await;
                    }
                    Err(error) => {
                        log::error!("[daemon] SIGTERM handler install failed: {error}");
                        // Degrade to Ctrl+C only: park forever on this branch.
                        std::future::pending::<()>().await;
                    }
                }
            };
            tokio::select! {
                _ = ctrl_c => {}
                _ = sigterm => {}
            }
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_err() {
                log::error!("[daemon] ctrl_c handler unavailable");
                return;
            }
        }
        log::info!("[daemon] termination signal received — entering graceful shutdown");
        let _ = shutdown.send(true);
    });
}

fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// OS 级咨询锁（std `File::try_lock`：Windows `LockFileEx` / Unix `flock`）
/// 保证单实例：持有者是打开的 File 句柄，进程退出（含崩溃）时由内核释放。
/// 因此无需 pid 判活与 stale 锁接管——`WouldBlock` 即代表存在活实例。
fn acquire_single_instance() -> Result<File, String> {
    let path = qaqh_types::platform::daemon_lock_path();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("open daemon lock {path:?}: {e}"))?;
    file.try_lock()
        .map_err(|_| "another daemon instance is already running".to_string())?;
    // pid 仅作诊断记录，不参与判活。
    writeln!(&file, "{}", std::process::id()).map_err(stringify)?;
    Ok(file)
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
