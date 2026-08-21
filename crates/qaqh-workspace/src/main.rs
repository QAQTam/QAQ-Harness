//! qaqh-workspace binaries — CLI tool runner + HTTP tool service.
//!
//! Usage:
//!   qaqh-workspace <tool_name> [json_args]
//!   qaqh-workspace explore
//!   qaqh-workspace read '{"path":"src/main.rs","start_line":1,"end_line":50}'
//!   qaqh-workspace list
//!   qaqh-workspace serve [--host 127.0.0.1] [--port 0] --token <secret>

use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("serve") {
        serve_main(&args[2..]);
        return;
    }
    qaqh_workspace::runtime::init_tools("cli", &[], vec![]);
    qaqh_workspace::runtime::set_context("cli", 4);
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".into());
    qaqh_workspace::workspace::set_process_workspace(&cwd);

    if args.len() < 2 {
        eprintln!("Usage: qaqh-workspace <tool> [json_args]");
        eprintln!("       qaqh-workspace list");
        std::process::exit(1);
    }

    let tool = &args[1];
    if tool == "journal" {
        std::process::exit(qaqh_workspace::journal::cli_main(&args[2..]));
    }

    if tool == "list" {
        let defs = qaqh_workspace::runtime::all_tools();
        println!("Available tools:");
        for def in &defs {
            println!("  {} — {}", def.function.name, def.function.description);
        }
        println!("\n{} tools registered", defs.len());
        return;
    }

    let json_args = args.get(2).map(|s| s.as_str()).unwrap_or("{}");
    let parsed_args: serde_json::Value = serde_json::from_str(json_args).unwrap_or_else(|_| {
        eprintln!("Error: invalid JSON args '{}'", json_args);
        std::process::exit(1);
    });

    let r = qaqh_workspace::execution::execute_with_context(
        tool,
        "",
        &parsed_args.to_string(),
        "cli_0",
        None,
    );

    println!("{}", r.result.model_text());
    if !r.result.is_success() {
        std::process::exit(1);
    }
}

/// `serve` subcommand: run the HTTP tool service.
///
/// Token 来源（按优先级）：`QAQH_WORKSPACE_TOKEN` 环境变量 > `--token` 参数。
/// daemon 用 env 传递 token，避免 secret 出现在进程命令行
/// （Windows 任意用户可经 WMIC/CIM 读取进程参数）。
/// 两者都缺失时拒绝启动（fail-closed，无匿名执行）。
fn serve_main(args: &[String]) {
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 0;
    let mut token: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--host" => {
                index += 1;
                if index < args.len() {
                    host = args[index].clone();
                }
            }
            "--port" => {
                index += 1;
                if index < args.len() {
                    port = args[index].parse().unwrap_or_else(|_| {
                        eprintln!("invalid --port: {}", args[index]);
                        std::process::exit(2);
                    });
                }
            }
            "--token" => {
                index += 1;
                if index < args.len() {
                    token = Some(args[index].clone());
                }
            }
            other => {
                eprintln!("unknown serve argument: {other}");
                eprintln!("usage: qaqh-workspace serve [--host H] [--port P] --token <secret>");
                std::process::exit(2);
            }
        }
        index += 1;
    }
    let token = match std::env::var("QAQH_WORKSPACE_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        Some(env_token) => env_token,
        None => match token {
            Some(token) if !token.is_empty() => token,
            _ => {
                eprintln!(
                    "serve requires QAQH_WORKSPACE_TOKEN env or --token <secret> (fail-closed)"
                );
                std::process::exit(2);
            }
        },
    };
    if let Err(error) = qaqh_workspace::serve::serve(&host, port, &token) {
        eprintln!("serve failed: {error}");
        std::process::exit(1);
    }
}
