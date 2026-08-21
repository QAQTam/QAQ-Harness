//! Smoke test: connect to the daemon, open a Ringing lease, observe the three
//! SSE channels, then issue a typed query.
//!
//! Run against the dev daemon:
//! ```powershell
//! $env:QAQH_DATA_DIR = "F:\QAQ-Harness\.deepx-test-home\.deepx"
//! cargo run -p qaqh-client --example connect
//! ```

use std::time::Duration;

use qaqh_client::{ChannelStatus, Client, ClientHandlers, ClientOptions, QueryRequest};

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let start = std::time::Instant::now();
        let client = Client::connect_async(ClientOptions {
            handlers: ClientHandlers {
                on_batch: std::sync::Arc::new(|batch| {
                    println!(
                        "[batch] {} seed={} seq={}..{} envelopes={}",
                        batch.channel.as_str(),
                        batch.seed,
                        batch.from_stream_seq,
                        batch.to_stream_seq,
                        batch.envelopes.len(),
                    );
                }),
                on_status: std::sync::Arc::new(|channel, status| {
                    let state = match &status {
                        ChannelStatus::Connecting => "connecting".to_string(),
                        ChannelStatus::Open { cursor, .. } => format!("open cursor={cursor}"),
                        ChannelStatus::Reconnecting { retry_ms, .. } => {
                            format!("reconnecting in {retry_ms}ms")
                        }
                        ChannelStatus::Closed { reason } => format!("closed: {reason}"),
                    };
                    println!("[status] {} {state}", channel.as_str());
                }),
                on_reset: None,
                ..Default::default()
            },
            launch_daemon_if_missing: false,
            ..Default::default()
        })
        .await
        .expect("connect failed");

        let session = client.session_state().await.expect("no session state");
        let epoch = session.server_epoch.chars().take(8).collect::<String>();
        println!(
            "[open] instance={} session={} epoch={epoch} ttl={}ms renew={}ms (took {:?})",
            session.client_instance_id,
            session.client_session_id,
            session.lease_ttl_ms,
            session.renew_interval_ms,
            start.elapsed()
        );

        // Observe events for a few seconds, then run a typed query.
        tokio::time::sleep(Duration::from_secs(5)).await;

        match client.query(QueryRequest::SessionList).await {
            Ok(value) => println!("[query] session.list -> {value}"),
            Err(err) => println!("[query] session.list failed: {err}"),
        }

        client.close();
        println!("[done] closed cleanly");
    });
}
