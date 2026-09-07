use std::sync::{Arc, atomic::AtomicU64};

use crate::{ExecOutputStream, ExecProgressEvent};

use super::*;

impl Shell {
    /// 测试专用便捷封装（生产路径走 `derive_exec_args_with(cmd, None)`）。
    fn derive_exec_args(&self, command: &str) -> Vec<String> {
        self.derive_exec_args_with(command, None)
    }
}

#[test]
fn shell_from_name_resolves_known_shells() {
    assert_eq!(Shell::from_name("pwsh"), Some(Shell::PowerShell));
    assert_eq!(Shell::from_name("powershell"), Some(Shell::PowerShell));
    assert_eq!(Shell::from_name("cmd"), Some(Shell::Cmd));
    assert_eq!(Shell::from_name("zsh"), Some(Shell::Zsh));
    assert_eq!(Shell::from_name("sh"), Some(Shell::Sh));
    assert_eq!(Shell::from_name("bash"), Some(Shell::Bash));
    assert_eq!(Shell::from_name("fish"), None);
    assert_eq!(Shell::from_name(""), None);
}

#[test]
fn rg_habit_fix_argv_mode() {
    // grep 习惯组合 -rn / -rni / -rnl → rg 正确写法（-n 前缀）
    let mut argv = vec!["rg".into(), "-rn".into(), "pattern".into()];
    normalize_rg_argv(&mut argv);
    assert_eq!(argv, vec!["rg", "-n", "pattern"]);

    let mut argv = vec!["rg".into(), "-rni".into(), "pattern".into()];
    normalize_rg_argv(&mut argv);
    assert_eq!(argv, vec!["rg", "-ni", "pattern"]);

    let mut argv = vec!["rg.exe".into(), "-rnl".into()];
    normalize_rg_argv(&mut argv);
    assert_eq!(argv, vec!["rg.exe", "-nl"]);

    // 合法用法不受影响：-r 单独（--replace 等待参数）、长选项、非 rg 程序
    let mut argv = vec!["rg".into(), "-r".into(), "x".into(), "pat".into()];
    normalize_rg_argv(&mut argv);
    assert_eq!(argv, vec!["rg", "-r", "x", "pat"]);

    let mut argv = vec!["rg".into(), "--replace".into(), "x".into()];
    normalize_rg_argv(&mut argv);
    assert_eq!(argv, vec!["rg", "--replace", "x"]);

    // grep 的 -rn 是合法组合，不处理
    let mut argv = vec!["grep".into(), "-rn".into(), "pat".into()];
    normalize_rg_argv(&mut argv);
    assert_eq!(argv, vec!["grep", "-rn", "pat"]);
}

#[test]
fn rg_habit_fix_command_mode() {
    // 简单形态
    assert_eq!(
        normalize_command_rg("rg -rn \"pat\" | head"),
        "rg -n \"pat\" | head"
    );
    // 组合变体
    assert_eq!(normalize_command_rg("rg -rni foo"), "rg -ni foo");
    // 管道后的第二个 rg、Windows 可执行名
    assert_eq!(
        normalize_command_rg("rg --files | rg -rn foo"),
        "rg --files | rg -n foo"
    );
    assert_eq!(normalize_command_rg("RG.EXE -rn foo"), "RG.EXE -n foo");
    // 合法用法不受影响
    assert_eq!(normalize_command_rg("rg -r x pat"), "rg -r x pat");
    assert_eq!(
        normalize_command_rg("rg --replace x pat"),
        "rg --replace x pat"
    );
    assert_eq!(normalize_command_rg("grep -rn foo"), "grep -rn foo");
    // 无 rg 调用原样
    assert_eq!(normalize_command_rg("cargo test"), "cargo test");
}

#[test]
fn shell_derive_args_are_shell_specific() {
    // Note: on Windows the bash path may have been resolved to
    // Git-for-Windows by another test (shared DETECTED_BASH_PATH), so
    // only assert the tail of argv[0] and the fixed wrapper arguments.
    let bash = Shell::Bash.derive_exec_args("ls -la");
    assert!(
        bash[0].ends_with("bash") || bash[0].ends_with("bash.exe"),
        "argv[0]={}",
        bash[0]
    );
    assert_eq!(bash[1], "-c");
    assert_eq!(bash[2], "ls -la");

    let pwsh = Shell::PowerShell.derive_exec_args("Get-ChildItem");
    assert_eq!(
        &pwsh[..11],
        [
            "pwsh",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-InputFormat",
            "Text",
            "-OutputFormat",
            "Text",
            "-EncodedCommand"
        ]
    );
    assert_eq!(pwsh.len(), 12);
    // 验证 Base64(UTF-16LE) 可逆且与 ps_encode 一致
    assert_eq!(pwsh[11], ps_encode("Get-ChildItem"));
    let decoded = {
        let bytes = base64_decode(&pwsh[11]).expect("valid base64");
        let utf16: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16(&utf16).expect("valid utf16le")
    };
    assert_eq!(decoded, "Get-ChildItem");

    let cmd = Shell::Cmd.derive_exec_args("dir");
    assert_eq!(&cmd[..2], ["cmd", "/c"]);
    assert_eq!(cmd[2], "dir");
}

#[test]
fn pwsh_command_with_args_uses_command_with_args() {
    let args = vec![
        "arg1".to_string(),
        "hello world".to_string(),
        "a\"b".to_string(),
    ];
    let pwsh = Shell::PowerShell
        .derive_exec_args_with("Write-Output $args[0]; Write-Output $args[1]", Some(&args));
    assert_eq!(
        &pwsh[..12],
        [
            "pwsh",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-InputFormat",
            "Text",
            "-OutputFormat",
            "Text",
            "-CommandWithArgs",
            "Write-Output $args[0]; Write-Output $args[1]"
        ]
    );
    assert_eq!(&pwsh[12..], &args);
    assert_eq!(pwsh.len(), 15);
    // 空 args 应回退到 EncodedCommand
    let pwsh2 = Shell::PowerShell.derive_exec_args_with("Get-ChildItem", Some(&[]));
    assert!(pwsh2.contains(&"-EncodedCommand".to_string()));
    assert!(!pwsh2.contains(&"-CommandWithArgs".to_string()));
}

#[test]
fn pwsh_tool_with_args_executes_via_command_with_args() {
    if !shell_available(Shell::PowerShell) {
        eprintln!("skipping: powershell not available on this machine");
        return;
    }
    let ctx = make_ctx(
        "pwsh",
        serde_json::json!({ "command": "$args | % { \"arg: $_\" }", "args": ["hello world", "a\"b"], "cwd": std::env::current_dir().unwrap() }),
    );
    let r = handle_run_pwsh(ctx);
    assert!(r.is_success(), "model text: {}", r.model_text());
    // model_text 是 ExecOutput 的 JSON，output 字段内才是原始 stdout；需解析后检查
    let v: serde_json::Value = serde_json::from_str(r.model_text()).expect("valid json");
    let output = v.get("output").and_then(|x| x.as_str()).unwrap_or("");
    assert!(output.contains("hello world"), "output: {output}");
    assert!(output.contains("a\"b"), "output: {output}");
}

#[test]
fn pwsh_tool_with_chinese_args_via_command_with_args() {
    if !shell_available(Shell::PowerShell) {
        eprintln!("skipping: powershell not available on this machine");
        return;
    }
    let ctx = make_ctx(
        "pwsh",
        serde_json::json!({ "command": "Write-Output $args[0]", "args": ["中文测试"], "cwd": std::env::current_dir().unwrap() }),
    );
    let r = handle_run_pwsh(ctx);
    assert!(r.is_success(), "model text: {}", r.model_text());
    assert!(
        r.model_text().contains("中文测试"),
        "output: {}",
        r.model_text()
    );
}
#[test]
fn test_git_status_returns_output() {
    let argv = vec!["git".to_string(), "status".to_string()];
    let result = direct_exec(&argv, None, None, 10000, 10, None, None, None, "test");
    eprintln!(
        "exit_code={:?} timed_out={}",
        result.exit_code, result.timed_out
    );
    assert!(!result.timed_out, "timed out");
    assert!(!result.output.is_empty(), "no output");
}

#[test]
fn test_git_diff_returns_output() {
    let argv = vec!["git".to_string(), "diff".to_string(), "--stat".to_string()];
    let result = direct_exec(&argv, None, None, 10000, 10, None, None, None, "test");
    eprintln!(
        "exit_code={:?} timed_out={}",
        result.exit_code, result.timed_out
    );
    assert!(!result.timed_out, "timed out");
}

#[test]
fn test_cargo_check_returns_output() {
    let argv = vec![
        "cargo".to_string(),
        "check".to_string(),
        "-p".to_string(),
        "qaqh-types".to_string(),
    ];
    let result = direct_exec(&argv, None, None, 10000, 60, None, None, None, "test");
    eprintln!(
        "exit_code={:?} timed_out={}",
        result.exit_code, result.timed_out
    );
    assert!(!result.timed_out, "timed out");
    assert!(!result.output.is_empty(), "no output");
}

#[cfg(windows)]
#[test]
fn per_call_cancel_stops_only_the_running_command() {
    let argv = vec![
        "cmd".to_string(),
        "/C".to_string(),
        "ping -n 6 127.0.0.1 >NUL".to_string(),
    ];
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        signal.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    let result = direct_exec(
        &argv,
        None,
        None,
        100,
        10,
        None,
        Some(cancel.as_ref()),
        None,
        "test",
    );
    assert!(
        result.cancelled,
        "per-call cancellation should stop the child"
    );
}

#[cfg(not(windows))]
#[test]
fn per_call_cancel_stops_only_the_running_command() {
    let argv = vec!["sleep".to_string(), "6".to_string()];
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        signal.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    let result = direct_exec(
        &argv,
        None,
        None,
        100,
        10,
        None,
        Some(cancel.as_ref()),
        None,
        "test",
    );
    assert!(
        result.cancelled,
        "per-call cancellation should stop the child"
    );
}

/// 2026-09-02 冻结事故回归（P0-1）：孙进程持有管道写端时，子进程退出后
/// EOF 永不出现；收集必须在有界预算内以注册表快照完成，不得依赖 EOF。
/// 旧实现 recv_timeout(2s)×2 在此场景固定多等 ~4s（事故 audit 实测 +2.0s）。
#[cfg(not(windows))]
#[test]
fn grandchild_holding_pipe_write_end_collects_bounded() {
    let argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        "sleep 5 & echo GRANDCHILD-HOLDS-PIPE".to_string(),
    ];
    let start = std::time::Instant::now();
    let result = direct_exec(&argv, None, None, 10000, 10, None, None, None, "test");
    let elapsed = start.elapsed();
    assert!(
        result.output.contains("GRANDCHILD-HOLDS-PIPE"),
        "registry snapshot must retain child output: {}",
        result.output
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "collect must not wait for grandchild-held pipe EOF, took {elapsed:?}"
    );
}

/// 阶段 2（1.3 驻留治愈）回归：孙进程持有管道写端时，读线程必须在
/// 有界时间内退出并 drop progress sender。旧实现读线程永卡 read()，
/// sender 永不释放（drain 的 Disconnected 快路径永不触发）。
#[cfg(not(windows))]
#[test]
fn reader_threads_terminate_after_grandchild_settle_even_without_eof() {
    let argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        "echo settled; sleep 5 &".to_string(),
    ];
    let (tx, rx) = crate::bounded_exec_progress_channel();
    let start = std::time::Instant::now();
    let _result = direct_exec(&argv, None, None, 10000, 10, None, None, Some(tx), "test");
    let deadline = start + std::time::Duration::from_secs(5);
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "读线程必须在 settle 预算内退出（sender drop → Disconnected）"
                );
            }
        }
    }
}

/// 阶段 2（2.1 契约）回归：seal 的权威源是注册表**完整**捕获。
/// 12000+ 字节输出远超 tail 视图的 4000 字符裁剪线；孙进程持写端时
/// 旧兜底快照只剩尾部（数据损失），captured_full 必须首尾俱全。
#[cfg(not(windows))]
#[test]
fn seal_uses_full_registry_capture_not_tail_when_grandchild_holds_pipe() {
    let argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        "seq 1 3000; sleep 5 &".to_string(),
    ];
    let result = direct_exec(&argv, None, None, 100000, 10, None, None, None, "test");
    assert!(
        result.output.contains("3000"),
        "末行必须存活: {:?}",
        result.output.get(..200)
    );
    assert!(
        result.output.lines().count() >= 2999,
        "3000 行输出不得被 tail 裁剪，实际 {} 行",
        result.output.lines().count()
    );
}

/// Windows 等效回归：`start /b` 在同一控制台派生后台子进程并继承管道写端。
#[cfg(windows)]
#[test]
fn grandchild_holding_pipe_write_end_collects_bounded() {
    let argv = vec![
        "cmd".to_string(),
        "/C".to_string(),
        "start /b cmd /c \"timeout /t 5 >NUL\" & echo GRANDCHILD-HOLDS-PIPE".to_string(),
    ];
    let start = std::time::Instant::now();
    let result = direct_exec(&argv, None, None, 10000, 10, None, None, None, "test");
    let elapsed = start.elapsed();
    assert!(
        result.output.contains("GRANDCHILD-HOLDS-PIPE"),
        "registry snapshot must retain child output: {}",
        result.output
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "collect must not wait for grandchild-held pipe EOF, took {elapsed:?}"
    );
}

#[test]
fn truncated_output_instructs_the_model_to_retry_narrowly() {
    let text = "token ".repeat(1_000);
    let truncated = token_truncate(&text, 10);
    assert!(truncated.contains("Call exec again with narrower argv or a filtering command."));
}

#[test]
fn pipe_reader_forwards_retained_chunks_with_the_call_id() {
    let (tx, rx) = crate::bounded_exec_progress_channel();
    // registry-native 阶段 2：输出权威源在注册表完整捕获（不再由读线程
    // 返回汇总），读线程只回报 (saw_eof, capped) 生命周期信号。
    let proc_id = crate::process_registry::ProcessRegistry::register("reader-test");
    let mut stream = std::io::Cursor::new(b"first\nsecond\n".to_vec());
    let ctx = PipePumpCtx {
        progress_tx: Some(tx),
        tool_call_id: "call-stream-1".to_string(),
        output_stream: ExecOutputStream::Stdout,
        progress_seq: Arc::new(AtomicU64::new(0)),
        registry_id: proc_id,
    };
    let (saw_eof, capped) =
        drain_pipe_to_registry(&mut stream, 1024, &ctx, &mut |_s: &mut std::io::Cursor<
            Vec<u8>,
        >| {
            Ok(Readiness::Ready)
        });

    let chunks: Vec<_> = rx.try_iter().collect();
    let (full_out, _) = crate::process_registry::ProcessRegistry::captured_full(proc_id)
        .expect("registry entry must exist");
    assert_eq!(full_out, "first\nsecond\n");
    assert!(saw_eof, "Cursor 读尽即 EOF");
    assert!(!capped);
    assert_eq!(
        chunks,
        vec![ExecProgressEvent {
            tool_call_id: "call-stream-1".to_string(),
            stream: ExecOutputStream::Stdout,
            seq: 0,
            chunk: "first\nsecond\n".to_string(),
        }]
    );
}

#[cfg(windows)]
#[test]
fn exec_forwards_stdout_to_the_progress_channel_before_returning() {
    let argv = vec![
        "cmd".to_string(),
        "/C".to_string(),
        "echo streamed-output".to_string(),
    ];
    let (tx, rx) = crate::bounded_exec_progress_channel();

    let result = direct_exec(
        &argv,
        None,
        None,
        100,
        10,
        None,
        None,
        Some(tx),
        "call-stream-2",
    );
    let chunks: Vec<_> = rx.try_iter().collect();

    assert!(result.output.contains("streamed-output"));
    assert!(chunks.iter().any(|event| {
        event.tool_call_id == "call-stream-2"
            && event.stream == ExecOutputStream::Stdout
            && event.chunk.contains("streamed-output")
    }));
}

#[test]
fn pipe_reader_keeps_split_utf8_characters_intact_for_the_ui() {
    let (tx, rx) = crate::bounded_exec_progress_channel();
    let mut input = vec![b'a'; 8191];
    input.extend_from_slice("中".as_bytes());
    let proc_id = crate::process_registry::ProcessRegistry::register("utf8-reader-test");
    let mut stream = std::io::Cursor::new(input);
    let ctx = PipePumpCtx {
        progress_tx: Some(tx),
        tool_call_id: "utf8".to_string(),
        output_stream: ExecOutputStream::Stdout,
        progress_seq: Arc::new(AtomicU64::new(0)),
        registry_id: proc_id,
    };
    let (saw_eof, capped) = drain_pipe_to_registry(
        &mut stream,
        16 * 1024,
        &ctx,
        &mut |_s: &mut std::io::Cursor<Vec<u8>>| Ok(Readiness::Ready),
    );
    assert!(saw_eof);
    assert!(!capped);
    let (full_out, _) = crate::process_registry::ProcessRegistry::captured_full(proc_id)
        .expect("registry entry must exist");
    assert!(full_out.ends_with('中'));
    assert!(!full_out.contains('\u{fffd}'));
    let text: String = rx.try_iter().map(|event| event.chunk).collect();
    assert!(text.ends_with('中'));
    assert!(!text.contains('\u{fffd}'));
}

#[cfg(windows)]
#[test]
fn windows_oem_output_is_decoded_without_utf8_beta_mode() {
    // GBK/936 for "正在", representative of cmd.exe ping output.
    assert_eq!(
        decode_windows_oem(&[0xD5, 0xFD, 0xD4, 0xDA]),
        Some("正在".to_string())
    );
}

#[test]
fn bounded_progress_queue_drops_updates_without_blocking_pipe_readers() {
    let (tx, _rx) = crate::bounded_exec_progress_channel();
    for seq in 0..=crate::EXEC_PROGRESS_CHANNEL_CAPACITY {
        tx.try_send(ExecProgressEvent {
            tool_call_id: "bounded".to_string(),
            stream: ExecOutputStream::Stdout,
            seq: seq as u64,
            chunk: "x".to_string(),
        });
    }
    assert_eq!(tx.dropped_bytes(), 1);
}

/// 阶段 2（报告 P1）回归：process wait 阻塞期间收到取消旗标必须立即
/// 返回，不得阻塞到 timeout_secs（非 exec 阻塞工具的飞行中取消）。
#[test]
fn wait_for_returns_promptly_on_per_call_cancel() {
    let id = crate::process_registry::ProcessRegistry::register("wait-cancel-test");
    // 无 child 的条目永远 Running——旧实现会在此阻塞满 timeout_secs。
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        signal.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let start = std::time::Instant::now();
    let info = crate::process_registry::ProcessRegistry::wait_for(id, 30, Some(&cancel))
        .expect("wait_for 必须返回");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "取消必须立即打破 wait_for 阻塞"
    );
    assert_eq!(info["wait_interrupted_by_cancel"], serde_json::json!(true));
}

/// Linux 侧 3.2 冒烟：工具层全生命周期（真 handler 调用，非 direct_exec 直调）。
/// 前台执行 → background_after_secs 快速移交 → 注册表 check → kill → 终态。
/// 验证的是 bash 工具 → handle_run_with_shell → direct_exec → registry 的完整链路。
#[cfg(not(windows))]
#[test]
fn bash_tool_full_lifecycle_smoke_foreground_handoff_kill() {
    if !shell_available(Shell::Bash) {
        eprintln!("skipping: bash not available");
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    // ① 前台：echo 经完整 tool 层（spawn → poll → seal → 汇聚）
    let ctx = make_ctx(
        "bash",
        serde_json::json!({ "command": "echo SMOKE-FOREGROUND-OK", "cwd": cwd }),
    );
    let r = handle_run_bash(ctx);
    assert!(r.is_success(), "foreground: {}", r.model_text());
    assert!(r.model_text().contains("SMOKE-FOREGROUND-OK"));

    // ② 快速移交：30s 长任务 + 1s 观察窗 → backgrounded + process_id
    let ctx = make_ctx(
        "bash",
        serde_json::json!({ "command": "sleep 30", "cwd": cwd, "background_after_secs": 1 }),
    );
    let r = handle_run_bash(ctx);
    let v: serde_json::Value =
        serde_json::from_str(r.model_text()).expect("bash 工具结果必须是 ExecOutput JSON");
    assert_eq!(v["status"], "backgrounded", "移交状态: {v}");
    let pid = v["process_id"].as_u64().expect("移交必须携带 process_id") as u32;

    // ③ check：running（try_wait 刷新终态，不依赖管道 EOF）
    let info =
        crate::process_registry::ProcessRegistry::get_info(pid).expect("移交后条目必须在注册表");
    assert_eq!(info["status"], "running");

    // ④ kill 整树 + 终态收敛（killpg 组杀：bash 与 sleep 同组）
    assert!(
        crate::process_registry::ProcessRegistry::kill(pid),
        "kill 应成功"
    );
    let after = crate::process_registry::ProcessRegistry::get_info(pid).expect("仍被跟踪");
    assert_eq!(after["status"], "killed");

    // ⑤ wait_for 对 Killed 终态立即返回（有界，不空等到 timeout）
    let final_info = crate::process_registry::ProcessRegistry::wait_for(pid, 10, None)
        .expect("wait_for 必须返回");
    assert_eq!(final_info["status"], "killed");
}

/// 观测线纪律：后台派生检测的判定口径。
#[test]
fn background_derivation_detection_boundaries() {
    assert!(detect_background_derivation("nohup ./x run > log 2>&1 &"));
    assert!(detect_background_derivation("sleep 30 &"));
    assert!(detect_background_derivation("a & b"));
    // 逻辑与、重定向组合不得误报
    assert!(!detect_background_derivation("cargo test && cargo clippy"));
    assert!(!detect_background_derivation("cmd >f 2>&1"));
    assert!(!detect_background_derivation("cmd &>f"));
    assert!(!detect_background_derivation("echo ok"));
}

/// 工具层 e2e：后台派生命令的前台结果携带强提示（模型可见）。
/// `sleep 1 &` 同时覆盖"孙进程持写端 → settle 有界封口"路径。
#[cfg(not(windows))]
#[test]
fn bash_tool_appends_background_derivation_hint() {
    if !shell_available(Shell::Bash) {
        eprintln!("skipping: bash not available");
        return;
    }
    let ctx = make_ctx(
        "bash",
        serde_json::json!({
            "command": "sleep 1 & echo HINT-E2E",
            "cwd": std::env::current_dir().unwrap()
        }),
    );
    let r = handle_run_bash(ctx);
    assert!(r.is_success(), "result: {}", r.model_text());
    assert!(r.model_text().contains("HINT-E2E"));
    assert!(
        r.model_text().contains("[!] 后台派生检测"),
        "强提示必须随结果输出"
    );
}

#[test]
fn shell_detect_finds_available_shell() {
    let shell = Shell::detect();
    let path = shell.path();
    // Verify the detected shell binary actually exists
    let status = std::process::Command::new(path)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    assert!(
        status.is_ok(),
        "detected shell '{path}' should be runnable (got {:?})",
        shell
    );
}

#[test]
fn command_mode_uses_detected_shell() {
    // 默认检测的 shell（Windows=pwsh / Unix=bash）应可运行 command 模式
    let argv = Shell::detect().derive_exec_args("echo hello-from-shell");
    let result = direct_exec(&argv, None, None, 100, 10, None, None, None, "shell-test");
    assert_eq!(
        result.exit_code,
        Some(0),
        "shell exec failed: {}",
        result.output
    );
    // Should output "hello-from-shell" from the echo command
    assert!(
        result.output.contains("hello-from-shell"),
        "expected 'hello-from-shell' in output, got: '{}'",
        result.output
    );
}

#[cfg(windows)]
#[test]
fn explicit_bash_shell_resolves_git_bash() {
    // 模型显式传 shell: bash 时（Windows）应解析到可运行 bash，
    // 且 POSIX 管道语义可用。
    let argv = Shell::from_name("bash")
        .expect("bash name resolves")
        .derive_exec_args("echo posix-ok | tr a-z A-Z");
    let result = direct_exec(&argv, None, None, 100, 10, None, None, None, "bash-test");
    assert_eq!(
        result.exit_code,
        Some(0),
        "bash exec failed: {}",
        result.output
    );
    assert!(
        result.output.contains("POSIX-OK"),
        "bash pipeline output missing marker: {}",
        result.output
    );
}

#[test]
fn shell_discovery_does_not_execute_path_candidates() {
    let root = std::env::temp_dir().join(format!("qaqh-exec-shell-probe-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    #[cfg(windows)]
    let candidate = root.join("probe-shell.exe");
    #[cfg(not(windows))]
    let candidate = root.join("probe-shell");
    #[cfg(windows)]
    std::fs::write(&candidate, b"not an executable").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&candidate, b"#!/bin/sh\n: > \"$0.ran\"\n").unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    assert!(executable_in_dirs(
        "probe-shell",
        std::iter::once(root.clone())
    ));
    assert!(!root.join("probe-shell.ran").exists());

    let _ = std::fs::remove_file(candidate);
    let _ = std::fs::remove_dir(root);
}

#[cfg(windows)]
#[test]
fn timeout_transfers_process_to_background_registry() {
    // 8 秒 sleep，超时 3 秒 → 移交后台（不 kill）。
    // 用 PowerShell Start-Sleep（无孙进程，避免句柄继承干扰）。
    let argv = vec![
        "powershell".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Seconds 8; Write-Output done".to_string(),
    ];
    let result = direct_exec(&argv, None, None, 100, 3, None, None, None, "bg-test");
    assert!(result.timed_out, "应超时");
    assert_eq!(result.status, "backgrounded", "超时 = 移交后台");
    let pid = result.process_id.expect("移交必须携带 process_id");
    // 进程存活于注册表（running）
    let info = crate::process_registry::ProcessRegistry::get_info(pid).expect("进程必须在注册表");
    assert_eq!(info["status"], "running", "移交后进程不得被杀");
    assert!(
        result.output.contains("process(action="),
        "提示应指向 process 检查动作"
    );
    // process(wait) 语义：等待自然退出
    let final_info = crate::process_registry::ProcessRegistry::wait_for(pid, 15, None)
        .expect("wait_for 必须返回");
    eprintln!("final_info: {final_info}");
    assert_eq!(final_info["status"], "exited", "ping 自然结束后应为 exited");
    // 输出已逐 chunk 追加到注册表（backgrounded 期间也累积）
    assert!(final_info["output"].is_string() || final_info.get("output_tail").is_some());
}

#[cfg(windows)]
#[test]
fn backgrounded_process_check_sees_running_then_kill_tree() {
    // cmd /C 生成孙进程树（ping 8 秒）；超时 2 秒移交
    let argv = vec![
        "cmd".to_string(),
        "/C".to_string(),
        "ping -n 8 127.0.0.1 >NUL".to_string(),
    ];
    let result = direct_exec(&argv, None, None, 100, 2, None, None, None, "bg-kill");
    let pid = result.process_id.expect("process_id");
    assert_eq!(
        crate::process_registry::ProcessRegistry::get_info(pid).unwrap()["status"],
        "running"
    );
    // 注册表 kill = 进程树终止
    assert!(
        crate::process_registry::ProcessRegistry::kill(pid),
        "kill 应成功"
    );
    let after = crate::process_registry::ProcessRegistry::get_info(pid).expect("still tracked");
    assert_eq!(after["status"], "killed");
}

#[cfg(windows)]
#[test]
fn background_after_secs_handoff_before_timeout() {
    // 长驻进程（8 秒 sleep），timeout 设 60 秒，但 background_after_secs=3
    // → 3 秒即移交后台，而不是死等到 60 秒（验证 agent loop 不阻塞）。
    let argv = vec![
        "powershell".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Seconds 8; Write-Output done".to_string(),
    ];
    let started = std::time::Instant::now();
    let result = direct_exec(&argv, None, None, 100, 60, Some(3), None, None, "bg-fast");
    let elapsed = started.elapsed().as_secs_f64();
    assert!(result.timed_out, "观察窗口到期应移交");
    assert_eq!(result.status, "backgrounded");
    assert!(
        elapsed < 10.0,
        "移交必须远早于 timeout=60s，实际 {elapsed}s"
    );
    let pid = result.process_id.expect("移交必须携带 process_id");
    let info = crate::process_registry::ProcessRegistry::get_info(pid).expect("in registry");
    assert_eq!(info["status"], "running", "移交后进程存活");
    assert!(
        result.output.contains("transferred_after_secs"),
        "backgrounded 输出应包含移交耗时字段"
    );
    // 清理：等待自然退出（8 秒 sleep 早已结束）
    let final_info = crate::process_registry::ProcessRegistry::wait_for(pid, 15, None)
        .expect("wait_for 必须返回");
    assert_eq!(final_info["status"], "exited");
}

#[cfg(windows)]
#[test]
fn backgrounded_status_refreshes_when_child_exits_while_grandchild_holds_pipe() {
    // 复现用户场景（cargo test 通过后孙进程未回收）：
    // cmd /C 先 spawn 后台孙进程（ping 6 秒，继承 exec 管道写端），
    // 子进程自身 ping 2 秒后退出。孙进程持有管道 → EOF 永不到达。
    // 修复前：状态停在 running（mark_exited 只在 EOF 后执行），
    // process check/wait 误以为任务未结束。
    // 修复后：try_wait 感知子进程退出即刷新为 exited。
    let argv = vec![
        "cmd".to_string(),
        "/C".to_string(),
        "start /b cmd /c ping -n 6 127.0.0.1 >NUL & ping -n 2 127.0.0.1 >NUL & exit 0".to_string(),
    ];
    let result = direct_exec(
        &argv,
        None,
        None,
        100,
        15,
        Some(1),
        None,
        None,
        "bg-grandchild",
    );
    assert_eq!(result.status, "backgrounded", "1 秒观察窗到期应移交");
    let pid = result.process_id.expect("process_id");

    // 子进程约 2 秒退出；孙进程（ping 6 秒）继续持有管道
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut status = String::new();
    while std::time::Instant::now() < deadline {
        let _ = crate::process_registry::ProcessRegistry::try_wait(pid);
        status = crate::process_registry::ProcessRegistry::get_info(pid)
            .map(|i| i["status"].as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        if status == "exited" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(
        status, "exited",
        "子进程退出后状态必须刷新（不依赖孙进程管道 EOF）"
    );

    // 清理：kill 进程树（孙进程仍活着），验证整树终止
    assert!(
        crate::process_registry::ProcessRegistry::kill(pid),
        "kill 应成功"
    );
    let after = crate::process_registry::ProcessRegistry::get_info(pid).expect("still tracked");
    assert_eq!(after["status"], "killed");
}

// ── 独立 shell 工具（4.2：bash / pwsh）──

fn make_ctx(name: &str, args: serde_json::Value) -> crate::ToolCallCtx {
    crate::ToolCallCtx {
        id: "exec-test".into(),
        name: name.into(),
        action: String::new(),
        args,
        tx_progress: None,
        timeout_secs: Some(30),
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        skill_effects: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

#[test]
fn shell_tools_registered_without_shell_param() {
    let mut mgr = crate::ToolManager::new();
    register_shell_tool(&mut mgr, "bash", Shell::Bash, handle_run_bash);
    register_shell_tool(&mut mgr, "pwsh", Shell::PowerShell, handle_run_pwsh);
    let defs = mgr.all_defs();
    assert_eq!(defs.len(), 2);
    for d in &defs {
        let props = d.function.parameters.get("properties").unwrap();
        assert!(
            props.get("shell").is_none(),
            "{} must NOT expose a shell param (tool name is the shell)",
            d.function.name
        );
        assert!(props.get("command").is_some());
        assert!(props.get("argv").is_some());
    }
    let bash_desc = &defs
        .iter()
        .find(|d| d.function.name == "bash")
        .unwrap()
        .function
        .description;
    assert!(bash_desc.contains("bash"), "desc: {bash_desc}");
    let pwsh_desc = &defs
        .iter()
        .find(|d| d.function.name == "pwsh")
        .unwrap()
        .function
        .description;
    assert!(pwsh_desc.contains("pwsh"), "desc: {pwsh_desc}");
}

#[test]
fn bash_tool_executes_command_through_fixed_shell() {
    if !shell_available(Shell::Bash) {
        eprintln!("skipping: bash not available on this machine");
        return;
    }
    // cwd 显式传当前目录：并行测试会污染 CURRENT_WORKSPACE（可能指向已删除
    // 的 tempdir），不传则 spawn 带无效 cwd → os error 267。
    let ctx = make_ctx(
        "bash",
        serde_json::json!({ "command": "echo shell-tool-ok", "cwd": std::env::current_dir().unwrap() }),
    );
    let r = handle_run_bash(ctx);
    assert!(r.is_success(), "model text: {}", r.model_text());
    assert!(r.model_text().contains("shell-tool-ok"));
}

#[test]
fn pwsh_tool_executes_command_through_fixed_shell() {
    if !shell_available(Shell::PowerShell) {
        eprintln!("skipping: powershell not available on this machine");
        return;
    }
    let ctx = make_ctx(
        "pwsh",
        serde_json::json!({ "command": "Write-Output shell-tool-ok", "cwd": std::env::current_dir().unwrap() }),
    );
    let r = handle_run_pwsh(ctx);
    assert!(r.is_success(), "model text: {}", r.model_text());
    assert!(r.model_text().contains("shell-tool-ok"));
}

#[test]
fn bash_tool_argv_mode_still_direct_exec() {
    // argv 模式与 shell 无关：bash 工具也能直跑程序（无包装）。
    #[cfg(windows)]
    let argv = serde_json::json!(["cmd", "/c", "echo", "shell-tool-ok"]);
    #[cfg(not(windows))]
    let argv = serde_json::json!(["echo", "shell-tool-ok"]);
    let ctx = make_ctx(
        "bash",
        serde_json::json!({ "argv": argv, "cwd": std::env::current_dir().unwrap() }),
    );
    let r = handle_run_bash(ctx);
    assert!(r.is_success(), "model text: {}", r.model_text());
}
