#[test]
fn ten_parallel_reads() {
    use std::sync::mpsc;
    use std::time::Duration;

    // Setup workspace with a small test file in a temp dir
    let tmp = std::env::temp_dir().join("qaqh_test_concurrent_read");
    let _ = std::fs::create_dir_all(&tmp);
    let file_path = tmp.join("test.txt");
    std::fs::write(&file_path, "0123456789").unwrap();
    qaqh_workspace::set_workspace(&tmp.to_string_lossy());

    // Init tool manager
    qaqh_workspace::runtime::init_tools("test", &[], vec![]);

    let file_path_str = file_path.to_string_lossy().to_string();
    let args = serde_json::json!({"path": file_path_str}).to_string();

    let (done_tx, done_rx) = mpsc::channel();

    // Knife-1 step 2: runtime context is per-actor (thread-local), so each
    // executing thread provides its own context. The concurrent tool manager
    // reads over the shared process manager remain the point of this test.
    let mut handles = Vec::new();
    for i in 0..10 {
        let args = args.clone();
        let done_tx = done_tx.clone();
        handles.push(std::thread::spawn(move || {
            qaqh_workspace::runtime::set_context("test", 4);
            let result = qaqh_workspace::execution::execute_with_context(
                "read",
                "",
                &args,
                &format!("tc_{}", i),
                None,
            );
            done_tx.send((i, result.success, result.content)).unwrap();
        }));
    }
    drop(done_tx);

    // Join with 5-second timeout
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    for h in handles {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "Timed out waiting for tool thread");
        h.join().unwrap();
    }

    let mut results = Vec::new();
    while let Ok(r) = done_rx.try_recv() {
        results.push(r);
    }
    assert_eq!(
        results.len(),
        10,
        "Expected 10 results, got {}",
        results.len()
    );
    for (_i, success, content) in &results {
        assert!(success, "Tool failed: {}", content);
        assert!(
            content.contains("0123456789"),
            "Missing content: {}",
            content
        );
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}
