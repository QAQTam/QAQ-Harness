/// CJK byte-boundary safety check — prevents panics on multi-byte characters.
///
/// Runs `scripts/check-cjk-split.py --check` and fails if any UNSAFE slices are found.
/// Skips silently if Python or script is unavailable (local dev without Python).
#[test]
fn cjk_no_unsafe_byte_slices() {
    // CARGO_MANIFEST_DIR is crates/qaqh-workspace, navigate up to project root
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let script = root.join("scripts").join("check-cjk-split.py");

    if !script.exists() {
        eprintln!("[SKIP] CJK check script not found at {}", script.display());
        return;
    }

    let output = match std::process::Command::new("python")
        .arg(&script)
        .args(["crates", "--check"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[SKIP] Cannot run Python: {e}");
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        // Clean run — print summary for CI log
        let unsafe_count = stdout.lines().filter(|l| l.contains("UNSAFE")).count();
        let maybe_count = stdout.lines().filter(|l| l.contains("MAYBE")).count();
        println!(
            "CJK check passed: 0 UNSAFE, {} MAYBE (see --all for details)",
            maybe_count.saturating_sub(unsafe_count)
        );
    } else {
        panic!("CJK UNSAFE byte slices found:\n\n{stdout}\n{stderr}");
    }
}
