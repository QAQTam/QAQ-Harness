//! Git utility functions — thin wrappers around git2 for backend use.
//!
//! These are NOT LLM-invocable tools. They are called directly by the
//! Git service functions exposed to clients through the daemon control protocol.

use git2::{DiffOptions, Repository};
use std::path::Path;

/// Open a git repository at the given path.
fn open_repo(path: &str) -> Result<Repository, String> {
    Repository::open(Path::new(path)).map_err(|e| format!("open repo: {e}"))
}

/// Get working-tree status as a JSON array of `{path, change, lines_added, lines_removed}`.
pub fn status_json(workspace: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;
    let mut files: Vec<serde_json::Value> = Vec::new();

    let statuses = repo.statuses(None).map_err(|e| format!("status: {e}"))?;
    for entry in statuses.iter() {
        let path = entry.path().unwrap_or("").to_string();
        let status = entry.status();
        let change = if status.is_index_new() || status.is_wt_new() {
            "added"
        } else if status.is_index_deleted() || status.is_wt_deleted() {
            "deleted"
        } else if status.is_index_modified() || status.is_wt_modified() {
            "modified"
        } else if status.is_index_renamed() || status.is_wt_renamed() {
            "renamed"
        } else {
            continue;
        };

        let (lines_added, lines_removed) = (0, 0);
        // FIXME(git2-isolation): diff_tree_to_workdir_with_index 的 pathspec 过滤可能失效，
        // 导致全量 diff 计算拖垮 tokio 运行时(code=1006)。注释掉待验证。
        // 复用前：if matches!(change, "modified" | "added") {
        //     let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        //     let mut opts = DiffOptions::new();
        //     opts.pathspec(&path);
        //     head_tree
        //         .and_then(|tree| {
        //             repo.diff_tree_to_workdir_with_index(Some(&tree), Some(&mut opts))
        //                 .ok()
        //         })
        //         .and_then(|d| d.stats().ok())
        //         .map(|s| (s.insertions() as u32, s.deletions() as u32))
        //         .unwrap_or((0, 0))
        // } else {
        //     (0, 0)
        // };

        files.push(serde_json::json!({
            "path": path,
            "change": change,
            "lines_added": lines_added,
            "lines_removed": lines_removed,
        }));
    }

    serde_json::to_string(&files).map_err(|e| format!("serialize: {e}"))
}

/// Get the current branch name (shorthand). Returns empty string if detached HEAD.
pub fn current_branch(workspace: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;
    let head = repo.head().map_err(|_| "no HEAD")?;
    if head.is_branch() {
        Ok(head.shorthand().unwrap_or("HEAD").to_string())
    } else {
        Ok("HEAD".into())
    }
}

/// List all local branches as a JSON array of `{name, current}`.
pub fn list_branches(workspace: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;
    let head_name = repo
        .head()
        .ok()
        .map(|h| h.shorthand().ok().unwrap_or("HEAD").to_string());

    let mut branches: Vec<serde_json::Value> = Vec::new();
    if let Ok(iter) = repo.branches(Some(git2::BranchType::Local)) {
        for b in iter.flatten() {
            let name = b.0.name().ok().flatten().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            branches.push(serde_json::json!({
                "name": name,
                "current": head_name.as_deref() == Some(&name),
            }));
        }
    }
    serde_json::to_string(&branches).map_err(|e| format!("serialize: {e}"))
}

/// Switch to a branch. If `stash` is true, stash uncommitted changes first and
/// pop them after switching. Returns the new branch name.
pub fn switch_branch(workspace: &str, branch: &str, stash: bool) -> Result<String, String> {
    let mut repo = open_repo(workspace)?;

    let has_changes = repo.statuses(None).map(|s| !s.is_empty()).unwrap_or(false);
    let mut stashed = false;

    if stash && has_changes {
        let sig = git2::Signature::now("QAQ-Harness", "qaqh@local")
            .map_err(|e| format!("signature: {e}"))?;
        repo.stash_save(&sig, "qaqh-auto-stash", None)
            .map_err(|e| format!("stash: {e}"))?;
        stashed = true;
    }

    {
        let branch_ref = repo
            .find_branch(branch, git2::BranchType::Local)
            .map_err(|e| format!("find branch '{}': {}", branch, e))?;
        let obj = branch_ref
            .get()
            .peel(git2::ObjectType::Tree)
            .map_err(|e| format!("peel: {e}"))?;

        let mut checkout_opts = git2::build::CheckoutBuilder::new();
        checkout_opts.safe();
        repo.checkout_tree(&obj, Some(&mut checkout_opts))
            .map_err(|e| format!("checkout tree: {e}"))?;
        repo.set_head(branch_ref.get().name().ok().unwrap_or(""))
            .map_err(|e| format!("set HEAD: {e}"))?;
    }

    if stashed && let Err(e) = repo.stash_pop(0, None) {
        log::warn!("stash pop failed (likely conflict, stash kept): {e}");
    }

    let new_head = repo
        .head()
        .ok()
        .map(|h| h.shorthand().unwrap_or("HEAD").to_string())
        .unwrap_or_default();
    Ok(new_head)
}

/// Stage all changes and commit with the given message. Returns the commit OID.
pub fn commit_all(workspace: &str, message: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;

    let mut index = repo.index().map_err(|e| format!("index: {e}"))?;
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(|e| format!("add_all: {e}"))?;
    index.write().map_err(|e| format!("index write: {e}"))?;

    let tree_oid = index.write_tree().map_err(|e| format!("write_tree: {e}"))?;
    let tree = repo
        .find_tree(tree_oid)
        .map_err(|e| format!("find_tree: {e}"))?;

    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();

    let sig =
        git2::Signature::now("QAQ-Harness", "qaqh@local").map_err(|e| format!("signature: {e}"))?;

    let oid = repo
        .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .map_err(|e| format!("commit: {e}"))?;

    Ok(oid.to_string())
}

/// Get the diff for a single file (HEAD vs index + working tree) as a unified patch.
/// `*_with_index` includes both staged and unstaged edits, matching the status list.
pub fn file_diff(workspace: &str, file_path: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;
    let head = repo.head().map_err(|e| format!("head: {e}"))?;
    let head_tree = head.peel_to_tree().map_err(|e| format!("tree: {e}"))?;

    let mut diff_opts = DiffOptions::new();
    diff_opts.pathspec(file_path);

    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut diff_opts))
        .map_err(|e| format!("diff: {e}"))?;

    let mut patch_text = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        let origin = line.origin();
        let content = std::str::from_utf8(line.content()).unwrap_or("");
        // FileHeader ('F') / HunkHeader ('H') 行的 content 已是完整行
        // （"diff --git ...\n" / "@@ ...\n"），拼 origin 会把 'F'/'H' 混入
        // 输出，破坏 unified diff 格式（from_buffer 将无法解析）。
        match origin {
            'F' | 'H' => patch_text.push_str(content),
            _ => {
                patch_text.push(origin);
                patch_text.push_str(content);
            }
        }
        true
    })
    .map_err(|e| format!("print diff: {e}"))?;

    Ok(patch_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn branch_switch_without_stash_preserves_dirty_worktree() {
        let dir = tempfile::tempdir().expect("temp repo");
        let repo = Repository::init(dir.path()).expect("init repo");
        fs::write(dir.path().join("tracked.txt"), "committed\n").expect("write fixture");

        let mut index = repo.index().expect("index");
        index
            .add_path(Path::new("tracked.txt"))
            .expect("stage fixture");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature =
            git2::Signature::now("QAQ-Harness Test", "qaqh-test@local").expect("signature");
        let commit_id = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("initial commit");
        let commit = repo.find_commit(commit_id).expect("find commit");
        repo.branch("feature", &commit, false)
            .expect("create branch");
        let original_branch = current_branch(dir.path().to_str().unwrap()).expect("current branch");

        fs::write(dir.path().join("tracked.txt"), "uncommitted\n").expect("dirty fixture");

        let result = switch_branch(dir.path().to_str().unwrap(), "feature", false);

        assert_eq!(
            fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "uncommitted\n"
        );
        let branch_after = current_branch(dir.path().to_str().unwrap()).unwrap();
        if result.is_err() {
            assert_eq!(branch_after, original_branch);
        }
        assert!(branch_after == original_branch || branch_after == "feature");
    }
}
