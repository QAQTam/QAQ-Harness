//! Ported Codex apply-patch scenario suite (`*** Begin Patch` format).
//!
//! Each fixture directory: `patch.txt` + `input/` (initial state) + `expected/`
//! (final state after applying the patch). Assertions are purely on the final
//! filesystem snapshot (byte-exact), matching the upstream runner; scenario
//! names containing rejects/requires/fails/empty additionally must surface an
//! engine error.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use qaqh_workspace::apply_patch_engine::{UpdateMode, apply_patch_engine};
use tempfile::tempdir;

#[test]
fn apply_patch_engine_scenarios() {
    let scenarios_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/apply_patch_scenarios");
    let mut ran = 0usize;
    for entry in fs::read_dir(&scenarios_dir).expect("scenarios dir") {
        let entry = entry.expect("valid dir entry");
        if !entry.path().is_dir() {
            continue;
        }
        run_scenario(&entry.path());
        ran += 1;
    }
    assert!(ran >= 25, "expected >= 25 scenarios, ran {ran}");
}

fn run_scenario(dir: &Path) {
    let name = dir
        .file_name()
        .expect("scenario dir has a name")
        .to_string_lossy()
        .to_string();
    let tmp = tempdir().expect("tempdir");

    let input_dir = dir.join("input");
    if input_dir.is_dir() {
        copy_dir_recursive(&input_dir, tmp.path());
    }

    let patch = fs::read_to_string(dir.join("patch.txt"))
        .unwrap_or_else(|e| panic!("{name}: read patch.txt: {e}"));

    // Run with PreserveLineEndings, matching the upstream runner (fixtures
    // 023/024 exercise CRLF / mixed line endings).
    let result = apply_patch_engine(&patch, tmp.path(), UpdateMode::PreserveLineEndings);

    let expects_error = name.contains("rejects")
        || name.contains("requires")
        || name.contains("fails")
        || name.contains("empty");
    if expects_error {
        assert!(
            result.is_err(),
            "{name}: expected engine error, got Ok with outcome {result:?}"
        );
    }

    let expected_snapshot = snapshot_dir(&dir.join("expected"));
    let actual_snapshot = snapshot_dir(tmp.path());
    assert_eq!(
        actual_snapshot,
        expected_snapshot,
        "{name}: final filesystem state mismatch (input dir: {})",
        input_dir.display()
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File(Vec<u8>),
    Dir,
}

fn snapshot_dir(root: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut entries = BTreeMap::new();
    if root.is_dir() {
        snapshot_dir_recursive(root, root, &mut entries);
    }
    entries
}

fn snapshot_dir_recursive(base: &Path, dir: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("valid dir entry");
        let rel = entry
            .path()
            .strip_prefix(base)
            .expect("entry under base")
            .to_path_buf();
        if entry.file_type().expect("file_type").is_dir() {
            entries.insert(rel.clone(), Entry::Dir);
            snapshot_dir_recursive(base, &entry.path(), entries);
        } else {
            entries.insert(
                rel,
                Entry::File(fs::read(entry.path()).expect("read scenario file")),
            );
        }
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("valid dir entry");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("file_type").is_dir() {
            fs::create_dir_all(&target).expect("create dir");
            copy_dir_recursive(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy scenario file");
        }
    }
}
