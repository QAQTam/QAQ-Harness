//! Attachment preview hydration shared between daemon HTTP paths.
//! Extracted from `qaqh-daemon/src/ringing_http.rs` to avoid drift.

use crate::RingingHub;

/// Hydrate `ConversationSendMessage` attachment references into inline previews.
///
/// Takes `attachments` out of the command, resolves each `content_id` via
/// `hub.get_content(seed, ...)`, validates sha256/media_type, and prepends
/// `[Files]` previews to `text`. No-op for non-conversation commands or empty
/// attachments.
pub fn hydrate_attachment_previews(
    hub: &RingingHub,
    seed: &str,
    command: &mut qaqh_ringing::RingingCommand,
) -> Result<(), String> {
    let qaqh_ringing::RingingCommand::Conversation(
        qaqh_domain::ConversationCommand::ConversationSendMessage {
            text, attachments, ..
        },
    ) = command
    else {
        return Ok(());
    };
    let Some(references) = attachments.take() else {
        return Ok(());
    };
    if references.is_empty() {
        return Ok(());
    }
    let mut parts = vec!["[Files]".to_string()];
    for reference in references {
        let entry = hub
            .get_content(seed, &reference.content_id)
            .ok_or_else(|| "attachment_not_found".to_string())?;
        if entry.sha256 != reference.sha256 || entry.media_type != reference.media_type {
            return Err("attachment_mismatch".into());
        }
        let preview = String::from_utf8_lossy(&entry.bytes)
            .lines()
            .take(10)
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(1000)
            .collect::<String>();
        parts.push(format!(
            "\n{} ({}):\n{}",
            reference.content_id, reference.media_type, preview
        ));
    }
    parts.push(format!("\n\n[Message]\n{text}"));
    *text = parts.join("");
    Ok(())
}
