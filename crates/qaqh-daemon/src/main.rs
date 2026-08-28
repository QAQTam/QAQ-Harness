mod axum_server;
mod debug_http;
mod http;
mod ringing_http;
mod server;

use std::io::{Read, Write};

/// 诊断日志 sink：daemon 此前无任何 logger 初始化，msgloop 的
/// [COMPACT]/[TURN] 等 log::error/warn 全部丢弃，压缩失败等原因无从追查。
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
            // Preserve the complete interactive PATH for workers, but defer
            // prompt-only OS/tool probing to each worker process.
            qaqh_runtime::cache_system_path();
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(error) = runtime.block_on(server::run()) {
                eprintln!("qaqh-daemon: {error}");
                std::process::exit(1);
            }
        }
        Some(command) => {
            eprintln!("unknown command: {command}; expected run, server, status, or stop");
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

fn discovery_reachable(discovery: &qaqh_proto::DaemonDiscovery) -> bool {
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

fn read_discovery() -> Result<qaqh_proto::DaemonDiscovery, String> {
    let content = std::fs::read_to_string(qaqh_types::platform::daemon_discovery_path())
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}
