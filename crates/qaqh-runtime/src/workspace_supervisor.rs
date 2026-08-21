//! WorkspaceSupervisor — daemon 拉起的 qaqh-workspace serve 生命周期管理。
//!
//! daemon 启动时按 config `[workspace] mode` 拉起工具服务：
//!
//! - `local`：本机 `qaqh-workspace serve`（与 daemon 同目录，Windows 原生）。
//! - `wsl`（仅 Windows）：经 `wsl.exe` 在 WSL 发行版内拉起
//!   `qaqh-workspace serve`；WSL2 localhost 转发使 Windows daemon 经
//!   127.0.0.1 访问。WSL 不可用/未装 serve 时自动回退 local（降级记录）。
//!
//! 生命周期：随 daemon —— 拉起 → /health 就绪 → 崩溃退避重启（5s）→
//! daemon shutdown 时 kill 回收。worker 经 `QAQH_WORKSPACE_URL/TOKEN`
//! 环境变量注入 endpoint；serve 崩溃重启后端口变化时，已运行 worker 由
//! HttpToolExecutionBackend 自动回退进程内执行（不影响代理环路），
//! 新 worker 拿到新 endpoint。

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// 工具套件运行环境。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMode {
    Local,
    Wsl,
}

impl WorkspaceMode {
    pub fn parse(mode: &str) -> Self {
        if cfg!(target_os = "windows") && mode.eq_ignore_ascii_case("wsl") {
            WorkspaceMode::Wsl
        } else {
            WorkspaceMode::Local
        }
    }

    /// 稳定字符串标签（config 与状态上报使用："local" | "wsl"）。
    pub fn label(&self) -> &'static str {
        match self {
            WorkspaceMode::Local => "local",
            WorkspaceMode::Wsl => "wsl",
        }
    }
}

pub struct WorkspaceSupervisor {
    /// 当前已通过 health 的连接资料。每次子进程重启均原子替换。
    connection: Arc<RwLock<WorkspaceConnection>>,
    /// 当前实际运行模式（WSL 降级后为 Local；stop() 按此回收）。
    mode: WorkspaceMode,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
}

/// workspace 连接代际；endpoint/token 仅留在 daemon 内存并注入新 worker。
#[derive(Clone)]
pub struct WorkspaceConnection {
    pub generation: u64,
    pub endpoint: String,
    pub token: String,
    pub mode: WorkspaceMode,
}

pub type WorkspaceConnectionHandler = Arc<dyn Fn(WorkspaceConnection) + Send + Sync>;

fn workspace_exe_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "qaqh-workspace.exe"
    } else {
        "qaqh-workspace"
    }
}

impl WorkspaceSupervisor {
    /// 拉起 serve 并等待就绪（超时 5s）。
    ///
    /// 降级链：请求模式拉起 → READY/health 未就绪（含 WSL 预检失败）→
    /// 若请求是 WSL 则回退 local serve → 再失败才返回 Err（daemon 禁用
    /// workspace 服务，worker 全本地执行）。绝不静默半启用。
    pub fn start(
        mode: WorkspaceMode,
        on_connection: WorkspaceConnectionHandler,
    ) -> Result<Self, String> {
        let (child, token, endpoint, active_mode, label) = match Self::try_start(mode) {
            Ok(ready) => ready,
            Err(wsl_error) if mode == WorkspaceMode::Wsl => {
                // WSL 不可用/未就绪 → 回退 local serve（降级记录）。
                log::warn!(
                    "[workspace] WSL mode unavailable ({wsl_error}); falling back to local serve"
                );
                match Self::try_start(WorkspaceMode::Local) {
                    Ok(ready) => ready,
                    Err(local_error) => {
                        return Err(format!(
                            "workspace serve unavailable (wsl: {wsl_error}; local: {local_error})"
                        ));
                    }
                }
            }
            Err(e) => return Err(format!("workspace serve: {e}")),
        };

        let connection = WorkspaceConnection {
            generation: 1,
            endpoint,
            token,
            mode: active_mode,
        };
        let supervisor = WorkspaceSupervisor {
            connection: Arc::new(RwLock::new(connection.clone())),
            mode: active_mode,
            child: Arc::new(Mutex::new(Some(child))),
            stop: Arc::new(AtomicBool::new(false)),
        };
        supervisor.watchdog(active_mode, label.clone(), on_connection);
        log::info!(
            "[workspace] serve ready at {} ({label})",
            connection.endpoint
        );
        Ok(supervisor)
    }

    /// 读取当前已健康检查通过的连接代际。
    pub fn connection(&self) -> WorkspaceConnection {
        self.connection
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 尝试拉起并等待就绪。返回 (child, token, endpoint, active_mode, label)。
    fn try_start(
        mode: WorkspaceMode,
    ) -> Result<(Child, String, String, WorkspaceMode, String), String> {
        if mode == WorkspaceMode::Wsl {
            // 预检：WSL 可用且发行版内存在 qaqh-workspace。
            // 快速失败（2s 超时），避免在 READY 阶段空等 5s 才发现。
            let probe = Command::new("wsl.exe")
                .args(["-e", "bash", "-lc", "command -v qaqh-workspace"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match probe {
                Ok(mut p) => {
                    let ok = p.wait().map(|s| s.success()).unwrap_or(false);
                    if !ok {
                        return Err("wsl.exe probe failed: qaqh-workspace not found in WSL (or WSL unavailable)".into());
                    }
                }
                Err(e) => {
                    return Err(format!("wsl.exe probe failed: {e}"));
                }
            }
        }
        let token = random_hex();
        let mut child = Self::spawn(mode, &token)?;

        // 读取 stdout 的 `QAQH_WORKSPACE_READY <host>:<port>` 行：
        // 专用线程阻塞读取，主循环 recv_timeout 非阻塞轮询子进程状态。
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "workspace stdout unavailable".to_string())?;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut ready_sent = false;
            loop {
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if !ready_sent
                            && let Some(rest) = line.strip_prefix("QAQH_WORKSPACE_READY ")
                        {
                            let _ = ready_tx.send(rest.trim().to_string());
                            ready_sent = true;
                        }
                        line.clear();
                    }
                    Err(_) => break,
                }
            }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let endpoint = loop {
            if let Ok(addr) = ready_rx.recv_timeout(Duration::from_millis(100)) {
                break format!("http://{addr}");
            }
            if let Some(code) = child.try_wait().map_err(|e| e.to_string())? {
                let _ = child.kill();
                return Err(format!("workspace serve exited early with code {code}"));
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                return Err("workspace serve did not publish READY within 5s".into());
            }
        };

        // /health 确认（serve 内 init_tools 在 READY 行前完成，健康探测兜底）。
        let health_deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(response) = ureq::Agent::config_builder()
                .timeout_connect(Some(Duration::from_secs(2)))
                .timeout_per_call(Some(Duration::from_secs(2)))
                .build()
                .new_agent()
                .get(&format!("{endpoint}/health"))
                .header("Authorization", &format!("Bearer {token}"))
                .call()
            {
                if response.status() == 200 {
                    break;
                }
            }
            if std::time::Instant::now() >= health_deadline {
                let _ = child.kill();
                return Err("workspace serve /health did not become ready within 5s".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok((child, token, endpoint, mode, format!("{mode:?}")))
    }

    fn spawn(mode: WorkspaceMode, token: &str) -> Result<Child, String> {
        let mut command = match mode {
            WorkspaceMode::Local => {
                let exe = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join(workspace_exe_name())))
                    .filter(|p| p.exists())
                    .unwrap_or_else(|| workspace_exe_name().into());
                // 完整性校验：与 daemon-manifest.json 记录的 SHA-256 比对。
                // 防 resources/ 内 serve 二进制被替换后向 agent 回传伪造工具结果。
                Self::verify_local(&exe)?;
                let mut command = Command::new(exe);
                // token 走环境变量（防进程命令行泄露）；--port 0 = 随机端口。
                command.args(["serve", "--port", "0"]);
                command.env("QAQH_WORKSPACE_TOKEN", token);
                command
            }
            WorkspaceMode::Wsl => {
                // WSL 发行版内须已安装 qaqh-workspace（PATH 可达）。
                let mut command = Command::new("wsl.exe");
                command.args([
                    "-e",
                    "bash",
                    "-lc",
                    "QAQH_WORKSPACE_TOKEN=\"$QAQH_WORKSPACE_TOKEN\" qaqh-workspace serve --port 0",
                ]);
                command.env("QAQH_WORKSPACE_TOKEN", token);
                command
            }
        };
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn: {e}"))
    }

    /// 校验 local workspace 二进制哈希与 manifest 一致。
    ///
    /// - manifest 缺失（dev 模式 / 未打包）→ 跳过（记录 info，不阻塞开发）；
    /// - manifest 无 workspace_sha256（旧版打包）→ 跳过；
    /// - 哈希不匹配 → Err：拒绝拉起（防篡改，daemon 退化为纯本地执行）。
    fn verify_local(exe: &std::path::Path) -> Result<(), String> {
        let manifest_dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
        let manifest_path = manifest_dir.join("daemon-manifest.json");
        let manifest = match std::fs::read_to_string(&manifest_path) {
            Ok(text) => text,
            Err(_) => {
                log::info!(
                    "[workspace] no daemon-manifest.json (dev mode); skipping integrity check"
                );
                return Ok(());
            }
        };
        let json: serde_json::Value = serde_json::from_str(&manifest)
            .map_err(|e| format!("parse daemon-manifest.json: {e}"))?;
        let expected = match json.get("workspace_sha256").and_then(|v| v.as_str()) {
            Some(hash) => hash.to_string(),
            None => {
                log::warn!(
                    "[workspace] manifest has no workspace_sha256 (old package); skipping check"
                );
                return Ok(());
            }
        };
        let bytes = std::fs::read(exe).map_err(|e| format!("read workspace binary: {e}"))?;
        let actual = {
            use sha2::Digest;
            let digest = sha2::Sha256::digest(&bytes);
            let mut out = String::with_capacity(digest.len() * 2);
            for byte in digest {
                out.push_str(&format!("{byte:02x}"));
            }
            out
        };
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(format!(
                "workspace binary integrity check FAILED: {} (expected {expected}, got {actual}); refusing to launch",
                exe.display()
            ));
        }
        log::info!("[workspace] binary integrity verified: {}", exe.display());
        Ok(())
    }

    /// 崩溃退避重启：serve 异常退出后 5s 重拉；shutdown 置 stop 后退出。
    fn watchdog(
        &self,
        mode: WorkspaceMode,
        label: String,
        on_connection: WorkspaceConnectionHandler,
    ) {
        let child = self.child.clone();
        let stop = self.stop.clone();
        let connection = self.connection.clone();
        std::thread::Builder::new()
            .name("workspace-supervisor".into())
            .spawn(move || {
                let mut health_failures: u32 = 0;
                loop {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let exited = {
                        let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
                        match guard.as_mut() {
                            Some(c) => c.try_wait().ok().flatten().is_some(),
                            None => true,
                        }
                    };
                    if !exited {
                        // 稳态健康轮询：/health 在 serve 的 HTTP 线程直接响应
                        // （不经串行 worker），长 exec 不会误伤。连续失败判定
                        // 进程挂死（活着但不再 accept/响应）→ 主动杀，走统一
                        // 重启路径——防"端口无监听但进程未退出"的僵尸态。
                        if probe_health(&connection) {
                            health_failures = 0;
                        } else {
                            health_failures += 1;
                            if health_failures >= HEALTH_FAILURE_LIMIT {
                                log::warn!(
                                    "[workspace] serve unhealthy ({health_failures} failed probes); killing for restart ({label})"
                                );
                                let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(c) = guard.as_mut() {
                                    kill_serve_process(mode, c);
                                }
                                health_failures = 0;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    // 崩溃/退出 → 退避重启。端口随机，endpoint 可能变化：
                    // 已运行 worker 降级本地执行，新 worker 由 daemon 重启后
                    // 重新注入（v1 语义：serve 长稳，崩溃是罕见路径）。
                    log::warn!("[workspace] serve exited; restarting in 5s ({label})");
                    std::thread::sleep(Duration::from_secs(5));
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    // 只有 READY + /health 均成功后才发布新 endpoint/token；失败继续退避。
                    match Self::try_start(mode) {
                        Ok((new_child, token, endpoint, active_mode, _)) => {
                            let next = WorkspaceConnection {
                                generation: connection
                                    .read()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .generation
                                    .saturating_add(1),
                                endpoint,
                                token,
                                mode: active_mode,
                            };
                            // 先更新 registry，随后再暴露新进程；新 worker 绝不会拿到旧凭据。
                            on_connection(next.clone());
                            *connection
                                .write()
                                .unwrap_or_else(|error| error.into_inner()) = next;
                            let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
                            *guard = Some(new_child);
                        }
                        Err(e) => log::warn!("[workspace] restart failed: {e}"),
                    }
                }
            })
            .ok();
    }

    /// daemon shutdown：杀进程树回收。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = guard.take() {
            match self.mode {
                // WSL 模式：taskkill 只杀 Windows 侧 wsl.exe 客户端，
                // WSL 发行版内的 serve 进程不会退出（孤儿常驻）。
                // 必须经 wsl.exe 在发行版内 pkill。
                WorkspaceMode::Wsl => {
                    let _ = Command::new("wsl.exe")
                        .args(["-e", "bash", "-lc", "pkill -f 'qaqh-workspace serve'"])
                        .status();
                    let _ = child.kill();
                    let _ = child.wait();
                }
                WorkspaceMode::Local => {
                    #[cfg(target_os = "windows")]
                    {
                        let _ = Command::new("taskkill")
                            .args(["/pid", &child.id().to_string(), "/T", "/F"])
                            .status();
                        let _ = child.wait();
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            }
        }
    }

    /// 实际运行模式（WSL 降级后为 Local；供 daemon 状态上报）。
    pub fn mode_label(&self) -> &'static str {
        self.mode.label()
    }
}

fn random_hex() -> String {
    use sha2::Digest;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{nanos}-{}", std::process::id());
    let digest = sha2::Sha256::digest(seed.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ═════════════════════════════════════════════════════════
// WSL 诊断与安装（前端"工具套件"分类的流程入口）
// ═════════════════════════════════════════════════════════

/// Windows 路径 → WSL /mnt 路径（`F:\QAQ-Harness` → `/mnt/f/QAQ-Harness`）。
fn to_wsl_path(windows_path: &str) -> Option<String> {
    let path = std::path::Path::new(windows_path);
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Prefix(prefix)) => {
            let drive = prefix.as_os_str().to_string_lossy();
            let drive_letter = drive
                .trim_start_matches("\\\\?\\")
                .chars()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !drive_letter.is_ascii_alphabetic() {
                return None;
            }
            let rest: Vec<String> = components
                .filter_map(|c| match c {
                    std::path::Component::Normal(seg) => Some(seg.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            let mut out = format!("/mnt/{drive_letter}");
            for seg in rest {
                out.push('/');
                out.push_str(&seg);
            }
            Some(out)
        }
        _ => None,
    }
}

/// 运行一条 WSL 命令并收集输出。
fn run_wsl(args: &[&str], timeout_secs: u64) -> Result<(bool, String), String> {
    let mut command = Command::new("wsl.exe");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|e| format!("spawn wsl.exe: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "wsl stdout unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "wsl stderr unavailable".to_string())?;
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            text.push_str(&line);
            text.push('\n');
        }
        let _ = out_tx.send(text);
    });
    std::thread::spawn(move || {
        let mut text = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            text.push_str(&line);
            text.push('\n');
        }
        let _ = err_tx.send(text);
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            return Err(format!("wsl command timed out after {timeout_secs}s"));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = out_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    let stderr = err_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    let combined = format!("{stdout}{stderr}").trim().to_string();
    Ok((status.success(), combined))
}

/// WSL 诊断：逐项检测并做真实连接测试。
///
/// 返回 JSON 结构（前端逐项渲染）：
/// ```json
/// {
///   "wsl_available": true, "distro": "Ubuntu-22.04",
///   "workspace_installed": true, "workspace_version": "...",
///   "connection_ok": true, "endpoint": "http://127.0.0.1:xxxxx",
///   "error": null
/// }
/// ```
pub fn diagnose_wsl() -> Result<serde_json::Value, String> {
    if cfg!(not(target_os = "windows")) {
        return Ok(serde_json::json!({
            "wsl_available": false,
            "note": "非 Windows 系统无需 WSL（工具直接运行在系统环境）",
        }));
    }

    // 1. wsl.exe 存在 + 默认发行版连通。
    let (wsl_ok, _) = match run_wsl(&["-e", "bash", "-lc", "echo wsl-ok"], 10) {
        Ok((ok, out)) => (ok, out),
        Err(e) => {
            return Ok(serde_json::json!({
                "wsl_available": false,
                "workspace_installed": false,
                "connection_ok": false,
                "error": format!("WSL 不可用: {e}"),
            }));
        }
    };
    if !wsl_ok {
        return Ok(serde_json::json!({
            "wsl_available": false,
            "workspace_installed": false,
            "connection_ok": false,
            "error": "wsl.exe 存在但默认发行版不可用（请先安装/启动 WSL 发行版）",
        }));
    }

    // 2. 发行版信息。
    let distro = run_wsl(
        &[
            "-e",
            "bash",
            "-lc",
            "source /etc/os-release 2>/dev/null; echo \"$PRETTY_NAME\"",
        ],
        10,
    )
    .ok()
    .map(|(_, out)| out.trim().to_string())
    .filter(|s| !s.is_empty());

    // 3. qaqh-workspace 是否已安装（PATH 可达）。
    let (installed, workspace_version) = match run_wsl(
        &[
            "-e",
            "bash",
            "-lc",
            "command -v qaqh-workspace && qaqh-workspace list 2>&1 | tail -1",
        ],
        15,
    ) {
        Ok((true, out)) => {
            let lines: Vec<&str> = out.lines().collect();
            let version = lines
                .last()
                .map(|l| l.trim().to_string())
                .filter(|s| s.contains("tools registered"))
                .unwrap_or_else(|| "installed".into());
            (true, Some(version))
        }
        Ok((false, _)) => (false, None),
        Err(e) => {
            return Ok(serde_json::json!({
                "wsl_available": true,
                "distro": distro,
                "workspace_installed": false,
                "connection_ok": false,
                "error": format!("检查 qaqh-workspace 失败: {e}"),
            }));
        }
    };

    if !installed {
        return Ok(serde_json::json!({
            "wsl_available": true,
            "distro": distro,
            "workspace_installed": false,
            "connection_ok": false,
            "error": "WSL 内未安装 qaqh-workspace，请点击「安装到 WSL」",
        }));
    }

    // 4. 真实连接测试：拉起 serve → READY → health → 回收。
    match WorkspaceSupervisor::try_start(WorkspaceMode::Wsl) {
        Ok((mut child, token, endpoint, _mode, _label)) => {
            let health = ureq::Agent::config_builder()
                .timeout_connect(Some(Duration::from_secs(3)))
                .timeout_per_call(Some(Duration::from_secs(3)))
                .build()
                .new_agent()
                .get(&format!("{endpoint}/health"))
                .header("Authorization", &format!("Bearer {token}"))
                .call();
            let health_ok = health.is_ok_and(|r| r.status() == 200);
            // 回收：WSL 内 pkill + 杀 wsl.exe 客户端。
            let _ = Command::new("wsl.exe")
                .args(["-e", "bash", "-lc", "pkill -f 'qaqh-workspace serve'"])
                .status();
            let _ = child.kill();
            let _ = child.wait();
            Ok(serde_json::json!({
                "wsl_available": true,
                "distro": distro,
                "workspace_installed": true,
                "workspace_version": workspace_version,
                "connection_ok": health_ok,
                "endpoint": endpoint,
                "error": null,
            }))
        }
        Err(e) => Ok(serde_json::json!({
            "wsl_available": true,
            "distro": distro,
            "workspace_installed": true,
            "workspace_version": workspace_version,
            "connection_ok": false,
            "error": format!("连接测试失败: {e}"),
        })),
    }
}

/// 安装 qaqh-workspace 到 WSL（拷贝源码到 WSL 原生路径构建）。
///
/// - `repo_root`（可选）：Windows 侧仓库根（如 `F:\QAQ-Harness`）。缺失时尝试从
///   当前可执行文件推导（dev 模式）；推导不到返回需要参数的指引。
///
/// 流程（为什么拷贝而不是 /mnt 直连构建）：
///   1. WSL 的 /mnt 是 9p 文件系统，git2 vendored 有数千小文件，直连编译
///      极慢；拷贝到 WSL 原生路径（ext4）构建速度正常。
///   2. tar 排除 target/.git/node_modules 等大目录；`~/.deepx-workspace-src/`
///      内的旧 target 保留 → 二次安装增量编译（秒级）。
///   3. 产物 install 到 `~/.local/bin/qaqh-workspace`；若 PATH 未含
///      `~/.local/bin` 则幂等追加到 `~/.bashrc`（否则 supervisor 的
///      `command -v qaqh-workspace` 找不到）。
///   4. 首次构建可能耗时数分钟（依赖编译）；超时 900s。
pub fn install_wsl(repo_root: Option<&str>) -> Result<serde_json::Value, String> {
    if cfg!(not(target_os = "windows")) {
        return Err("非 Windows 系统无需安装 WSL 工具套件".into());
    }
    let repo_root = match repo_root {
        Some(root) if !root.is_empty() => root.to_string(),
        _ => match derive_repo_root() {
            Some(root) => root,
            None => {
                return Err(
                    "无法推导 QAQ-Harness 仓库路径：请在请求中提供 repo_root（Windows 路径，如 F:\\QAQ-Harness）"
                        .into(),
                );
            }
        },
    };
    let wsl_repo = to_wsl_path(&repo_root)
        .ok_or_else(|| format!("无法转换仓库路径到 WSL 格式: {repo_root}"))?;

    // 已安装 → 直接成功（幂等）。
    let (installed, _) = run_wsl(&["-e", "bash", "-lc", "command -v qaqh-workspace"], 15)?;
    if installed {
        return Ok(serde_json::json!({
            "already_installed": true,
            "ok": true,
            "message": "WSL 内已存在 qaqh-workspace",
        }));
    }

    // cargo 可用性检查。
    let (has_cargo, _) = run_wsl(&["-e", "bash", "-lc", "command -v cargo"], 15)?;
    if !has_cargo {
        return Err(
            "WSL 内未安装 Rust 工具链。请先在 WSL 中执行:\n  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y\n然后重试。"
                .into(),
        );
    }

    // 源码存在性检查（/mnt 映射）。
    let (src_ok, out) = run_wsl(
        &[
            "-e",
            "bash",
            "-lc",
            &format!("test -f {wsl_repo}/Cargo.toml && echo src-ok"),
        ],
        15,
    )?;
    if !src_ok || !out.contains("src-ok") {
        return Err(format!(
            "WSL 无法访问仓库源码 {wsl_repo}（检查 /mnt 挂载与路径）"
        ));
    }

    // 1. 拷贝源码到 WSL 原生路径（排除大目录；保留旧 target 供增量）。
    //    find 清空旧源码（target 除外），tar 管道 /mnt → ~/.deepx-workspace-src。
    let copy_script = format!(
        "set -e; \
         mkdir -p ~/.deepx-workspace-src; \
         find ~/.deepx-workspace-src -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {{}} + 2>/dev/null || true; \
         cd {wsl_repo} && \
         tar --exclude=target --exclude=.git --exclude=.deepx --exclude=node_modules \
             --exclude=out --exclude=release --exclude=packages --exclude=staging \
             --exclude=payload --exclude=.cache -cf - . | \
         tar -xf - -C ~/.deepx-workspace-src && \
         echo COPY_OK"
    );
    let (copy_ok, copy_out) = run_wsl(&["-e", "bash", "-lc", &copy_script], 300)?;
    if !copy_ok || !copy_out.contains("COPY_OK") {
        return Err(format!("拷贝源码到 WSL 失败:\n{copy_out}"));
    }

    // 2. 构建 + 安装（WSL 原生路径；target 增量保留）。构建输出较大，
    //    截断保存尾部（成功/失败都返回摘要）。
    let build_cmd = format!(
        "set -e; \
         cd ~/.deepx-workspace-src && \
         cargo build --release -p qaqh-workspace 2>&1 | tail -40; \
         test -x target/release/qaqh-workspace && \
         mkdir -p ~/.local/bin && \
         install -m 755 target/release/qaqh-workspace ~/.local/bin/qaqh-workspace && \
         if ! command -v qaqh-workspace >/dev/null 2>&1; then \
           grep -q '.local/bin' ~/.bashrc 2>/dev/null || echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.bashrc; \
         fi && \
         echo INSTALL_OK"
    );
    let (ok, out) = run_wsl(&["-e", "bash", "-lc", &build_cmd], 900)?;
    if !ok || !out.contains("INSTALL_OK") {
        return Err(format!("构建/安装失败:\n{out}"));
    }

    // 3. 确认（全路径验证，不依赖 PATH）。
    let (ok2, out2) = run_wsl(
        &[
            "-e",
            "bash",
            "-lc",
            "~/.local/bin/qaqh-workspace list 2>&1 | tail -1",
        ],
        15,
    )?;
    Ok(serde_json::json!({
        "already_installed": false,
        "ok": ok2,
        "message": if ok2 { "安装成功" } else { "安装完成但验证失败" },
        "verify": out2.trim(),
        "detail": out.lines().last().unwrap_or("").to_string(),
    }))
}

/// 推导 Windows 侧仓库根：dev 模式 current_exe → 向上找 Cargo.toml（最多 6 级）。
fn derive_repo_root() -> Option<String> {
    let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for _ in 0..6 {
        if dir.join("Cargo.toml").exists() {
            return Some(dir.to_string_lossy().into_owned());
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_defaults_to_local() {
        assert_eq!(WorkspaceMode::parse("local"), WorkspaceMode::Local);
        assert_eq!(WorkspaceMode::parse("LOCAL"), WorkspaceMode::Local);
        assert_eq!(WorkspaceMode::parse("bogus"), WorkspaceMode::Local);
        assert_eq!(WorkspaceMode::parse(""), WorkspaceMode::Local);
        // wsl 仅在 Windows 上接受（Linux 原生系统无此选项）
        assert_eq!(
            WorkspaceMode::parse("wsl"),
            if cfg!(target_os = "windows") {
                WorkspaceMode::Wsl
            } else {
                WorkspaceMode::Local
            }
        );
    }

    #[test]
    fn random_hex_is_unique_and_hex() {
        let a = random_hex();
        let b = random_hex();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn verify_local_accepts_matching_hash_and_rejects_mismatch() {
        let temp = std::env::temp_dir().join(format!("qaqh-ws-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let exe = temp.join("qaqh-workspace.exe");
        std::fs::write(&exe, b"fake-binary-content").unwrap();
        let manifest = temp.join("daemon-manifest.json");

        // 匹配哈希 → Ok
        let good_hash: String = {
            use sha2::Digest;
            let digest = sha2::Sha256::digest(b"fake-binary-content");
            digest.iter().map(|b| format!("{b:02x}")).collect()
        };
        std::fs::write(
            &manifest,
            format!(r#"{{"workspace_sha256":"{good_hash}"}}"#),
        )
        .unwrap();
        assert!(WorkspaceSupervisor::verify_local(&exe).is_ok());

        // 不匹配 → Err（拒绝拉起）
        std::fs::write(&manifest, r#"{"workspace_sha256":"0000000000000000000000000000000000000000000000000000000000000000"}"#)
            .unwrap();
        let err = WorkspaceSupervisor::verify_local(&exe).unwrap_err();
        assert!(err.contains("integrity check FAILED"), "unexpected: {err}");

        // manifest 缺失（dev 模式）→ Ok
        std::fs::remove_file(&manifest).unwrap();
        assert!(WorkspaceSupervisor::verify_local(&exe).is_ok());

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    #[cfg(windows)] // Windows 盘符/cmd 语义；Linux 无对应环境
    fn wsl_path_conversion_maps_drive_and_segments() {
        assert_eq!(
            to_wsl_path(r"F:\QAQ-Harness").as_deref(),
            Some("/mnt/f/QAQ-Harness")
        );
        assert_eq!(
            to_wsl_path(r"F:\QAQ-Harness\crates\qaqh-workspace").as_deref(),
            Some("/mnt/f/QAQ-Harness/crates/qaqh-workspace")
        );
        assert_eq!(
            to_wsl_path(r"\\?\C:\Users\qa\QAQ-Harness").as_deref(),
            Some("/mnt/c/Users/qa/QAQ-Harness")
        );
        // 非盘符路径（UNC / 相对）→ None
        assert_eq!(to_wsl_path(r"\\server\share"), None);
        assert_eq!(to_wsl_path("relative/path"), None);
    }

    #[test]
    fn derive_repo_root_finds_cargo_toml_upwards() {
        // dev 模式：current_exe 在 <repo>/target/debug/ 下，向上两级应找到 Cargo.toml。
        if let Some(root) = derive_repo_root() {
            assert!(
                std::path::Path::new(&root).join("Cargo.toml").exists(),
                "derived root must contain Cargo.toml: {root}"
            );
        }
    }
}

/// 连续健康探测失败多少次后判定 serve 挂死并主动杀。
/// 每次探测 2s 超时，三次失败 ≈ 6-8s 内检出。
const HEALTH_FAILURE_LIMIT: u32 = 3;

/// 探测 serve `/health`（2s 超时）。连接未发布（启动初期）时返回 true，
/// 不参与失败计数，避免误杀。
fn probe_health(connection: &RwLock<WorkspaceConnection>) -> bool {
    let Ok(conn) = connection.read() else {
        return false;
    };
    if conn.endpoint.is_empty() {
        return true;
    }
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_per_call(Some(Duration::from_secs(2)))
        .build()
        .new_agent()
        .get(&format!("{}/health", conn.endpoint))
        .header("Authorization", &format!("Bearer {}", conn.token))
        .call()
        .is_ok_and(|resp| resp.status() == 200)
}

/// 按运行模式杀 serve 进程树（与 stop() 同语义，供健康轮询重启路径用）。
fn kill_serve_process(mode: WorkspaceMode, child: &mut Child) {
    match mode {
        WorkspaceMode::Wsl => {
            let _ = Command::new("wsl.exe")
                .args(["-e", "bash", "-lc", "pkill -f 'qaqh-workspace serve'"])
                .status();
            let _ = child.kill();
        }
        WorkspaceMode::Local => {
            #[cfg(target_os = "windows")]
            {
                let _ = Command::new("taskkill")
                    .args(["/pid", &child.id().to_string(), "/T", "/F"])
                    .status();
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = child.kill();
            }
        }
    }
}
