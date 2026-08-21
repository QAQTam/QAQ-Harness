//! Per-session Ringing V1 timeline SSE stream.
//!
//! Mirrors the Ringing V1 timeline semantics (the original Electron reference
//! implementation `apps/desktop/electron/timelineClient.ts` was removed): one
//! transcript, one SSE stream, one monotonically increasing cursor
//! (`{epoch}:timeline:{seq}`).
//! Gap recovery re-fetches the authoritative snapshot and advances the
//! cursor to its watermark so `Last-Event-ID` never stalls.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::watch;

use crate::error::{ClientError, Result};
use crate::session::RingingSession;
use crate::sse_decoder::SseDecoder;
use crate::types::{SseFrame, TimelineEntry, TimelinePage, TimelineSseFrame, TimelineStatus};

const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_MS: u64 = 30_000;

/// One per-session timeline stream with independent cursor, reconnect backoff
/// and gap recovery. Created by `Client::activate_timeline` and run as a
/// background task; callbacks fire on the tokio side.
pub struct TimelineStream {
    base_url: String,
    token: String,
    seed: String,
    http: reqwest::Client,
    /// Read on every connect: (server_epoch, client_session_id).
    session: Arc<RingingSession>,
    on_entry: Arc<dyn Fn(String, TimelineEntry) + Send + Sync>,
    on_status: Arc<dyn Fn(TimelineStatus) + Send + Sync>,
    /// Forwarded on gap recovery: the fresh snapshot becomes the new baseline.
    on_snapshot: Arc<dyn Fn(TimelinePage) + Send + Sync>,
    /// Optional sink for `Client::timeline_status()` (also fed on exit).
    status_tx: Option<watch::Sender<Option<TimelineStatus>>>,
    /// Cursor of the last accepted entry (starts at the snapshot watermark).
    cursor: u64,
    /// Epoch of the last successful connect. A lease re-negotiation (renewal
    /// failure -> reopen) changes the server epoch; the old cursor is invalid
    /// against the new epoch (daemon treats a stale `Last-Event-ID` as 0 and
    /// replays from the head, which the cursor guard rejects as Protocol
    /// error — the exact reconnect-death loop this stream must break).
    last_epoch: Option<String>,
}

impl TimelineStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: String,
        token: String,
        seed: String,
        http: reqwest::Client,
        session: Arc<RingingSession>,
        on_entry: Arc<dyn Fn(String, TimelineEntry) + Send + Sync>,
        on_status: Arc<dyn Fn(TimelineStatus) + Send + Sync>,
        on_snapshot: Arc<dyn Fn(TimelinePage) + Send + Sync>,
        initial_cursor: u64,
        status_tx: Option<watch::Sender<Option<TimelineStatus>>>,
    ) -> Self {
        Self {
            base_url,
            token,
            seed,
            http,
            session,
            on_entry,
            on_status,
            on_snapshot,
            status_tx,
            cursor: initial_cursor,
            last_epoch: None,
        }
    }

    fn set_status(&self, status: TimelineStatus) {
        // `send_replace` updates the value even without receivers: the
        // `timeline_status()` query reads the sender-side value, and the
        // receiver may be dropped as soon as `activate_timeline` returns.
        if let Some(tx) = &self.status_tx {
            let _ = tx.send_replace(Some(status.clone()));
        }
        (self.on_status)(status);
    }

    /// Run the connect loop until `stop` (own handle) or `session_stop`
    /// (client-wide close) is signalled. Never returns an error to the caller
    /// unless the stream is stopped.
    pub async fn run(
        &mut self,
        mut stop: watch::Receiver<bool>,
        mut session_stop: watch::Receiver<bool>,
    ) {
        let mut retry_ms = RETRY_BASE_MS;
        while !*stop.borrow() && !*session_stop.borrow() {
            match self.connect_once(&mut stop, &mut session_stop).await {
                Ok(()) => {
                    // Clean stream end (stop signal): exit.
                    if *stop.borrow() || *session_stop.borrow() {
                        return;
                    }
                }
                Err(err) => {
                    if *stop.borrow() || *session_stop.borrow() {
                        return;
                    }
                    // A gap means the server journal no longer covers our
                    // cursor. Recover by fetching the authoritative snapshot:
                    // its watermark becomes the new cursor so the next
                    // reconnect resumes from a covered position. Without this,
                    // Last-Event-ID never advances and the client reconnects
                    // into the same gap forever (mirrors TS `onGap`).
                    if matches!(err, ClientError::TimelineGap { .. }) {
                        match self.recover_gap().await {
                            Ok(()) => log::info!(
                                "[qaqh-client] timeline {} gap recovered at cursor {}",
                                self.seed,
                                self.cursor
                            ),
                            Err(recovery_err) => log::warn!(
                                "[qaqh-client] timeline {} gap snapshot recovery failed: {recovery_err}",
                                self.seed
                            ),
                        }
                    }
                    self.set_status(TimelineStatus::Reconnecting {
                        seed: self.seed.clone(),
                        retry_ms,
                        cursor: self.cursor,
                    });
                    log::warn!(
                        "[qaqh-client] timeline {} reconnect in {retry_ms}ms: {err}",
                        self.seed
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(retry_ms)) => {}
                        _ = stop.changed() => return,
                        _ = session_stop.changed() => return,
                    }
                    retry_ms = std::cmp::min(retry_ms * 2, RETRY_MAX_MS);
                }
            }
        }
        self.set_status(TimelineStatus::Closed {
            seed: self.seed.clone(),
            reason: "stopped".into(),
        });
    }

    async fn connect_once(
        &mut self,
        stop: &mut watch::Receiver<bool>,
        session_stop: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        self.set_status(TimelineStatus::Connecting {
            seed: self.seed.clone(),
        });
        let state = self
            .session
            .state()
            .await
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;

        // Lease re-negotiation swapped the epoch: re-baseline against the
        // authoritative snapshot so the reconnect cursor stays covered, then
        // forward the snapshot so listeners rebuild the transcript.
        if self.last_epoch.as_deref() != Some(state.server_epoch.as_str()) {
            let epoch_changed = self.last_epoch.is_some();
            self.last_epoch = Some(state.server_epoch.clone());
            if epoch_changed {
                match self.recover_gap().await {
                    Ok(()) => log::info!(
                        "[qaqh-client] timeline {} re-baselined after session re-negotiation (cursor {})",
                        self.seed,
                        self.cursor
                    ),
                    Err(recovery_err) => {
                        // 兜底：从 0 全量回放（daemon 按 0 处理旧 epoch 的
                        // Last-Event-ID），避免带着旧 cursor 连进 Protocol
                        // error 死循环。
                        log::warn!(
                            "[qaqh-client] timeline {} re-baseline failed ({recovery_err}); replaying from head",
                            self.seed
                        );
                        self.cursor = 0;
                    }
                }
            }
        }

        let path = format!("/ringing/v1/sessions/{}/timeline/events", self.seed);
        let mut request = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .header("Accept", "text/event-stream");
        if self.cursor > 0 {
            request = request.header(
                "Last-Event-ID",
                format!("{}:timeline:{}", state.server_epoch, self.cursor),
            );
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        self.set_status(TimelineStatus::Open {
            seed: self.seed.clone(),
            server_epoch: state.server_epoch.clone(),
            cursor: self.cursor,
        });

        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let idle = tokio::time::sleep(SSE_IDLE_TIMEOUT);
        tokio::pin!(idle);

        loop {
            tokio::select! {
                _ = stop.changed() => {
                    return Ok(()); // stopped: exit loop cleanly
                }
                _ = session_stop.changed() => {
                    return Ok(()); // client-wide close
                }
                _ = &mut idle => {
                    return Err(ClientError::Transport("timeline SSE idle timeout".into()));
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            idle.as_mut().reset(tokio::time::Instant::now() + SSE_IDLE_TIMEOUT);
                            decoder.push(&bytes);
                            self.drain_frames(&mut decoder, &state.server_epoch)?;
                        }
                        Some(Err(err)) => {
                            return Err(ClientError::Transport(format!("timeline SSE read: {err}")));
                        }
                        None => {
                            return Err(ClientError::Transport("timeline SSE stream ended".into()));
                        }
                    }
                }
            }
        }
    }

    /// 消费解码器中所有已完整的帧并分发。
    fn drain_frames(&mut self, decoder: &mut SseDecoder, server_epoch: &str) -> Result<()> {
        while let Some(frame) = decoder.next_frame() {
            let frame = match frame {
                Ok(frame) => frame,
                Err(()) => continue,
            };
            if frame.data.trim().is_empty() {
                continue; // keepalive/空 data 帧
            }
            self.dispatch(frame, server_epoch)?;
        }
        Ok(())
    }

    fn dispatch(&mut self, frame: SseFrame, server_epoch: &str) -> Result<()> {
        let parsed: TimelineSseFrame = serde_json::from_str(frame.data.trim())
            .map_err(|e| ClientError::Protocol(format!("bad timeline frame: {e}")))?;
        if parsed.schema != qaqh_ringing::RINGING_SCHEMA
            || parsed.version != qaqh_ringing::RINGING_VERSION
            || parsed.seed != self.seed
            || parsed.server_epoch != server_epoch
        {
            return Err(ClientError::Protocol(
                "invalid Ringing V1 timeline SSE frame".into(),
            ));
        }
        if parsed.entry.timeline_seq <= self.cursor {
            return Err(ClientError::Protocol(format!(
                "timeline entry at/below cursor: {} <= {}",
                parsed.entry.timeline_seq, self.cursor
            )));
        }
        let expected_id = format!("{server_epoch}:timeline:{}", parsed.entry.timeline_seq);
        if !frame.id.is_empty() && frame.id != expected_id {
            return Err(ClientError::Protocol(
                "timeline SSE cursor/frame mismatch".into(),
            ));
        }
        if parsed.entry.timeline_seq != self.cursor + 1 {
            return Err(ClientError::TimelineGap {
                expected: self.cursor + 1,
                received: parsed.entry.timeline_seq,
            });
        }
        self.cursor = parsed.entry.timeline_seq;
        (self.on_entry)(self.seed.clone(), parsed.entry);
        Ok(())
    }

    /// Re-fetch the authoritative snapshot; its watermark becomes the new
    /// reconnect cursor. The snapshot is forwarded so the shell can rebuild
    /// the transcript (mirrors TS `onGap`).
    async fn recover_gap(&mut self) -> Result<()> {
        let state = self
            .session
            .state()
            .await
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;
        let path = format!("/ringing/v1/sessions/{}/timeline", self.seed);
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        let page: TimelinePage = response.json().await?;
        page.validate_for(&self.seed)
            .map_err(ClientError::Protocol)?;
        self.cursor = page.snapshot.watermark;
        (self.on_snapshot)(page);
        Ok(())
    }
}
