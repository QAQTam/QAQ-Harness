mod axum_server;
mod server;

use std::io::{Read, Write};

/// 诊断日志 sink：daemon 此前无任何 logger 初始化，agent loop（前 msgloop crate，现
/// runtime/agent）的 [COMPACT]/[TURN] 等 log::error/warn 全部丢弃，压缩失败等原因无从追查。
/// 追加写入 `<数据目录>/qaqh-daemon.log`（Windows: `%USERPROFILE%\.qaqh`）。
fn init_file_logging() {
    struct FileLogger(std::sync::Mutex<std::fs::File>);
    impl log::Log for FileLogger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }
        fn log(&self, record: &log::Record) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let Ok(mut file) = self.0.lock() else {
                return;
            };
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(
                file,
                "[{secs}] {:<5} {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
        fn flush(&self) {}
    }
    let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) else {
        return;
    };
    let path = std::path::Path::new(&home)
        .join(".qaqh")
        .join("qaqh-daemon.log");
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = log::set_boxed_logger(Box::new(FileLogger(std::sync::Mutex::new(file))));
    log::set_max_level(log::LevelFilter::Info);
}

fn main() {
    init_file_logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("status") => status(),
        Some("stop") => stop(),
        Some("server") => {
            // 临时跨端模式：headless 监听局域网地址，供远端壳直连。
            qaqh_runtime::cache_system_path();
            qaqh_runtime::detect_os_info();
            let config = match server::ServerNetworkConfig::parse(&args[1..]) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("qaqh-daemon: {error}");
                    std::process::exit(2);
                }
            };
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(error) = runtime.block_on(server::run_with(config)) {
                eprintln!("qaqh-daemon: {error}");
                std::process::exit(1);
            }
        }
        Some("run") | None => {
            // Preserve the complete interactive PATH for workers; OS/toolchain
            // probing runs once here at daemon startup (populates prompt.rs
            // OS_INFO/TOOLS_INFO for {{OS}}/{{TOOLS}} in the system prompt).
            qaqh_runtime::cache_system_path();
            qaqh_runtime::detect_os_info();
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(error) = runtime.block_on(server::run()) {
                eprintln!("qaqh-daemon: {error}");
                std::process::exit(1);
            }
        }
        Some("todo") => std::process::exit(todo_cli(&args[1..])),
        Some(command) => {
            eprintln!("unknown command: {command}; expected run, server, status, stop, or todo");
            std::process::exit(2);
        }
    }
}

fn status() {
    match read_discovery() {
        Ok(discovery) if discovery_reachable(&discovery) => println!(
            "running pid={} endpoint={}",
            discovery.pid, discovery.endpoint
        ),
        Ok(_) => {
            println!("stopped (stale discovery record)");
            std::process::exit(1);
        }
        Err(error) => {
            println!("stopped ({error})");
            std::process::exit(1);
        }
    }
}

fn discovery_reachable(discovery: &qaqh_types::DaemonDiscovery) -> bool {
    if !qaqh_types::platform::process_is_running(discovery.pid) {
        return false;
    }
    let address = discovery
        .endpoint
        .trim_start_matches("ws://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default();
    address.parse().ok().is_some_and(|address| {
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300))
            .is_ok()
    })
}

fn stop() {
    let discovery = match read_discovery() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("daemon is not running: {error}");
            return;
        }
    };
    let endpoint = discovery
        .endpoint
        .trim_start_matches("ws://")
        .trim_start_matches("http://");
    let address = endpoint.split('/').next().unwrap_or(endpoint);
    let Ok(socket_address) = address.parse() else {
        eprintln!("invalid daemon address");
        return;
    };
    match std::net::TcpStream::connect_timeout(&socket_address, std::time::Duration::from_secs(2)) {
        Ok(mut stream) => {
            let request = format!(
                "POST /control/v1/stop HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                discovery.token
            );
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                if response.starts_with("HTTP/1.1 200") {
                    println!("daemon stopping");
                    return;
                }
            }
            eprintln!("daemon rejected stop request");
        }
        Err(error) => eprintln!("cannot connect to daemon: {error}"),
    }
}

fn read_discovery() -> Result<qaqh_types::DaemonDiscovery, String> {
    let content = std::fs::read_to_string(qaqh_types::platform::daemon_discovery_path())
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

// ───────────────────────── todo CLI（薄壳直访 service 面） ─────────────────────────
//
// 路线：daemon.json discovery → /ringing/v1/clients/open（attach_seed 轻量
// lease 握手）→ POST /ringing/v1/service/todo.{list,set}（WRITE_SEEDED/
// READ_SEEDED 白名单）。执行核心与 LLM 工具分发表共用（exec 系列 seed
// 参数化变体），禁止直改 todo.json（进程内互斥/Dashboard 推送断链）。

fn todo_cli(args: &[String]) -> i32 {
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!("usage: qaqh-daemon todo list [--seed S] [--status ST]");
        eprintln!(
            "       qaqh-daemon todo set --seed S (--id T1 --status ST [--evidence E] [--title X] [--description D] | --ids T1,T2 --status ST | --json '<params>')"
        );
        return 2;
    };

    let mut flags = std::collections::BTreeMap::new();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if let Some(flag) = arg.strip_prefix("--") {
            let Some(value) = args.get(i + 1) else {
                eprintln!("todo: --{flag} requires a value");
                return 2;
            };
            flags.insert(flag.to_string(), value.clone());
            i += 2;
        } else {
            eprintln!("todo: unexpected argument {arg}");
            return 2;
        }
    }
    let opt = |name: &str| flags.get(name).cloned();

    let discovery = match read_discovery() {
        Ok(discovery) => discovery,
        Err(error) => {
            eprintln!("daemon is not running: {error}");
            return 1;
        }
    };

    // ── seed 解析：显式 --seed 优先；缺省时唯一会话自动取用 ──
    let seed = match opt("seed") {
        Some(seed) => seed,
        None => match auto_discover_seed(&discovery) {
            Ok(seed) => seed,
            Err(error) => {
                eprintln!("todo: {error}");
                eprintln!(
                    "hint: pass --seed explicitly (session seeds are listed by the TUI / session.list)"
                );
                return 1;
            }
        },
    };

    match action {
        "list" => {
            let mut params = serde_json::json!({ "seed": seed });
            if let Some(status) = opt("status") {
                params["status"] = serde_json::Value::String(status);
            }
            run_service_call(&discovery, &seed, "todo.list", &params)
        }
        "set" => {
            let params = match opt("json") {
                Some(raw) => {
                    let mut value: serde_json::Value = match serde_json::from_str(&raw) {
                        Ok(value @ serde_json::Value::Object(_)) => value,
                        Ok(_) => {
                            eprintln!("todo: --json must be a JSON object");
                            return 2;
                        }
                        Err(error) => {
                            eprintln!("todo: --json is not valid JSON: {error}");
                            return 2;
                        }
                    };
                    value["seed"] = serde_json::Value::String(seed.clone());
                    value
                }
                None => {
                    let mut params = serde_json::json!({ "seed": seed });
                    if let Some(id) = opt("id") {
                        params["id"] = serde_json::Value::String(id);
                    }
                    if let Some(ids) = opt("ids") {
                        params["ids"] = serde_json::Value::Array(
                            ids.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(String::from)
                                .map(serde_json::Value::String)
                                .collect(),
                        );
                    }
                    for field in ["status", "title", "description", "evidence"] {
                        if let Some(value) = opt(field) {
                            params[field] = serde_json::Value::String(value);
                        }
                    }
                    let has_target = params.get("id").is_some() || params.get("ids").is_some();
                    let has_change = ["status", "title", "description", "evidence"]
                        .iter()
                        .any(|f| params.get(*f).is_some());
                    if !has_target || !has_change {
                        eprintln!(
                            "todo: set requires a target (--id/--ids) and a change (--status/--title/--description/--evidence), or --json"
                        );
                        return 2;
                    }
                    params
                }
            };
            run_service_call(&discovery, &seed, "todo.set", &params)
        }
        other => {
            eprintln!("todo: unknown action {other}; expected list or set");
            2
        }
    }
}

/// 缺省 --seed 时：session.list 只返回一个会话则自动取用（多/零会话报错，
/// 绝不静默猜测写错会话的 todo.json）。
fn auto_discover_seed(discovery: &qaqh_types::DaemonDiscovery) -> Result<String, String> {
    let (_, sessions) = http_post_json(
        discovery,
        "/ringing/v1/service/session.list",
        &serde_json::json!({}),
        None,
    )?;
    let Some(entries) = sessions.as_array() else {
        return Err("session.list returned unexpected payload".into());
    };
    let seeds: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry.get("seed").and_then(|v| v.as_str()))
        .collect();
    match seeds.as_slice() {
        [only] => Ok((*only).to_string()),
        [] => Err("no sessions exist yet".into()),
        many => Err(format!(
            "multiple sessions running; specify --seed (candidates: {})",
            many.iter()
                .take(8)
                .copied()
                .collect::<Vec<&str>>()
                .join(", ")
        )),
    }
}

/// open lease（attach_seed 轻量握手）→ 调 service 方法 → 打印结果。
fn run_service_call(
    discovery: &qaqh_types::DaemonDiscovery,
    seed: &str,
    method: &str,
    params: &serde_json::Value,
) -> i32 {
    let instance_id = format!(
        "cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let open_body = serde_json::json!({
        "schema": qaqh_ringing::protocol::RINGING_SCHEMA,
        "version": qaqh_ringing::protocol::RINGING_VERSION,
        "client_instance_id": instance_id,
        "attach_seed": seed,
    });
    let (status, response) =
        match http_post_json(discovery, "/ringing/v1/clients/open", &open_body, None) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("todo: lease open failed: {error}");
                return 1;
            }
        };
    if status != 200 {
        eprintln!("todo: lease open rejected (HTTP {status}): {response}");
        return 1;
    }
    let Some(session_id) = response.get("client_session_id").and_then(|v| v.as_str()) else {
        eprintln!("todo: lease open response missing client_session_id");
        return 1;
    };

    let path = format!("/ringing/v1/service/{method}");
    match http_post_json(discovery, &path, params, Some(session_id)) {
        Ok((status, body)) if (200..300).contains(&status) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
            0
        }
        Ok((status, body)) => {
            eprintln!("todo: {method} failed (HTTP {status}): {body}");
            1
        }
        Err(error) => {
            eprintln!("todo: {method} call failed: {error}");
            1
        }
    }
}

/// 极简 HTTP/1.1 客户端：Bearer token + 可选 lease 头 + JSON 体。
/// `Connection: close` 让服务端回完即关流，读至 EOF 规避分块解析。
/// 与 stop() 同一套本机直连假设（无 TLS；远端跨机模式不在此路线上）。
fn http_post_json(
    discovery: &qaqh_types::DaemonDiscovery,
    path: &str,
    body: &serde_json::Value,
    session_header: Option<&str>,
) -> Result<(u16, serde_json::Value), String> {
    use std::io::{Read as _, Write as _};

    let endpoint = discovery
        .endpoint
        .trim_start_matches("ws://")
        .trim_start_matches("http://");
    let address = endpoint.split('/').next().unwrap_or(endpoint).to_string();
    let socket: std::net::SocketAddr = address
        .parse()
        .map_err(|e| format!("invalid daemon address {address}: {e}"))?;

    let payload = body.to_string();
    let mut request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {address}\r\n\
         Authorization: Bearer {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        discovery.token,
        payload.len(),
    );
    if let Some(session) = session_header {
        request.push_str(&format!("x-qaqh-client-session-id: {session}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&payload);

    let mut stream =
        std::net::TcpStream::connect_timeout(&socket, std::time::Duration::from_secs(3))
            .map_err(|e| format!("connect {address}: {e}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("send request: {e}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("read response: {e}"))?;
    let text = String::from_utf8_lossy(&response);
    let (head, body_text) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response".to_string())?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("malformed status line: {head}"))?;
    let json = serde_json::from_str(body_text.trim()).unwrap_or(serde_json::Value::Null);
    Ok((status, json))
}
