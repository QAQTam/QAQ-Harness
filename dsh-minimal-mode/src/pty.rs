//! Line-based scrollback buffer and a portable-pty backed persistent shell
//! session, mirroring the minimal-mode terminal seam used by the bash tool.

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Default PTY size (rows x cols) used when spawning the persistent shell.
const PTY_ROWS: u16 = 24;
const PTY_COLS: u16 = 120;

/// Resolve the bash executable used for the persistent shell. On Windows we
/// prefer a real Git-for-Windows / MSYS2 bash over the WSL wrapper.
fn bash_path() -> String {
    #[cfg(windows)]
    {
        const CANDIDATES: &[&str] = &[
            "C:\\Program Files\\Git\\bin\\bash.exe",
            "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
            "C:\\msys64\\usr\\bin\\bash.exe",
        ];
        for candidate in CANDIDATES {
            if std::path::Path::new(candidate).is_file() {
                return candidate.to_string();
            }
        }
    }
    "bash".to_string()
}

/// A bounded, line-oriented scrollback. Mirrors `terminals.read` paging
/// (offset/count, lineEnd, totalLines, truncated) used by minimal-mode.
#[derive(Debug, Default)]
pub struct Scrollback {
    lines: Vec<String>,
    partial: String,
    truncated: bool,
}

pub struct Page {
    pub text: String,
    pub line_end: usize,
    pub total_lines: usize,
    pub truncated: bool,
}

impl Scrollback {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append raw PTY bytes. `\r\n` and `\n` terminate a line; a bare `\r`
    /// is dropped (TERM=dumb, stty -echo output).
    pub fn append(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\r' => {}
                '\n' => {
                    self.lines.push(std::mem::take(&mut self.partial));
                }
                other => self.partial.push(other),
            }
        }
    }

    /// Number of completed lines.
    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    /// Full retained text: completed lines joined by `\n`, plus any partial.
    pub fn full_text(&self) -> String {
        let mut out = self.lines.join("\n");
        if !self.partial.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&self.partial);
        }
        out
    }

    /// Read a page starting at `offset` for up to `count` completed lines.
    pub fn read(&self, offset: usize, count: usize) -> Page {
        let total = self.lines.len();
        let end = (offset + count).min(total);
        let text = if offset >= total {
            String::new()
        } else {
            self.lines[offset..end].join("\n")
        };
        Page {
            text,
            line_end: end,
            total_lines: total,
            truncated: self.truncated,
        }
    }
}

/// One live persistent PTY shell session (owner-scoped).
pub struct PtySession {
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    scrollback: Arc<Mutex<Scrollback>>,
    exited: AtomicBool,
    /// `None` while running; `Some(Some(code))` / `Some(None)` after exit.
    exit: Mutex<Option<Option<i32>>>,
}

impl PtySession {
    /// Spawn `bash` with the given working directory and start a reader thread
    /// that drains output into the scrollback buffer.
    pub fn spawn(cwd: &str) -> Result<Arc<Self>, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty failed: {e}"))?;

        let mut cmd = CommandBuilder::new(&bash_path());
        if !cwd.is_empty() {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "dumb");
        cmd.env("PAGER", "cat");
        cmd.env("GIT_PAGER", "cat");
        cmd.env("PROMPT_COMMAND", "");
        cmd.env("BASH_SILENCE_DEPRECATION_WARNING", "1");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn bash failed: {e}"))?;
        drop(pair.slave);

        let master = pair.master;
        let reader = master
            .try_clone_reader()
            .map_err(|e| format!("clone pty reader failed: {e}"))?;
        let writer = master
            .take_writer()
            .map_err(|e| format!("take pty writer failed: {e}"))?;

        let scrollback = Arc::new(Mutex::new(Scrollback::new()));

        let session = Arc::new(Self {
            child: Mutex::new(Some(child)),
            writer: Mutex::new(writer),
            scrollback: scrollback.clone(),
            exited: AtomicBool::new(false),
            exit: Mutex::new(None),
        });

        // Reader thread: drain PTY output into the scrollback. The master is
        // moved here and held until EOF so the PTY stays open for the session.
        let session_reader = session.clone();
        std::thread::Builder::new()
            .name("dsh-minimal-pty-reader".into())
            .spawn(move || {
                let _master_keepalive = master;
                let mut reader = reader;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                            session_reader.scrollback.lock().unwrap().append(&text);
                            // Minimal terminal-emulator response: bash's readline
                            // queries the cursor position (ESC[6n) and waits; reply
                            // with cursor at row 1, col 1 (ESC[1;1R).
                            if text.contains("\x1b[6n") {
                                let _ = session_reader.send_bytes(b"\x1b[1;1R");
                            }
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::Interrupted {
                                log::warn!("[dsh-minimal] pty read error: {e}");
                                break;
                            }
                        }
                    }
                }
                session_reader.exited.store(true, Ordering::SeqCst);
            })
            .map_err(|e| format!("spawn pty reader thread failed: {e}"))?;

        Ok(session)
    }

    /// Write a line to the PTY (echo-free because of stty -echo).
    pub fn send_line(&self, line: &str) -> Result<(), String> {
        self.send_bytes(line.as_bytes())?;
        self.send_bytes(b"\n")
    }

    /// Write raw bytes to the PTY master.
    pub fn send_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(bytes)
            .and_then(|_| writer.flush())
            .map_err(|e| format!("write to pty failed: {e}"))
    }

    pub fn scrollback(&self) -> Arc<Mutex<Scrollback>> {
        self.scrollback.clone()
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Query child exit status. Returns `Some(Some(code))` when the child has
    /// exited with a code, `Some(None)` when it exited without a code, and
    /// `None` while still running.
    pub fn try_exit_code(&self) -> Option<Option<i32>> {
        if let Some(cached) = *self.exit.lock().unwrap() {
            return Some(cached);
        }
        let mut guard = self.child.lock().unwrap();
        let child = guard.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.exit_code() as i32;
                *self.exit.lock().unwrap() = Some(Some(code));
                Some(Some(code))
            }
            _ => None,
        }
    }

    pub fn kill(&self) {
        let mut guard = self.child.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *guard = None;
    }
}

/// Read the latest `count` completed lines as a page (offset 0).
pub fn read_page(scrollback: &Mutex<Scrollback>, count: usize) -> Page {
    scrollback.lock().unwrap().read(0, count)
}

/// Assemble the full retained scrollback (joining page text, mirroring
/// `retainedScrollback` which pages from offset 0 and joins with `\n`).
pub fn retained_text(scrollback: &Mutex<Scrollback>) -> String {
    scrollback.lock().unwrap().full_text()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_echo_roundtrip() {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let session = PtySession::spawn(&cwd).expect("spawn");
        session
            .send_line("printf '%s\\n' MARKER_123")
            .expect("send");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let text = retained_text(&session.scrollback());
            if text.contains("MARKER_123") {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for pty output; got: {text:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        session.kill();
    }
}
