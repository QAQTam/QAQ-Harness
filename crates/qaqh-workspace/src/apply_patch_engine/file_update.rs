//! Chunk → replacements → new file contents.
//! Ported from OpenAI `codex-rs/apply-patch` (Apache-2.0); sync + std-fs.

use std::path::Path;

use crate::apply_patch_engine::EngineError;
use crate::apply_patch_engine::UpdateFileChunk;
use crate::apply_patch_engine::UpdateMode;
use crate::apply_patch_engine::seek_sequence::seek_sequence;
use crate::apply_patch_engine::text_file::Replacement;
use crate::apply_patch_engine::text_file::SourceFile;

pub struct AppliedPatch {
    pub original_contents: String,
    pub new_contents: String,
}

/// Compute the new file contents after applying the chunks, reading the
/// current file contents from disk.
pub fn derive_new_contents_from_chunks(
    path: &Path,
    chunks: &[UpdateFileChunk],
    update_file_mode: UpdateMode,
) -> Result<AppliedPatch, EngineError> {
    let original_contents = std::fs::read_to_string(path).map_err(|e| EngineError::Io {
        context: format!("Failed to read file to update {}", path.to_string_lossy()),
        source: e,
    })?;

    let path_text = path.to_string_lossy().to_string();
    let new_contents = match update_file_mode {
        UpdateMode::NormalizeToLf => {
            let mut original_lines = original_contents
                .split('\n')
                .map(String::from)
                .collect::<Vec<_>>();

            // Drop the trailing empty element that results from the final newline so
            // that line counts match the behaviour of standard `diff`.
            if original_lines.last().is_some_and(String::is_empty) {
                original_lines.pop();
            }

            let replacements =
                compute_replacements(&original_lines, &path_text, chunks, update_file_mode)?;
            let mut new_lines = apply_replacements(original_lines, &replacements);
            if !new_lines.last().is_some_and(String::is_empty) {
                new_lines.push(String::new());
            }
            new_lines.join("\n")
        }
        UpdateMode::PreserveLineEndings => {
            let mut source_file = SourceFile::parse(&original_contents);
            let original_lines = source_file.line_texts();
            let replacements =
                compute_replacements(&original_lines, &path_text, chunks, update_file_mode)?;
            source_file.apply_replacements(&replacements);
            source_file.into_contents()
        }
    };
    Ok(AppliedPatch {
        original_contents,
        new_contents,
    })
}

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`. Each replacement is returned as
/// `(start_index, old_len, new_lines)`.
fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
    update_file_mode: UpdateMode,
) -> Result<Vec<Replacement>, EngineError> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        // If a chunk has a `change_context`, we use seek_sequence to find it, then
        // adjust our `line_index` to continue from there.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                /*eof*/ false,
                update_file_mode,
            ) {
                line_index = idx + 1;
            } else {
                return Err(EngineError::Compute(format!(
                    "Failed to find context '{ctx_line}' in {path}"
                )));
            }
        }

        if chunk.old_lines.is_empty() {
            // Preserve the legacy split representation's handling of a final
            // empty line. `SourceFile` only exposes real source lines, so its
            // insertion point is always after the final line.
            let insertion_idx = match update_file_mode {
                UpdateMode::NormalizeToLf => {
                    if original_lines.last().is_some_and(String::is_empty) {
                        original_lines.len() - 1
                    } else {
                        original_lines.len()
                    }
                }
                UpdateMode::PreserveLineEndings => original_lines.len(),
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        // Otherwise, try to match the existing lines in the file with the old lines
        // from the chunk. If found, schedule that region for replacement.
        // In many real-world diffs the last element of `old_lines` is an *empty*
        // string representing the terminating newline of the region being
        // replaced. This sentinel is not present in `original_lines` because
        // `SourceFile` stores the terminator on the preceding line. If a direct
        // search fails and the pattern ends with an empty string, retry without
        // that final element so modifications touching the end-of-file can be
        // located reliably.

        let mut pattern: &[String] = &chunk.old_lines;
        let mut found = seek_sequence(
            original_lines,
            pattern,
            line_index,
            chunk.is_end_of_file,
            update_file_mode,
        );

        let mut new_slice: &[String] = &chunk.new_lines;

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            // Retry without the trailing empty line which represents the final
            // newline in the file.
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }

            found = seek_sequence(
                original_lines,
                pattern,
                line_index,
                chunk.is_end_of_file,
                update_file_mode,
            );
        }

        if let Some(start_idx) = found {
            match update_file_mode {
                UpdateMode::NormalizeToLf => {
                    replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
                }
                UpdateMode::PreserveLineEndings => {
                    // Context lines occur in both sides of a patch chunk. Keep those
                    // original lines in place so their exact contents and terminators
                    // survive, especially when the file has mixed line endings.
                    let mut old_start = 0;
                    let mut new_start = 0;
                    for &(old_context, new_context) in &chunk.context_line_indices {
                        // A trailing empty context line can be removed from `pattern`
                        // and `new_slice` above when it represents the final newline.
                        if old_context >= pattern.len() || new_context >= new_slice.len() {
                            break;
                        }
                        if old_start != old_context || new_start != new_context {
                            replacements.push((
                                start_idx + old_start,
                                old_context - old_start,
                                new_slice[new_start..new_context].to_vec(),
                            ));
                        }
                        old_start = old_context + 1;
                        new_start = new_context + 1;
                    }
                    if old_start != pattern.len() || new_start != new_slice.len() {
                        replacements.push((
                            start_idx + old_start,
                            pattern.len() - old_start,
                            new_slice[new_start..].to_vec(),
                        ));
                    }
                }
            }
            line_index = start_idx + pattern.len();
        } else {
            return Err(EngineError::Compute(format!(
                "Failed to find expected lines in {}:\n{}",
                path,
                chunk.old_lines.join("\n"),
            )));
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);

    Ok(replacements)
}

fn apply_replacements(mut lines: Vec<String>, replacements: &[Replacement]) -> Vec<String> {
    for (start_idx, old_len, new_lines) in replacements {
        lines.splice(*start_idx..*start_idx + *old_len, new_lines.iter().cloned());
    }
    lines
}
