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
            let id = r.next_id;
            r.next_id += 1;
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
                *entry.child.lock().unwrap() = Some(child);
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
            match *entry.status.lock().unwrap() {
                ProcStatus::Exited(code) => return Some(code),
                ProcStatus::Killed => return None,
                ProcStatus::Running => {}
            }
            let mut child_opt = entry.child.lock().unwrap();
            let child = child_opt.as_mut()?;
            match child.try_wait().ok()? {
                Some(status) => {
                    let code = status.code().unwrap_or(-1);
                    *child_opt = None;
                    *entry.status.lock().unwrap() = ProcStatus::Exited(code);
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
                if matches!(*e.status.lock().unwrap(), ProcStatus::Running) {
                    Some(e.pty_writer.clone())
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| format!("process {id} not found or not running"))?;

        let mut guard = writer_arc.lock().map_err(|e| format!("lock: {e}"))?;
        match guard.as_mut() {
            Some(w) => w.write(text.as_bytes()).map_err(|e| format!("write: {e}")),
            None => Err(format!("process {id} has no PTY stdin (not interactive)")),
        }
    }

    /// Mark a process as exited.
    pub fn mark_exited(id: u32, code: i32) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                *entry.status.lock().unwrap() = ProcStatus::Exited(code);
                *entry.child.lock().unwrap() = None;
            }
        });
    }

    /// Set the final answer for a subagent process.
    pub fn set_answer(id: u32, answer: String) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                *entry.answer.lock().unwrap() = Some(answer);
            }
        });
    }

    /// Append stdout output to a tracked process.
    pub fn append_output(id: u32, chunk: &str) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                let mut out = entry.output.lock().unwrap();
                out.push_str(chunk);
                if out.len() > 5000 {
                    let drain = out.len() - 4000;
                    *out = out.chars().skip(drain).collect();
                }
            }
        });
    }

    /// Append stderr output.
    pub fn append_stderr(id: u32, chunk: &str) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                let mut err = entry.stderr.lock().unwrap();
                err.push_str(chunk);
                if err.len() > 3000 {
                    let drain = err.len() - 2000;
                    *err = err.chars().skip(drain).collect();
                }
            }
        });
    }

    /// Get info for a process as JSON.
    pub fn get_info(id: u32) -> Option<serde_json::Value> {
        Self::with(|r| {
            let entry = r.entries.get(&id)?;
            let status = entry.status.lock().unwrap().clone();
            let output = entry.output.lock().unwrap().clone();
            let stderr = entry.stderr.lock().unwrap().clone();
            let answer = entry.answer.lock().unwrap().clone();
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
                    "output": output, "stderr": stderr,
                }),
                ProcStatus::Running => serde_json::json!({
                    "id": id, "name": entry.name, "status": "running",
                    "elapsed_secs": elapsed,
                    "output_tail": if output.len() > 500 {
                        format!("...({} total)\n{}", output.len(), &output[output.len().saturating_sub(500)..])
                    } else { output.clone() },
                    "stderr_tail": if stderr.len() > 300 {
                        format!("...(stderr {} total)\n{}", stderr.len(), &stderr[stderr.len().saturating_sub(300)..])
                    } else { stderr.clone() },
                    "output_size": output.len(),
                }),
            };
            if let Some(ans) = answer
                && let serde_json::Value::Object(ref mut map) = info {
                    map.insert("answer".to_string(), serde_json::json!(ans));
                }
            Some(info)
        })
    }

    /// Kill a process by id（Windows：杀整棵进程树，防止后代进程泄漏管道）。
    pub fn kill(id: u32) -> bool {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                let mut child_opt = entry.child.lock().unwrap();
                if let Some(mut c) = child_opt.take() {
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
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
                *entry.status.lock().unwrap() = ProcStatus::Killed;
                true
            } else {
                false
            }
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
                            *e.status.lock().unwrap(),
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
