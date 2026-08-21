//! Native Ringing client contracts.
//!
//! The daemon, transport client, and native shells share the canonical domain
//! and wire types. JSON is a serialization detail at the HTTP/SSE boundary;
//! it is not an application-facing event model.

use qaqh_ringing::{RINGING_SCHEMA, RINGING_VERSION};
use serde::{Deserialize, Serialize};

pub use qaqh_domain::{
    ActivityState as DomainActivityState, AskAnswer, AskQuestion as DomainAskQuestion, ContentRef,
    ControlCommand, ControlEvent, ConversationCommand, ConversationEvent, ConversationMode,
    DashboardSnapshot as DomainDashboardSnapshot, DomainError, ErrorScope, PermissionCategory,
    PermissionRisk, ProviderToolState, RingingChannel as Channel, RoundDeltaKind,
    SessionState as DomainSessionState, SkillInfo, SkillRuntimeInfo, TimelineBlock,
    TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent, TimelineRound,
    TimelineSnapshot, TimelineTool, TimelineToolState, TimelineTurn, TimelineTurnState, TodoItem,
    ToolCommand, ToolEvent,
};
pub use qaqh_ringing::{
    ClientOpenRequest as OpenRequest, ClientOpenResponse as OpenResponse, RingingCommand,
    RingingCommandAck, RingingCommandAckStatus, RingingCommandState, RingingCommandStatus,
    RingingEvent, RingingEventBatch as EventBatch, RingingEventEnvelope,
    RingingResetRequired as ResetRequired,
};

/// Stable channel order used to start the three independent SSE streams.
pub const CHANNELS: [Channel; 3] = [Channel::Control, Channel::Conversation, Channel::Tool];

/// Per-channel SSE connection state. This is a native transport state rather
/// than a renderer payload; UI shells marshal it onto their dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelStatus {
    Connecting,
    Open { server_epoch: String, cursor: u64 },
    Reconnecting { retry_ms: u64, last_cursor: u64 },
    Closed { reason: String },
}

/// Per-session timeline connection state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineStatus {
    Connecting {
        seed: String,
    },
    Open {
        seed: String,
        server_epoch: String,
        cursor: u64,
    },
    Reconnecting {
        seed: String,
        retry_ms: u64,
        cursor: u64,
    },
    Closed {
        seed: String,
        reason: String,
    },
}

/// Versioned response from `GET /ringing/v1/sessions/{seed}/timeline`.
///
/// `snapshot` is the authoritative materialized transcript. Pagination
/// metadata remains outside it because it describes the current HTTP page,
/// not transcript state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TimelinePage {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
    pub snapshot: TimelineSnapshot,
    pub has_more: bool,
    pub total_turns: usize,
}

impl TimelinePage {
    pub fn validate_for(&self, seed: &str) -> Result<(), String> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_VERSION
            || self.seed != seed
            || self.server_epoch.is_empty()
        {
            return Err("invalid Ringing V1 timeline page".into());
        }
        Ok(())
    }
}

/// Ringing V1 timeline SSE frame.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelineSseFrame {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
    pub entry: TimelineEntry,
}

/// Parsed SSE frame (a block of `key: value` lines separated by a blank line).
#[derive(Debug, Clone, Default)]
pub struct SseFrame {
    pub id: String,
    pub event_type: String,
    pub data: String,
}

pub fn parse_sse_frame(frame: &str) -> SseFrame {
    let mut parsed = SseFrame::default();
    for line in frame.split('\n') {
        if line.starts_with(':') {
            continue;
        }
        if let Some(id) = line.strip_prefix("id:") {
            parsed.id = id.trim().to_string();
        } else if let Some(event) = line.strip_prefix("event:") {
            parsed.event_type = event.trim().to_string();
        } else if let Some(data) = line.strip_prefix("data:") {
            if !parsed.data.is_empty() {
                parsed.data.push('\n');
            }
            parsed.data.push_str(data.trim());
        }
    }
    parsed
}

/// Extract the stream sequence from `id: <epoch>:<channel>:<seq>`.
pub fn cursor_from_sse_id(id: &str, channel: Channel) -> Option<u64> {
    let mut parts = id.split(':');
    let epoch = parts.next()?;
    let frame_channel = parts.next()?;
    let seq = parts.next()?;
    if epoch.is_empty() || frame_channel != channel.as_str() || parts.next().is_some() {
        return None;
    }
    seq.parse::<u64>().ok()
}

pub fn validate_envelope(envelope: &RingingEventEnvelope, channel: Channel) -> Result<(), String> {
    envelope.validate().map_err(str::to_string)?;
    if envelope.event.channel() != channel {
        return Err(format!(
            "envelope channel {:?} != connection channel {:?}",
            envelope.event.channel(),
            channel
        ));
    }
    Ok(())
}

/// Wrap one validated SSE envelope in the canonical Ringing batch type.
pub fn envelope_to_batch(
    channel: Channel,
    envelope: RingingEventEnvelope,
    server_epoch: String,
) -> EventBatch {
    let seq = envelope.stream_seq;
    EventBatch {
        schema: RINGING_SCHEMA.to_string(),
        version: RINGING_VERSION,
        channel,
        seed: envelope.seed.clone(),
        server_epoch,
        from_stream_seq: seq,
        to_stream_seq: seq,
        envelopes: vec![envelope],
    }
}

/// Options for one typed command submission. The client creates an id when
/// callers do not need to supply one for durable retry/correlation.
#[derive(Debug, Clone, Default)]
pub struct CommandOptions {
    pub command_id: Option<String>,
    pub expected_revision: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ConversationEvent, Delivery};
    use qaqh_ringing::RingingEvent;

    #[test]
    fn canonical_envelope_keeps_domain_event_typed() {
        let envelope = RingingEventEnvelope::new(
            "seed-1",
            7,
            3,
            3,
            "event-1",
            RingingEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hello".into(),
            }),
        );
        assert_eq!(envelope.delivery, Delivery::Reliable);
        validate_envelope(&envelope, Channel::Conversation).expect("valid envelope");
        let batch = envelope_to_batch(Channel::Conversation, envelope, "epoch-1".into());
        batch.validate().expect("canonical batch");
    }

    #[test]
    fn timeline_page_validates_version_and_seed() {
        let page = TimelinePage {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_VERSION,
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
            snapshot: TimelineSnapshot {
                watermark: 0,
                turns: vec![],
            },
            has_more: false,
            total_turns: 0,
        };
        page.validate_for("seed-1").expect("valid page");
        assert!(page.validate_for("seed-2").is_err());
    }
}
