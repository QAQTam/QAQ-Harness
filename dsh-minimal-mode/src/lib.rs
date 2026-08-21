//! `dsh-minimal-mode` — faithful replication of the deepseek-harness
//! minimal-mode tool set inside QAQ-Harness.
//!
//! Two tools are registered:
//!   * `bash_v2`            — persistent PTY bash (verbatim minimal-mode output)
//!   * `str_replace_editor` — view/create/str_replace/insert editor
//!
//! Both return pure text that matches the minimal-mode harness word-for-word.
//! `bash_v2` is registered under a distinct name (not `bash`) so it stays
//! isolated from QAQ-Harness's existing non-persistent `bash` tool; a future tool
//! whitelist can re-map it to the canonical `bash` name.

// The faithful JS-port logic leans on `.unwrap()` on Mutex locks and on
// byte-index string slicing to mirror `indexOf`/`lastIndexOf` semantics.
// This crate is intentionally isolated, so these lints are relaxed here only.
#![allow(clippy::unwrap_used, clippy::string_slice)]

pub mod bash;
pub mod editor;
pub mod pty;

use qaqh_workspace::ToolManager;

/// Register the minimal-mode tools (`bash_v2`, `str_replace_editor`) onto a
/// QAQ-Harness `ToolManager`. Compatible with `qaqh_workspace::registration::ToolRegistrar`.
pub fn register(mgr: &mut ToolManager) {
    bash::register(mgr);
    editor::register(mgr);
}
