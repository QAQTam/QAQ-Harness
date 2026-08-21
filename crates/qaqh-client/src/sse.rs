//! SSE stream reader: frame parsing, cursor tracking, idle timeout, reconnect.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::watch;

use crate::error::{ClientError, Result};
use crate::sse_decoder::SseDecoder;
use crate::types::{Channel, ChannelStatus, SseFrame};

const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_MS: u64 = 30_000;

/// Callbacks for one channel stream.
pub struct StreamHandlers {
    pub on_batch: std::sync::Arc<dyn Fn(crate::types::EventBatch) + Send + Sync>,
    pub on_status: std::sync::Arc<dyn Fn(ChannelStatus) + Send + Sync>,
    pub on_reset: Option<std::sync::Arc<dyn Fn(crate::types::ResetRequired) + Send + Sync>>,
}

/// One SSE channel with independent cursor, reconnect backoff and idle timeout.
/// Cursor/epoch come from the shared session so `Last-Event-ID` stays coherent.
pub struct ChannelStream {
    url: String,
    token: String,
    channel: Channel,
    http: reqwest::Client,
    handlers: StreamHandlers,
    /// (server_epoch, client_session_id) — read on each connect. `None` until
    /// the session is negotiated; updated by lease re-negotiation.
    session_ctx: watch::Receiver<Option<(String, String)>>,
    /// Cursor of the last accepted frame (per channel).
    cursor: u64,
}

impl ChannelStream {
    pub fn new(
        url: String,
        token: String,
        channel: Channel,
        http: reqwest::Client,
        handlers: StreamHandlers,
        session_ctx: watch::Receiver<Option<(String, String)>>,
    ) -> Self {
        Self {
            url,
            token,
            channel,
            http,
            handlers,
            session_ctx,
            cursor: 0,
        }
    }

    /// Run the connect loop until `stop` is signalled. Never returns an error
    /// to the caller unless the stream is stopped.
    pub async fn run(&mut self, mut stop: watch::Receiver<bool>) {
        let mut retry_ms = RETRY_BASE_MS;
        while !*stop.borrow() {
            match self.connect_once(&mut stop).await {
                Ok(()) => {
                    // Clean stream end: reconnect without backoff reset (mirrors TS).
                }
                Err(err) => {
                    if *stop.borrow() {
                        return;
                    }
                    (self.handlers.on_status)(ChannelStatus::Reconnecting {
                        retry_ms,
                        last_cursor: self.cursor,
                    });
                    log::warn!(
                        "[qaqh-client] SSE {} reconnect in {retry_ms}ms: {err}",
                        self.channel.as_str()
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(retry_ms)) => {}
                        _ = stop.changed() => return,
                    }
                    retry_ms = std::cmp::min(retry_ms * 2, RETRY_MAX_MS);
                }
            }
        }
    }

    async fn connect_once(&mut self, stop: &mut watch::Receiver<bool>) -> Result<()> {
        (self.handlers.on_status)(ChannelStatus::Connecting);
        let Some((server_epoch, client_session_id)) = self.session_ctx.borrow().clone() else {
            return Err(ClientError::Negotiation("session not open".into()));
        };

        let mut request = self
            .http
            .get(&self.url)
            .bearer_auth(&self.token)
            .header("X-QAQH-Client-Session-Id", &client_session_id)
            .header("Accept", "text/event-stream");
        if self.cursor > 0 {
            request = request.header(
                "Last-Event-ID",
                format!("{server_epoch}:{}:{}", self.channel.as_str(), self.cursor),
            );
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path: self.url.clone(),
            });
        }
        (self.handlers.on_status)(ChannelStatus::Open {
            server_epoch: server_epoch.clone(),
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
                _ = &mut idle => {
                    return Err(ClientError::Transport("SSE idle timeout".into()));
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            idle.as_mut().reset(tokio::time::Instant::now() + SSE_IDLE_TIMEOUT);
                            decoder.push(&bytes);
                            self.drain_frames(&mut decoder, &server_epoch)?;
                        }
                        Some(Err(err)) => {
                            return Err(ClientError::Transport(format!("SSE read: {err}")));
                        }
                        None => {
                            return Err(ClientError::Transport("SSE stream ended".into()));
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
        if frame.event_type == "ringing.reset_required" {
            let reset: crate::types::ResetRequired = serde_json::from_str(frame.data.trim())
                .map_err(|e| ClientError::Protocol(format!("bad reset_required: {e}")))?;
            if let Some(on_reset) = &self.handlers.on_reset {
                on_reset(reset);
            }
            return Ok(());
        }

        let envelope: crate::types::RingingEventEnvelope = serde_json::from_str(frame.data.trim())
            .map_err(|e| ClientError::Protocol(format!("bad envelope: {e}")))?;
        crate::types::validate_envelope(&envelope, self.channel).map_err(ClientError::Protocol)?;

        // Cursor must match the frame id exactly; only accepted envelopes advance it.
        if let Some(frame_cursor) = crate::types::cursor_from_sse_id(&frame.id, self.channel) {
            if envelope.stream_seq != frame_cursor {
                return Err(ClientError::Protocol(format!(
                    "cursor mismatch: envelope stream_seq {} != SSE id seq {frame_cursor}",
                    envelope.stream_seq
                )));
            }
            self.cursor = frame_cursor;
        }
        let batch =
            crate::types::envelope_to_batch(self.channel, envelope, server_epoch.to_string());
        (self.handlers.on_batch)(batch);
        Ok(())
    }
}
