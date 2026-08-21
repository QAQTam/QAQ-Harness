//! Codex-style apply-patch engine (ported from OpenAI `codex-rs/apply-patch`,
//! Apache-2.0). Self-contained parser + content matcher for the
//! `*** Begin Patch` patch format — NO line numbers, NO unified-diff headers.
//!
//! Ported surface (sync, std-fs, PathBuf instead of PathUri/ExecutorFileSystem):
//! - `parser.rs`     — boundary checks + lenient heredoc stripping
//! - `streaming_parser.rs` — hunk state machine (lines → Hunk list)
//! - `seek_sequence.rs`    — 4-tier content matching (exact → rstrip → trim → Unicode-normalised)
//! - `file_update.rs`      — chunk → replacements → new contents
//! - `text_file.rs`        — line-ending-preserving source file abstraction
//!
//! Update semantics: all hunks located on the CURRENT file state, applied in
//! order; any failure rejects the whole patch (all-or-nothing by construction —
//! replacements are computed before any write). Paths resolve relative to the
//! caller-supplied cwd.

mod file_update;
mod parser;
mod seek_sequence;
mod streaming_parser;
mod text_file;

use std::fmt;
use std::path::{Path, PathBuf};

pub use file_update::{AppliedPatch, derive_new_contents_from_chunks};
pub use parser::{Hunk, ParseError, UpdateFileChunk, parse_patch};
pub use streaming_parser::StreamingPatchParser;

/// Controls how updates reconstruct the target file after matching a patch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UpdateMode {
    /// Preserve the historical behavior of normalizing updated files to LF
    /// (matches the workspace LF-canonical-view contract of read/hash).
    #[default]
    NormalizeToLf,
    /// Preserve existing line endings and use the file's preferred ending for new lines.
    PreserveLineEndings,
}

#[derive(Debug)]
pub enum EngineError {
    Parse(ParseError),
    Io {
        context: String,
        source: std::io::Error,
    },
    Compute(String),
    EmptyPatch,
    /// A patch path resolved outside the workspace root (or failed to resolve).
    PathOutsideWorkspace {
        path: String,
    },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Parse(e) => write!(f, "invalid patch: {e}"),
            EngineError::Io { context, source } => write!(f, "{context}: {source}"),
            EngineError::Compute(msg) => write!(f, "{msg}"),
            EngineError::EmptyPatch => write!(f, "no changes to apply"),
            EngineError::PathOutsideWorkspace { path } => {
                write!(f, "patch path resolves outside the workspace: {path}")
            }
        }
    }
}

impl From<ParseError> for EngineError {
    fn from(e: ParseError) -> Self {
        EngineError::Parse(e)
    }
}

impl From<std::io::Error> for EngineError {
    fn from(err: std::io::Error) -> Self {
        EngineError::Io {
            context: "I/O error".to_string(),
            source: err,
        }
    }
}

/// Per-file outcome of an applied patch (paths keep the patch's spelling).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AffectedPaths {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl AffectedPaths {
    pub fn all(&self) -> impl Iterator<Item = &String> {
        self.added.iter().chain(&self.modified).chain(&self.deleted)
    }
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// Per-file delta produced while applying (paths keep the patch's spelling;
/// contents are the actual bytes read/written — used for stats and ledger sync).
#[derive(Debug, Clone, PartialEq)]
pub struct FileDelta {
    pub path: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Outcome of applying (or dry-running) a patch.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ApplyOutcome {
    pub affected: AffectedPaths,
    pub deltas: Vec<FileDelta>,
}

/// Resolve a hunk path against the workspace root. Relative paths are joined to
/// `cwd`; absolute paths are used as-is. Anything escaping the workspace root is
/// rejected (the tool's permission model is workspace-bounded).
pub(crate) fn resolve_workspace_path(cwd: &Path, path: &Path) -> Result<PathBuf, EngineError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    // Canonicalize the parent so `..` escapes are caught; the file itself may
    // not exist yet (Add), so canonicalize the deepest existing ancestor.
    let abs = if joined.exists() {
        joined.canonicalize().unwrap_or(joined.clone())
    } else {
        match joined.parent() {
            Some(parent) if parent.exists() => {
                let canon_parent = parent
                    .canonicalize()
                    .unwrap_or_else(|_| parent.to_path_buf());
                canon_parent.join(joined.file_name().unwrap_or_default())
            }
            _ => joined.clone(),
        }
    };
    let cwd_abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    // Windows canonicalize returns a `\\?\`-prefixed verbatim path; strip it on
    // both sides so the prefix comparison is meaningful.
    let strip_verbatim = |p: &Path| -> PathBuf {
        let s = p.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        PathBuf::from(s)
    };
    if !strip_verbatim(&abs).starts_with(strip_verbatim(&cwd_abs)) {
        return Err(EngineError::PathOutsideWorkspace {
            path: joined.to_string_lossy().to_string(),
        });
    }
    Ok(abs)
}

/// Apply a `*** Begin Patch` patch to the workspace rooted at `cwd`.
pub fn apply_patch_engine(
    patch: &str,
    cwd: &Path,
    mode: UpdateMode,
) -> Result<ApplyOutcome, EngineError> {
    let hunks = parse_patch(patch)?.hunks;
    if hunks.is_empty() {
        return Err(EngineError::EmptyPatch);
    }
    let mut outcome = ApplyOutcome::default();
    for hunk in &hunks {
        let affected_path = hunk.path().to_string_lossy().to_string();
        let resolved = resolve_workspace_path(cwd, hunk.path())?;
        match hunk {
            Hunk::AddFile { contents, .. } => {
                write_file_with_missing_parent_retry(&resolved, contents.as_bytes())?;
                outcome.affected.added.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    old: None,
                    new: Some(contents.clone()),
                });
            }
            Hunk::DeleteFile { .. } => {
                let old = std::fs::read_to_string(&resolved).ok();
                let meta = std::fs::metadata(&resolved).map_err(|e| EngineError::Io {
                    context: format!("Failed to delete file {}", resolved.to_string_lossy()),
                    source: e,
                })?;
                if meta.is_dir() {
                    return Err(EngineError::Io {
                        context: format!(
                            "Failed to delete file {}: path is a directory",
                            resolved.to_string_lossy()
                        ),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "path is a directory",
                        ),
                    });
                }
                std::fs::remove_file(&resolved).map_err(|e| EngineError::Io {
                    context: format!("Failed to delete file {}", resolved.to_string_lossy()),
                    source: e,
                })?;
                outcome.affected.deleted.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    old,
                    new: None,
                });
            }
            Hunk::UpdateFile {
                path: src_path,
                move_path,
                chunks,
            } => {
                let resolved = resolve_workspace_path(cwd, src_path)?;
                let applied = derive_new_contents_from_chunks(&resolved, chunks, mode).map_err(
                    |e| match e {
                        EngineError::Compute(msg) => EngineError::Compute(format!(
                            "Failed to update file {}: {msg}",
                            resolved.to_string_lossy()
                        )),
                        other => other,
                    },
                )?;
                if let Some(dest) = move_path {
                    let dest_resolved = resolve_workspace_path(cwd, dest)?;
                    write_file_with_missing_parent_retry(
                        &dest_resolved,
                        applied.new_contents.as_bytes(),
                    )?;
                    let meta = std::fs::metadata(&resolved).map_err(|e| EngineError::Io {
                        context: format!(
                            "Failed to remove original {}",
                            resolved.to_string_lossy()
                        ),
                        source: e,
                    })?;
                    if meta.is_dir() {
                        return Err(EngineError::Io {
                            context: format!(
                                "Failed to remove original {}: path is a directory",
                                resolved.to_string_lossy()
                            ),
                            source: std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "path is a directory",
                            ),
                        });
                    }
                    std::fs::remove_file(&resolved).map_err(|e| EngineError::Io {
                        context: format!(
                            "Failed to remove original {}",
                            resolved.to_string_lossy()
                        ),
                        source: e,
                    })?;
                } else {
                    std::fs::write(&resolved, applied.new_contents.as_bytes()).map_err(|e| {
                        EngineError::Io {
                            context: format!("Failed to write file {}", resolved.to_string_lossy()),
                            source: e,
                        }
                    })?;
                }
                outcome.affected.modified.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    old: Some(applied.original_contents),
                    new: Some(applied.new_contents),
                });
            }
        }
    }
    Ok(outcome)
}

/// Dry-run: parse and fully compute every hunk against the current file state,
/// but write nothing. Every failure that a real apply would hit surfaces here.
pub fn dry_run_patch_engine(patch: &str, cwd: &Path) -> Result<ApplyOutcome, EngineError> {
    let hunks = parse_patch(patch)?.hunks;
    if hunks.is_empty() {
        return Err(EngineError::EmptyPatch);
    }
    let mut outcome = ApplyOutcome::default();
    for hunk in &hunks {
        let affected_path = hunk.path().to_string_lossy().to_string();
        let resolved = resolve_workspace_path(cwd, hunk.path())?;
        match hunk {
            Hunk::AddFile { contents, .. } => {
                if std::fs::metadata(&resolved).is_ok_and(|m| m.is_dir()) {
                    return Err(EngineError::Io {
                        context: format!(
                            "Cannot add file {}: path is a directory",
                            resolved.to_string_lossy()
                        ),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "path is a directory",
                        ),
                    });
                }
                outcome.affected.added.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    old: None,
                    new: Some(contents.clone()),
                });
            }
            Hunk::DeleteFile { .. } => {
                let old = std::fs::read_to_string(&resolved).ok();
                let meta = std::fs::metadata(&resolved).map_err(|e| EngineError::Io {
                    context: format!("Failed to delete file {}", resolved.to_string_lossy()),
                    source: e,
                })?;
                if meta.is_dir() {
                    return Err(EngineError::Io {
                        context: format!(
                            "Failed to delete file {}: path is a directory",
                            resolved.to_string_lossy()
                        ),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "path is a directory",
                        ),
                    });
                }
                outcome.affected.deleted.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    old,
                    new: None,
                });
            }
            Hunk::UpdateFile { chunks, .. } => {
                // Full compute (read + match + rebuild) — same failure surface
                // as a real apply, minus the write.
                let applied =
                    derive_new_contents_from_chunks(&resolved, chunks, UpdateMode::default())
                        .map_err(|e| match e {
                            EngineError::Compute(msg) => EngineError::Compute(format!(
                                "Failed to update file {}: {msg}",
                                resolved.to_string_lossy()
                            )),
                            other => other,
                        })?;
                outcome.affected.modified.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    old: Some(applied.original_contents),
                    new: Some(applied.new_contents),
                });
            }
        }
    }
    Ok(outcome)
}

fn write_file_with_missing_parent_retry(path: &Path, contents: &[u8]) -> Result<(), EngineError> {
    match std::fs::write(path, contents) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| EngineError::Io {
                    context: format!(
                        "Failed to create parent directories for {}",
                        path.to_string_lossy()
                    ),
                    source: e,
                })?;
            }
            std::fs::write(path, contents).map_err(|e| EngineError::Io {
                context: format!("Failed to write file {}", path.to_string_lossy()),
                source: e,
            })
        }
        Err(err) => Err(EngineError::Io {
            context: format!("Failed to write file {}", path.to_string_lossy()),
            source: err,
        }),
    }
}
