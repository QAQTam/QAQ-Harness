//! Web tool — fetch URL content.
//!
//! Web *search* is no longer a local tool: DeepSeek / OpenAI Responses APIs
//! ship a built-in `web_search` tool executed server-side (see
//! `qaqh-gate/src/responses.rs`). The model triggers it on its own, so the
//! local Bing-RSS parser was removed — this tool only fetches URLs the model
//! (or user) explicitly wants to read.

use crate::{JsonArgs, ToolCallCtx, ToolHandler, ToolResult, ToolRisk};
use std::time::Duration;

fn http_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .build()
        .into()
}

pub(super) fn handle_web_fetch(ctx: ToolCallCtx) -> ToolResult {
    let timeout_secs = ctx.timeout_secs.unwrap_or(30);
    if ctx.args.s("url").starts_with("http") {
        let payload = web_fetch(&ctx.args, timeout_secs);
        let is_error = serde_json::from_str::<serde_json::Value>(&payload)
            .ok()
            .and_then(|value| {
                value
                    .get("status")
                    .and_then(|status| status.as_str())
                    .map(|status| status == "error")
            })
            .unwrap_or(false);
        if is_error {
            ToolResult::error(payload)
        } else {
            ToolResult::ok(payload)
        }
    } else {
        ToolResult::error(crate::json_err(
            "MISSING_URL",
            "web_fetch: 'url' (starting with http) is required; web search is handled by the model's built-in web_search tool",
            "Pass a URL to fetch, or rely on the model's server-side web_search.",
        ))
    }
}

fn web_fetch(args: &serde_json::Value, timeout_secs: u64) -> String {
    const MAX_WEB_BODY_BYTES: u64 = 512 * 1024;
    let url = args.s("url");
    if url.is_empty() || !url.starts_with("http") {
        return crate::json_err("INVALID_URL", "web_fetch: url must start with http", "");
    }
    let resp = match http_agent(timeout_secs)
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36",
        )
        .call()
    {
        Ok(r) => r,
        Err(e) => return crate::json_err("FETCH_ERROR", format!("{e}"), ""),
    };
    if resp
        .body()
        .content_length()
        .is_some_and(|len| len > MAX_WEB_BODY_BYTES)
    {
        return crate::json_err(
            "RESPONSE_TOO_LARGE",
            format!("Response exceeds the {} byte limit", MAX_WEB_BODY_BYTES),
            "Fetch a narrower URL or use a source with a paginated API.",
        );
    }
    let is_html = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("html"))
        .unwrap_or(false);
    let body = match resp
        .into_body()
        .with_config()
        .limit(MAX_WEB_BODY_BYTES)
        .read_to_string()
    {
        Ok(b) => b,
        Err(_) => {
            return crate::json_err(
                "READ_ERROR",
                "Response could not be read within the body limit",
                "Fetch a narrower URL or use a source with a paginated API.",
            );
        }
    };
    let readable = if is_html || body.trim_start().starts_with("<") {
        html2text::from_read(body.as_bytes(), body.len().min(120_000)).unwrap_or(body)
    } else {
        body
    };
    if let Some(out) = args.get("output").and_then(|v| v.as_str()) {
        let target = crate::resolve_workspace_path(out);
        let before = std::fs::read_to_string(&target).ok();
        let _ = std::fs::write(&target, &readable);
        crate::journal::record_change(
            &crate::journal::active_session(),
            "",
            "web_fetch",
            out,
            "overwrite",
            before.as_deref(),
            Some(&readable),
            "ok",
        );
        // Lead with a save marker: plain-text folding keeps the first line,
        // so a folded historical result still tells the model where the
        // content lives — it can `read` the file instead of re-fetching.
        return format!("[saved to {}]\n{}", crate::display_path(&target), readable);
    }
    // Return the page text directly — the result *is* the content, no JSON
    // wrapper needed (errors above stay structured via json_err).
    readable
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler { key: "web_fetch".to_string(),
        description: "Fetch URL content (pass 'url') — this is a plain HTTP fetch tool. Web search is not a local tool — the model uses its built-in server-side web_search instead.",
        input_schema: serde_json::json!({"type":"object","properties":{"url":{"type":"string","description":"URL to fetch"},"output":{"type":"string","description":"Optional file path"}},"required":["url"],"additionalProperties":false}),
        handler: handle_web_fetch, risk: ToolRisk::ReadOnly,
        category: crate::permission::ToolCategory::Net,
        default_timeout: std::time::Duration::from_secs(30),
    },
    crate::ToolPlacement::Workspace,
);
}
