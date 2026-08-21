//! Build script: embeds the QAQ-Harness icon and file metadata into the
//! Windows executable. No-op on other targets.

const ICON_PATH: &str = "../../assets/qaqh-harness.ico";

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed={ICON_PATH}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ICON_PATH);
    res.set("FileDescription", "QAQ-Harness Tool Executor");
    res.set("ProductName", "QAQ-Harness");
    res.compile()
        .expect("failed to compile Windows resources (icon)");
}
