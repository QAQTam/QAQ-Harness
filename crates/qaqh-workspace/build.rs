//! Build script: embeds the QAQ-Harness icon and file metadata into the
//! Windows executable. No-op on other targets.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    embed_windows_resources();
}

/// `winresource` 仅在 Windows target 下作为 build-dependency 声明；
/// 非 Windows 编译期不可见，必须用 cfg 门控（而非仅运行时早退）。
#[cfg(target_os = "windows")]
fn embed_windows_resources() {
    const ICON_PATH: &str = "../../assets/qaqh-harness.ico";
    println!("cargo:rerun-if-changed={ICON_PATH}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ICON_PATH);
    res.set("FileDescription", "QAQ-Harness Tool Executor");
    res.set("ProductName", "QAQ-Harness");
    res.compile()
        .expect("failed to compile Windows resources (icon)");
}

#[cfg(not(target_os = "windows"))]
fn embed_windows_resources() {}
