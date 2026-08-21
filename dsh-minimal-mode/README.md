# dsh-minimal-mode

Faithful Rust replication of the deepseek-harness **minimal-mode** tool set, dropped
into the QAQ-Harness workspace as an isolated crate.

Source of truth: `F:\deepseek-harness-winui\minimal-mode-extraction`
(itself an exact copy of `packages/shell/tool-bash-persistent`,
`packages/fs/tool-str-replace-editor`, `packages/terminal/*`, `packages/fs/fs-local`).

## Tools

| Tool name            | Description                                                          |
|----------------------|----------------------------------------------------------------------|
| `bash_v2`            | Persistent PTY bash (portable-pty + line scrollback), verbatim output |
| `str_replace_editor` | `view` / `create` / `str_replace` / `insert`, verbatim output        |

## Requirements honoured

1. **Verbatim returns** — every model-facing string (success and error) matches
   minimal-mode word-for-word: `[exit code: N]`, `Here's the content of <path>
   with line numbers ...`, `New file created successfully at: ...`,
   `The file <path> has been edited successfully.`, the timeout block, the
   `<response clipped><NOTE>...` truncation marker, and the `LOST_PREFIX_MESSAGE`
   / `SHELL_RESET_MESSAGE` constants.

2. **Internal logic aligned** — `bash_v2` mirrors the minimal-mode control flow:
   - marker wrapping (`printf '%s\n' <start>; eval -- <cmd>; status=$?;
     printf '%s%s\n' <end> "$status"`),
   - `$'...'` quoting, prompt stripping, `commandOutput`/`partialOutput`
     parsing, `renderCaptured`/`renderShellExitStatus`,
   - scrollback line paging (`SCROLLBACK_PAGE_LINES` = 1000, 25 ms poll),
   - 300 s default timeout and 16 000-char output cap.
   `str_replace_editor` mirrors `view`/`create`/`str_replace`/`insert` exactly,
   including absolute-path checks, `old_str` uniqueness, `insert_line` bounds,
   directory listing exclusions (hidden, `node_modules`, `__pycache__`), and the
   `FS_*` error messages.

3. **No conflict with existing QAQ-Harness** — QAQ-Harness already ships a non-persistent
   `bash` tool (pipe-based, JSON envelope). This crate registers the persistent
   shell under **`bash_v2`** and adds `str_replace_editor` (which QAQ-Harness does not
   have). A future tool whitelist can re-map `bash_v2` to the canonical `bash`.

## Integration

- New workspace member crate (added to root `Cargo.toml` members).
- `dsh_minimal_mode::register` implements
  `qaqh_workspace::registration::ToolRegistrar` and registers both tools onto a
  `ToolManager`.
- Wired into the agent tool runtime in
  `crates/qaqh-msgloop/src/state/agent.rs` (`AgentState::init` and
  `init_subagent`) so the tools execute in-process via
  `qaqh_workspace::execution::execute_with_context`.

## Files

- `src/lib.rs` — crate root + `register()`.
- `src/editor.rs` — `str_replace_editor` (self-contained `std::fs`).
- `src/bash.rs` — `bash_v2` persistent shell tool + verbatim rendering logic.
- `src/pty.rs` — line scrollback + portable-pty session (with a minimal
  terminal-emulator DSR responder so bash's readline does not block).

## Tests

```
cargo test -p dsh-minimal-mode
```

Covers the verbatim strings/error messages (unit) and full ToolManager-driven
execution of both tools, including `bash_v2` persistent state across two calls
(integration).
