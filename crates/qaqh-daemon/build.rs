use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=QAQH_BUILD_ID");
    println!("cargo:rerun-if-env-changed=QAQH_CHANNEL");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../../../qaqh-webui/src");
    println!("cargo:rerun-if-changed=../../../qaqh-webui/index.html");
    println!("cargo:rerun-if-changed=../../../qaqh-webui/package.json");

    ensure_webui_embed();

    embed_windows_icon();

    let build_id = std::env::var("QAQH_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_commit)
        .unwrap_or_else(|| {
            println!(
                "cargo:warning=QAQH_BUILD_ID fell back to CARGO_PKG_VERSION ({}) because the git commit could not be resolved; packaged daemon will fail the desktop identity check unless the manifest uses the same value",
                env!("CARGO_PKG_VERSION")
            );
            env!("CARGO_PKG_VERSION").to_string()
        });
    println!("cargo:rustc-env=QAQH_BUILD_ID={build_id}");
}

/// Embed the product icon (and basic file metadata) into Windows executables.
/// `winresource` 仅在 Windows target 下作为 build-dependency 声明；
/// 非 Windows 编译期不可见，必须用 cfg 门控（而非仅运行时早退）。
#[cfg(target_os = "windows")]
fn embed_windows_icon() {
    const ICON_PATH: &str = "../../assets/qaqh-harness.ico";
    println!("cargo:rerun-if-changed={ICON_PATH}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ICON_PATH);
    res.set("FileDescription", "QAQ-Harness Daemon");
    res.set("ProductName", "QAQ-Harness");
    res.compile()
        .expect("failed to compile Windows resources (icon)");
}

#[cfg(not(target_os = "windows"))]
fn embed_windows_icon() {}

fn ensure_webui_embed() {
    // rust-embed 要求 folder 在编译时存在，否则报错。开发期 out/renderer 可能尚未 `bun run build`，
    // 此时创建占位目录+index，避免编译失败；运行时会显示“前端产物缺失”提示，引导用户构建。
    let webui_out = std::path::Path::new("../../../qaqh-webui/out/renderer");
    if !webui_out.join("index.html").exists() {
        let _ = std::fs::create_dir_all(webui_out);
        let placeholder = r#"<!doctype html><meta charset="utf-8"><title>QAQ Harness</title><p style="font-family:system-ui;padding:2rem">前端产物缺失：请在 <code>qaqh-webui</code> 执行 <code>bun run build</code> 后重新 <code>cargo build -p qaqh-daemon</code>。此占位由 build.rs 自动生成。</p>"#;
        let _ = std::fs::write(webui_out.join("index.html"), placeholder);
        println!("cargo:warning=webui out/renderer missing — generated placeholder index.html; run `bun run build` in qaqh-webui for full UI");
    }
}

fn git_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
