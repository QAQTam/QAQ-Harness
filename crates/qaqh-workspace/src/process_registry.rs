//! ProcessRegistry — tracks child processes spawned by exec / subagent tools.
//!
//! Enables timeout → inspect → wait/kill flow instead of blind termination.
//! Thread-safe: all access through Mutex, with static convenience methods.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Status of a tracked process.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcStatus {
    Running,
    Exited(i32),
    Killed,
}

/// 字节预算内取尾部，起点前移到 char boundary（子进程输出是任意 UTF-8，
/// 直接按字节索引切片会在多字节字符中点 panic）。
fn char_safe_tail(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    // 循环保证 start 落在边界上；get 仅为了通过 string_slice lint。
    s.get(start..).unwrap_or(s)
}

/// One tracked process entry.
pub struct ProcEntry {
    pub id: u32,
    pub name: String,
    pub status: Arc<Mutex<ProcStatus>>,
    pub started: Instant,
    pub output: Arc<Mutex<String>>,
    pub stderr: Arc<Mutex<String>>,
    /// Final answer collected from subagent stdout.
    pub answer: Arc<Mutex<Option<String>>>,
    child: Arc<Mutex<Option<std::process::Child>>>,
    /// W4：OS pid 快照——child 句柄被 try_wait 回收后仍可按 pid 清理
    /// 进程树（Windows taskkill /T、Unix killpg），不再依赖句柄存活。
    os_pid: Arc<Mutex<Option<u32>>>,
    /// W4：Exited 被后续清理 kill 覆盖为 Killed 时保留原 exit code。
    last_exit_code: Arc<Mutex<Option<i32>>>,
    /// PTY stdin writer for interactive processes.
    pty_writer: Arc<Mutex<Option<Box<dyn std::io::Write + Send>>>>,
}

/// Global process registry.
static REGISTRY: std::sync::LazyLock<Mutex<ProcessRegistry>> =
    std::sync::LazyLock::new(|| Mutex::new(ProcessRegistry::new()));

pub struct ProcessRegistry {
    entries: HashMap<u32, ProcEntry>,
    next_id: u32,
}

impl ProcessRegistry {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
        }
    }

    fn with<R>(f: impl FnOnce(&mut ProcessRegistry) -> R) -> R {
        f(&mut REGISTRY.lock().unwrap_or_else(|e| e.into_inner()))
    }

    // ── Static convenience methods ──

    /// Register a new process. Returns the assigned id.
    pub fn register(name: &str) -> u32 {
        Self::with(|r| {
            // W6：注册表只进不出会随长会话单调涨；注册前惰性驱逐
            // 终态超过 10 分钟的条目（输出快照随之释放）。
            let now = std::time::Instant::now();
            let stale: Vec<u32> = r
                .entries
                .iter()
                .filter(|(_, e)| {
                    let terminal = !matches!(
                        *e.status.lock().unwrap_or_else(|er| er.into_inner()),
                        ProcStatus::Running
                    );
                    terminal && now.duration_since(e.started).as_secs() > 600
                })
                .map(|(id, _)| *id)
                .collect();
            for id in stale {
                if let Some(mut e) = r.entries.remove(&id) {
                    *e.child.lock().unwrap_or_else(|er| er.into_inner()) = None;
                }
            }
            let id = r.next_id;
            r.next_id = r.next_id.checked_add(1).unwrap_or(u32::MAX);
            if r.next_id == u32::MAX {
                log::error!("[registry] process id space exhausted");
            }
            r.entries.insert(
                id,
                ProcEntry {
                    id,
                    name: name.to_string(),
                    status: Arc::new(Mutex::new(ProcStatus::Running)),
                    started: Instant::now(),
                    output: Arc::new(Mutex::new(String::new())),
                    stderr: Arc::new(Mutex::new(String::new())),
                    answer: Arc::new(Mutex::new(None)),
                    child: Arc::new(Mutex::new(None)),
                    os_pid: Arc::new(Mutex::new(None)),
                    last_exit_code: Arc::new(Mutex::new(None)),
                    pty_writer: Arc::new(Mutex::new(None)),
                },
            );
            id
        })
    }

    /// Attach an OS child handle to an entry.
    pub fn attach_child(id: u32, child: std::process::Child) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                let os_pid = Some(child.id());
                *entry.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                *entry.os_pid.lock().unwrap_or_else(|e| e.into_inner()) = os_pid;
            }
        });
    }

    /// 非阻塞查询子进程是否退出；已退出返回 exit code 并释放句柄、更新状态。
    /// 子进程句柄唯一持有在注册表（attach_child 移入），direct_exec 的
    /// poll 循环经此查询，避免 Child 双重持有。
    ///
    /// **终态自动更新**：检测到退出即置 `Exited`（幂等）。状态刷新不依赖
    /// 管道 EOF——孙进程可能持有管道写端导致 EOF 永不到达（如 cargo test
    /// 泄漏的后台 serve），若等 EOF 才 mark_exited，`process check/wait`
    /// 会永远显示 running。任何查询路径（exec 轮询、check、wait）经此刷新。
    pub fn try_wait(id: u32) -> Option<i32> {
        Self::with(|r| {
            let entry = r.entries.get(&id)?;
            // 终态缓存：child 句柄已释放，直接返回退出码（不再触碰句柄）
            match *entry.status.lock().unwrap_or_else(|e| e.into_inner()) {
                ProcStatus::Exited(code) => return Some(code),
                ProcStatus::Killed => return None,
                ProcStatus::Running => {}
            }
            let mut child_opt = entry.child.lock().unwrap_or_else(|e| e.into_inner());
            let child = child_opt.as_mut()?;
            match child.try_wait().ok()? {
                Some(status) => {
                    let code = status.code().unwrap_or(-1);
                    *child_opt = None;
                    *entry
                        .last_exit_code
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(code);
                    *entry.status.lock().unwrap_or_else(|e| e.into_inner()) =
                        ProcStatus::Exited(code);
                    Some(code)
                }
                None => None,
            }
        })
    }

    /// Write text to a process's PTY stdin. Returns true if the write succeeded.
    pub fn write_to(id: u32, text: &str) -> Result<usize, String> {
        let writer_arc = Self::with(|r| {
            r.entries.get(&id).and_then(|e| {
                if matches!(
                    *e.status.lock().unwrap_or_else(|e| e.into_inner()),
                    ProcStatus::Running
                ) {
                    Some(e.pty_writer.clone())
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| format!("process {id} not found or not running"))?;

        let mut guard = writer_arc.lock().map_err(|e| format!("lock: {e}"))?;
        match guard.as_mut() {
            Some(w) => {
                // W5：write_all 保证部分写不谎报全成；WouldBlock 短暂轮询
                // 直至写完或调用方超时放弃。
                let bytes = text.as_bytes();
                let mut written = 0usize;
                loop {
                    match w.write(&bytes[written..]) {
                        Ok(0) => return Err("write: zero-length write".to_string()),
                        Ok(n) => {
                            written += n;
                            if written == bytes.len() {
                                return Ok(bytes.len());
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(e) => return Err(format!("write: {e}")),
                    }
                }
            }
            None => Err(format!("process {id} has no PTY stdin (not interactive)")),
        }
    }

    /// W-low①：读取已捕获 stdout/stderr 快照（取消收集超时兜底用）。
    pub fn captured(id: u32) -> Option<(String, String)> {
        Self::with(|r| {
            r.entries.get(&id).map(|e| {
                (
                    e.output.lock().unwrap_or_else(|er| er.into_inner()).clone(),
                    e.stderr.lock().unwrap_or_else(|er| er.into_inner()).clone(),
                )
            })
        })
    }

    /// Mark a process as exited.
    pub fn mark_exited(id: u32, code: i32) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                *entry.status.lock().unwrap_or_else(|e| e.into_inner()) = ProcStatus::Exited(code);
                *entry.child.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        });
    }

    /// Set the final answer for a subagent process.
    pub fn set_answer(id: u32, answer: String) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                *entry.answer.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer);
            }
        });
    }

    /// Append stdout output to a tracked process.
    pub fn append_output(id: u32, chunk: &str) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                let mut out = entry.output.lock().unwrap_or_else(|e| e.into_inner());
                out.push_str(chunk);
                if out.chars().count() > 5000 {
                    // W-low②：按字符数裁剪（原实现字节计长+字符跳过，
                    // CJK 输出保留量最多缩水到 1/3 甚至清空）。
                    *out = crate::process_registry::char_safe_tail(out.as_str(), 4000).to_string();
                }
            }
        });
    }

    /// Append stderr output.
    pub fn append_stderr(id: u32, chunk: &str) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                let mut err = entry.stderr.lock().unwrap_or_else(|e| e.into_inner());
                err.push_str(chunk);
                if err.chars().count() > 3000 {
                    // W-low②：同上，字符口径。
                    *err = crate::process_registry::char_safe_tail(err.as_str(), 2000).to_string();
                }
            }
        });
    }

    /// Get info for a process as JSON.
    pub fn get_info(id: u32) -> Option<serde_json::Value> {
        Self::with(|r| {
            let entry = r.entries.get(&id)?;
            let status = entry
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let output = entry
                .output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let stderr = entry
                .stderr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let answer = entry
                .answer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let elapsed = entry.started.elapsed().as_secs();

            let mut info = match status {
                ProcStatus::Exited(c) => serde_json::json!({
                    "id": id, "name": entry.name, "status": "exited",
                    "exit_code": c, "elapsed_secs": elapsed,
                    "output": output, "stderr": stderr,
                }),
                ProcStatus::Killed => serde_json::json!({
                    "id": id, "name": entry.name, "status": "killed",
                    "elapsed_secs": elapsed,
                    "exit_code": *entry.last_exit_code.lock().unwrap_or_else(|e| e.into_inner()),
                    "output": output, "stderr": stderr,
                }),
                ProcStatus::Running => serde_json::json!({
                    "id": id, "name": entry.name, "status": "running",
                    "elapsed_secs": elapsed,
                    "output_tail": if output.len() > 500 {
                        format!("...({} total)\n{}", output.len(), char_safe_tail(&output, 500))
                    } else { output.clone() },
                    "stderr_tail": if stderr.len() > 300 {
                        format!("...(stderr {} total)\n{}", stderr.len(), char_safe_tail(&stderr, 300))
                    } else { stderr.clone() },
                    "output_size": output.len(),
                }),
            };
            if let Some(ans) = answer
                && let serde_json::Value::Object(ref mut map) = info
            {
                map.insert("answer".to_string(), serde_json::json!(ans));
            }
            Some(info)
        })
    }

    /// Kill a process by id（Windows：杀整棵进程树，防止后代进程泄漏管道）。
    /// W4 语义：对仍运行的进程执行整树终止并置 Killed；对已退出
    /// （Exited）的条目，仍按 os_pid 尽力清理残留后代（backgrounded
    /// 移交场景），状态同样收敛为 Killed，但原 exit code 保存在
    /// `last_exit_code` 并经 get_info 暴露——信息不再丢失。
    /// 条目不存在返回 false。
    pub fn kill(id: u32) -> bool {
        Self::with(|r| {
            let Some(entry) = r.entries.get(&id) else {
                return false;
            };
            let mut child_opt = entry.child.lock().unwrap_or_else(|e| e.into_inner());
            match child_opt.take() {
                Some(mut c) => {
                    #[cfg(windows)]
                    {
                        use std::process::Command;
                        let _ = Command::new("taskkill")
                            .args(["/pid", &c.id().to_string(), "/T", "/F"])
                            .status();
                        let _ = c.wait();
                    }
                    #[cfg(not(windows))]
                    {
                        // H6：整组 SIGKILL（spawn 侧 process_group(0)），
                        // 孙进程释放管道写端，reader 可 EOF。
                        unsafe {
                            libc::killpg(c.id() as i32, libc::SIGKILL);
                        }
                        let _ = c.wait();
                    }
                }
                None => {
                    // 句柄已被 try_wait 回收：按 os_pid 快照尽力清树。
                    if let Some(pid) = *entry.os_pid.lock().unwrap_or_else(|e| e.into_inner()) {
                        #[cfg(windows)]
                        {
                            use std::process::Command;
                            let _ = Command::new("taskkill")
                                .args(["/pid", &pid.to_string(), "/T", "/F"])
                                .status();
                        }
                        #[cfg(not(windows))]
                        {
                            unsafe {
                                libc::killpg(pid as i32, libc::SIGKILL);
                            }
                        }
                    }
                }
            }
            if let ProcStatus::Exited(code) =
                *entry.status.lock().unwrap_or_else(|e| e.into_inner())
            {
                *entry
                    .last_exit_code
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(code);
            }
            *entry.status.lock().unwrap_or_else(|e| e.into_inner()) = ProcStatus::Killed;
            true
        })
    }

    /// Wait for a process to exit (polling up to timeout_secs).
    ///
    /// 每次轮询先经 `try_wait` 刷新终态：子进程退出即返回，不依赖管道 EOF
    /// （孙进程可能持有管道写端，EOF 永不出现；原实现只查 status 字段，
    /// 而 backgrounded 路径的 mark_exited 在 EOF 后才执行 → 永远 running）。
    pub fn wait_for(id: u32, timeout_secs: u64) -> Option<serde_json::Value> {
        let start = Instant::now();
        loop {
            if start.elapsed().as_secs() > timeout_secs {
                return Self::get_info(id);
            }
            // 刷新终态（幂等；子进程已退出则自动置 Exited）
            let _ = Self::try_wait(id);
            let exited = Self::with(|r| {
                r.entries
                    .get(&id)
                    .map(|e| {
                        matches!(
                            *e.status.lock().unwrap_or_else(|e| e.into_inner()),
                            ProcStatus::Exited(_) | ProcStatus::Killed
                        )
                    })
                    .unwrap_or(true)
            });
            if exited {
                return Self::get_info(id);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}
