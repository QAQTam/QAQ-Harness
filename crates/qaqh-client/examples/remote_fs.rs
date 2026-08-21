//! Remote daemon smoke test: connect to `--server` mode via env and list/read
//! daemon-side files through the new `fs.*` queries.
//!
//! Usage:
//!   QAQH_REMOTE_URL=http://192.168.1.10:64413 \
//!   QAQH_REMOTE_TOKEN=<token> \
//!   cargo run -p qaqh-client --example remote_fs -- /home/user
//!
//! Second optional arg is the file to preview with `fs.read`.

use qaqh_client::{
    Client, ClientHandlers, ClientOptions, QueryRequest, RemoteEndpoint, display_path,
    remote_path_from_display,
};

#[tokio::main]
async fn main() {
    let base_url = std::env::var("QAQH_REMOTE_URL").expect("QAQH_REMOTE_URL");
    let token = std::env::var("QAQH_REMOTE_TOKEN").unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().cloned().unwrap_or_else(|| "/".to_string());

    let client = Client::connect_async(ClientOptions {
        handlers: ClientHandlers::default(),
        remote: Some(RemoteEndpoint {
            base_url: base_url.clone(),
            token,
        }),
        ..Default::default()
    })
    .await
    .expect("remote connect");

    // 显示约定演示：daemon 路径 <-> `//ip/...`。
    let host = qaqh_client::display_host(&base_url);
    let shown = display_path(host, &dir);
    println!("dir: {dir} -> display: {shown}");
    assert_eq!(
        remote_path_from_display(&shown).as_deref(),
        Some(dir.as_str()),
        "display round-trip"
    );

    let listing = client
        .query(QueryRequest::FsList { path: dir.clone() })
        .await
        .expect("fs.list");
    println!(
        "fs.list:\n{}",
        serde_json::to_string_pretty(&listing).unwrap_or_default()
    );

    if let Some(file) = args.get(1) {
        let content = client
            .query(QueryRequest::FsRead {
                path: file.clone(),
                max_bytes: Some(4096),
            })
            .await
            .expect("fs.read");
        println!(
            "fs.read:\n{}",
            serde_json::to_string_pretty(&content).unwrap_or_default()
        );
    }

    client.close();
}
