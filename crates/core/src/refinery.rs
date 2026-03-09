use std::path::{Path, PathBuf};
use std::process::Command;

use crate::copy::CopyManager;
use crate::db::Db;
use crate::types::{Id, MergeStatus};

#[derive(Debug)]
pub enum MergeOutcome {
    Merged,
    Conflicted(String),
    NeedsResolution {
        temp_dir: PathBuf,
        conflict_files: Vec<String>,
        conflict_diff: String,
    },
    VerifyFailed(String),
    Failed(String),
}

/// Run a git command in the given directory, returning stdout on success or an error message.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args.first().unwrap_or(&"")))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("git {} failed: {stderr}", args.join(" ")))
    }
}

/// Run a git commit with the copy manager's identity.
fn git_commit(copy_mgr: &CopyManager, dir: &Path, message: &str) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(["commit", "-m", message, "--no-verify"]);
    copy_mgr.git_identity().apply(&mut cmd);
    let output = cmd
        .current_dir(dir)
        .output()
        .map_err(|e| format!("failed to run git commit: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("git commit failed: {stderr}"))
    }
}

/// RAII guard that removes a directory on drop.
struct CleanupGuard(PathBuf);

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Process a single merge request via squash merge.
///
/// Worker branches are already in the source repo (created by git worktree),
/// so `git clone --shared` sees them as `origin/<branch>`. No fetch from
/// worker copies needed.
///
/// `commit_message` is used as the squash commit message.
pub fn process_merge(
    copy_mgr: &CopyManager,
    branch: &str,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
    commit_message: &str,
) -> MergeOutcome {
    let source = copy_mgr.project_root();

    // Step 1: Create a temporary shared clone of the source repo.
    // `--shared` reuses the object store — fast and space-efficient.
    // The clone already sees all branches including worktree-created ones.
    let tmp_dir = copy_mgr.copies_dir().join(format!(".merge-{}", mr_id));
    if tmp_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    let source_str = source.to_string_lossy();
    let tmp_str = tmp_dir.to_string_lossy();
    if let Err(e) = git(source, &["clone", "--shared", &source_str, &tmp_str]) {
        return MergeOutcome::Failed(format!("clone --shared: {e}"));
    }

    // Ensure cleanup on all exit paths.
    let _cleanup = CleanupGuard(tmp_dir.clone());

    // Step 2: Create a local tracking branch from the remote ref.
    // The shared clone sees the worktree branch as origin/<branch>.
    if let Err(e) = git(
        &tmp_dir,
        &["checkout", "-b", branch, &format!("origin/{branch}")],
    ) {
        return MergeOutcome::Failed(format!("checkout worker branch: {e}"));
    }

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Rebasing) {
        return MergeOutcome::Failed(format!("db update to rebasing: {e}"));
    }

    // Step 3: Verify the feature branch actually changed files.
    let default_tree = git(
        &tmp_dir,
        &["rev-parse", &format!("{default_branch}^{{tree}}")],
    );
    let branch_tree = git(&tmp_dir, &["rev-parse", &format!("{branch}^{{tree}}")]);

    match (default_tree, branch_tree) {
        (Ok(dt), Ok(bt)) if dt == bt => {
            return MergeOutcome::Failed(format!(
                "branch {branch} has no file changes vs {default_branch} — worker produced no output"
            ));
        }
        (Err(e), _) | (_, Err(e)) => {
            return MergeOutcome::Failed(format!("tree comparison failed: {e}"));
        }
        _ => {
            tracing::info!(branch, "feature branch has file changes, proceeding with merge");
        }
    }

    // Step 4: Squash-merge worker branch into default in the temp clone.
    if let Err(e) = git(&tmp_dir, &["checkout", default_branch]) {
        return MergeOutcome::Failed(format!("checkout {default_branch} in temp: {e}"));
    }

    if git(&tmp_dir, &["merge", "--squash", branch]).is_err() {
        // Squash merge had conflicts — check if they need agent help.
        let conflict_files = match git(&tmp_dir, &["diff", "--name-only", "--diff-filter=U"]) {
            Ok(files) => files.lines().map(String::from).collect::<Vec<_>>(),
            Err(e) => return MergeOutcome::Failed(format!("list conflict files: {e}")),
        };

        if conflict_files.is_empty() {
            let _ = git(&tmp_dir, &["reset", "--hard"]);
            return MergeOutcome::Conflicted(format!(
                "merge of {branch} onto {default_branch} failed with no identifiable conflicts"
            ));
        }

        let conflict_diff = git(&tmp_dir, &["diff"]).unwrap_or_default();

        tracing::info!(
            branch, default_branch,
            conflict_count = conflict_files.len(),
            "merge conflict detected — requesting resolution"
        );

        // Prevent RAII cleanup so the merger agent can work in this temp dir.
        // The dir is cleaned up by `finish_merge()` on the happy path, or by
        // `CopyManager::cleanup_orphaned_merge_dirs()` on session startup if
        // the agent dies or the session is killed before resolution completes.
        std::mem::forget(_cleanup);

        return MergeOutcome::NeedsResolution {
            temp_dir: tmp_dir,
            conflict_files,
            conflict_diff,
        };
    }

    // Squash staged all changes — create the commit.
    if let Err(e) = git_commit(copy_mgr, &tmp_dir, commit_message) {
        return MergeOutcome::Failed(format!("squash commit: {e}"));
    }

    // Continue to verify + bring back to source.
    finish_merge_inner(copy_mgr, &tmp_dir, default_branch, db, mr_id)
}

/// Complete a merge after conflicts have been resolved by the merger agent.
///
/// Creates the squash commit, runs verify.sh, then fetches the result back
/// to the source repo via ff-only merge. Cleans up the temp dir on all exit paths.
pub fn finish_merge(
    copy_mgr: &CopyManager,
    temp_dir: &Path,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
    commit_message: &str,
) -> MergeOutcome {
    let _cleanup = CleanupGuard(temp_dir.to_path_buf());

    // The merger agent resolved conflicts and staged files, but did not commit.
    if let Err(e) = git_commit(copy_mgr, temp_dir, commit_message) {
        return MergeOutcome::Failed(format!("squash commit after resolution: {e}"));
    }

    finish_merge_inner(copy_mgr, temp_dir, default_branch, db, mr_id)
}

/// Shared logic: verify.sh, then fetch+ff-only back to source.
fn finish_merge_inner(
    copy_mgr: &CopyManager,
    tmp_dir: &Path,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
) -> MergeOutcome {
    let source = copy_mgr.project_root();
    let tmp_str = tmp_dir.to_string_lossy();

    // Run verify.sh if present.
    let verify_script = source.join(".enki/verify.sh");
    if verify_script.exists() {
        if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Verifying) {
            return MergeOutcome::Failed(format!("db update to verifying: {e}"));
        }

        let verify_result = Command::new("bash")
            .arg(&verify_script)
            .current_dir(tmp_dir)
            .output();

        match verify_result {
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let detail = if stderr.is_empty() {
                    stdout.to_string()
                } else {
                    format!("{stdout}\n{stderr}")
                };
                return MergeOutcome::VerifyFailed(detail.trim().to_string());
            }
            Err(e) => {
                return MergeOutcome::Failed(format!("verify.sh execution error: {e}"));
            }
            Ok(_) => {} // verification passed
        }
    }

    // Bring result back to source safely.
    if let Err(e) = git(source, &["fetch", &tmp_str, default_branch]) {
        return MergeOutcome::Failed(format!("fetch result back to source: {e}"));
    }

    if let Err(e) = git(source, &["merge", "--ff-only", "FETCH_HEAD"]) {
        return MergeOutcome::Failed(format!("ff-only merge into source: {e}"));
    }

    // Clean up the task branch from source.
    let _ = git(source, &["branch", "-D", &format!("task/{}", mr_id)]);

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Merged) {
        return MergeOutcome::Failed(format!("db update to merged: {e}"));
    }

    MergeOutcome::Merged
}
