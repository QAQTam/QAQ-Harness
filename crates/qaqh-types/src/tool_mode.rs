//! Single source of truth for per-session tool modes.
//!
//! BUG-013 / REFACTOR-ROADMAP 刀6: the preset table used to live in
//! `qaqh-msgloop`, the daemon validation whitelist lived in `qaqh-runtime`,
//! the prompt special-case lived in `qaqh-config`, and every shell invented
//! its own `minimal:dsh` literal. Adding or changing a preset therefore
//! touched four crates.
//!
//! All names, tool sets, and model-facing projections now live here. The
//! runtime crates only consume this contract and never redefine it.

/// Full tool set, restored after any minimal/custom mode.
pub const STANDARD: &str = "standard";

/// Minimal tier A: bash/pwsh + file four-piece + search + confirm.
pub const MINIMAL: &str = "minimal";

/// Minimal tier B: reduced six-tool set.
pub const MINIMAL_B: &str = "minimal:b";

/// Minimal tier C: smallest stress-test set.
pub const MINIMAL_C: &str = "minimal:c";

/// deepseek-harness minimal preset. The model sees `bash` (not `bash_v2`).
pub const MINIMAL_DSH: &str = "minimal:dsh";

/// User-selected allowlist supplied through `custom_tools`.
pub const CUSTOM: &str = "custom";

/// Every mode accepted by `session.new` and `session.set_tool_mode`.
pub const KNOWN_MODES: &[&str] = &[STANDARD, MINIMAL, MINIMAL_B, MINIMAL_C, MINIMAL_DSH, CUSTOM];

/// Minimal-family modes share no-fold policy and the minimal system prompt
/// treatment is reserved for the dsh preset.
pub const MINIMAL_PREFIX: &str = "minimal";

/// Minimal tier A (internal registration keys).
pub const MINIMAL_TOOLS: &[&str] = &[
    "bash",
    "pwsh",
    "write",
    "edit",
    "read",
    "glob",
    "grep",
    "confirm_apply",
];

/// Minimal tier B (internal registration keys).
pub const MINIMAL_TOOLS_B: &[&str] = &["bash", "edit", "glob", "grep", "read", "confirm_apply"];

/// Minimal tier C (internal registration keys).
pub const MINIMAL_TOOLS_C: &[&str] = &["bash", "edit", "glob", "confirm_apply"];

/// Minimal dsh internal allowlist. `bash_v2` is the persistent-PTY handler.
pub const MINIMAL_DSH_TOOLS: &[&str] = &["bash_v2", "str_replace_editor"];

/// Minimal dsh model-facing schema: the internal `bash_v2` is projected to
/// the canonical minimal name `bash` and never leaked to the model.
pub const MINIMAL_DSH_MODEL_TOOLS: &[&str] = &["bash", "str_replace_editor"];

/// Returns `true` for every mode accepted by the daemon action whitelist.
pub fn is_known(mode: &str) -> bool {
    matches!(
        mode,
        STANDARD | MINIMAL | MINIMAL_B | MINIMAL_C | MINIMAL_DSH | CUSTOM
    )
}

/// Returns `true` for the minimal family. Callers must validate unknown names
/// before using this for fold-policy decisions.
pub fn is_minimal_family(mode: &str) -> bool {
    mode.starts_with(MINIMAL_PREFIX)
}

/// Returns `true` when the session uses the verbatim deepseek-harness prompt
/// and the `bash_v2 -> bash` projection.
pub fn is_minimal_dsh(mode: &str) -> bool {
    mode == MINIMAL_DSH
}

/// The internal tool allowlist for a fixed preset.
///
/// `standard`, `custom`, the empty legacy value, and unknown names return
/// `None`; those cases are interpreted by the caller.
pub fn preset_tools(mode: &str) -> Option<&'static [&'static str]> {
    match mode {
        MINIMAL => Some(MINIMAL_TOOLS),
        MINIMAL_B => Some(MINIMAL_TOOLS_B),
        MINIMAL_C => Some(MINIMAL_TOOLS_C),
        MINIMAL_DSH => Some(MINIMAL_DSH_TOOLS),
        _ => None,
    }
}

/// Project an internal tool key to its model-facing name.
///
/// Only `minimal:dsh` currently projects `bash_v2 -> bash`; every other
/// tool/mode combination is the identity function.
pub fn model_tool_name<'a>(mode: &str, internal_name: &'a str) -> &'a str {
    if is_minimal_dsh(mode) && internal_name == "bash_v2" {
        "bash"
    } else {
        internal_name
    }
}

/// Resolve a model-facing tool name back to the internal registration key.
///
/// This is the inverse projection used by permission admission and handler
/// dispatch, so the model never needs to know that `bash` is `bash_v2`.
pub fn internal_tool_name<'a>(mode: &str, model_name: &'a str) -> &'a str {
    if is_minimal_dsh(mode) && model_name == "bash" {
        "bash_v2"
    } else {
        model_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_modes_cover_the_persisted_contract() {
        for mode in KNOWN_MODES {
            assert!(is_known(mode), "{mode} should be known");
        }
        assert!(!is_known("turbo"));
        assert!(!is_known("minimal:future"));
    }

    #[test]
    fn preset_tools_are_stable_and_complete() {
        assert_eq!(preset_tools(MINIMAL), Some(MINIMAL_TOOLS));
        assert_eq!(preset_tools(MINIMAL_B), Some(MINIMAL_TOOLS_B));
        assert_eq!(preset_tools(MINIMAL_C), Some(MINIMAL_TOOLS_C));
        assert_eq!(preset_tools(MINIMAL_DSH), Some(MINIMAL_DSH_TOOLS));
        assert_eq!(preset_tools(STANDARD), None);
        assert_eq!(preset_tools(CUSTOM), None);
        assert_eq!(preset_tools(""), None);
    }

    #[test]
    fn minimal_dsh_never_leaks_bash_v2_to_the_model() {
        assert_eq!(model_tool_name(MINIMAL_DSH, "bash_v2"), "bash");
        assert_eq!(
            model_tool_name(MINIMAL_DSH, "str_replace_editor"),
            "str_replace_editor"
        );
        assert_eq!(model_tool_name(MINIMAL, "bash_v2"), "bash_v2");
    }

    #[test]
    fn minimal_dsh_execution_routes_bash_back_to_bash_v2() {
        assert_eq!(internal_tool_name(MINIMAL_DSH, "bash"), "bash_v2");
        assert_eq!(
            internal_tool_name(MINIMAL_DSH, "str_replace_editor"),
            "str_replace_editor"
        );
        assert_eq!(internal_tool_name(MINIMAL, "bash"), "bash");
        assert_eq!(internal_tool_name(STANDARD, "bash"), "bash");
        assert_eq!(internal_tool_name("", "bash"), "bash");
    }
}
