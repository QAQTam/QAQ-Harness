#[cfg(test)]
mod rt_tests {
    #[test]
    fn test_block_on_with_time() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let x = rt.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            42
        });
        assert_eq!(x, 42);
    }
}
