use std::path::Path;
use std::process::Command;

use crate::db::Db;
use crate::types::{Id, MergeStatus};
use crate::worktree::CopyManager;

#[derive(Debug)]
pub enum MergeOutcome {
    Merged,
    Conflicted(String),
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

/// Clean up temp-merge branch, ignoring errors (best-effort).
fn cleanup_temp_merge(dir: &Path, default_branch: &str) {
    let _ = git(dir, &["checkout", default_branch]);
    let _ = git(dir, &["branch", "-D", "temp-merge"]);
}

/// Process a single merge request.
///
/// New flow (APFS-copy-based):
/// 1. Fetch the worker's branch from the copy into the source repo
/// 2. Check that the branch has actual file changes
/// 3. Rebase onto the default branch via temp-merge
/// 4. Run verify.sh if present
/// 5. Fast-forward merge into default branch
/// 6. Clean up the task branch
pub fn process_merge(
    copy_mgr: &CopyManager,
    copy_path: &Path,
    branch: &str,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
) -> MergeOutcome {
    let source = copy_mgr.project_root();

    // Step 1: Fetch worker's branch from copy into source repo.
    if let Err(e) = copy_mgr.fetch_branch(copy_path, branch) {
        return MergeOutcome::Failed(format!("fetch from copy: {e}"));
    }

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Rebasing) {
        return MergeOutcome::Failed(format!("db update to rebasing: {e}"));
    }

    // Step 2: Verify the feature branch actually changed files.
    let default_tree = git(source, &["rev-parse", &format!("{default_branch}^{{tree}}")]);
    let branch_tree = git(source, &["rev-parse", &format!("{branch}^{{tree}}")]);

    match (default_tree, branch_tree) {
        (Ok(dt), Ok(bt)) => {
            if dt == bt {
                let _ = copy_mgr.delete_branch(branch);
                return MergeOutcome::Failed(format!(
                    "branch {branch} has no file changes vs {default_branch} — worker produced no output"
                ));
            }
            tracing::info!(branch, "feature branch has file changes, proceeding with merge");
        }
        (Err(e), _) | (_, Err(e)) => {
            let _ = copy_mgr.delete_branch(branch);
            return MergeOutcome::Failed(format!("tree comparison failed: {e}"));
        }
    }

    // Step 3: Create temp branch from feature branch and rebase onto default.
    if let Err(e) = git(source, &["checkout", "-b", "temp-merge", branch]) {
        cleanup_temp_merge(source, default_branch);
        let _ = copy_mgr.delete_branch(branch);
        return MergeOutcome::Failed(format!("checkout temp-merge from {branch}: {e}"));
    }

    if let Err(_) = git(source, &["rebase", default_branch]) {
        let _ = git(source, &["rebase", "--abort"]);
        cleanup_temp_merge(source, default_branch);
        let _ = copy_mgr.delete_branch(branch);
        return MergeOutcome::Conflicted(format!(
            "rebase of {branch} onto {default_branch} had conflicts"
        ));
    }

    // Step 4: Run verify.sh if present.
    let verify_script = source.join(".enki/verify.sh");
    if verify_script.exists() {
        if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Verifying) {
            cleanup_temp_merge(source, default_branch);
            let _ = copy_mgr.delete_branch(branch);
            return MergeOutcome::Failed(format!("db update to verifying: {e}"));
        }

        let verify_result = Command::new("bash")
            .arg(".enki/verify.sh")
            .current_dir(source)
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
                cleanup_temp_merge(source, default_branch);
                let _ = copy_mgr.delete_branch(branch);
                return MergeOutcome::VerifyFailed(detail.trim().to_string());
            }
            Err(e) => {
                cleanup_temp_merge(source, default_branch);
                let _ = copy_mgr.delete_branch(branch);
                return MergeOutcome::Failed(format!("verify.sh execution error: {e}"));
            }
            Ok(_) => {} // verification passed
        }
    }

    // Step 5: Fast-forward merge into default branch.
    if let Err(e) = git(source, &["checkout", default_branch]) {
        cleanup_temp_merge(source, default_branch);
        let _ = copy_mgr.delete_branch(branch);
        return MergeOutcome::Failed(format!("checkout {default_branch} for merge: {e}"));
    }

    if let Err(e) = git(source, &["merge", "--ff-only", "temp-merge"]) {
        cleanup_temp_merge(source, default_branch);
        let _ = copy_mgr.delete_branch(branch);
        return MergeOutcome::Failed(format!("ff-only merge: {e}"));
    }

    // Cleanup.
    let _ = git(source, &["branch", "-d", "temp-merge"]);
    let _ = copy_mgr.delete_branch(branch);

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Merged) {
        return MergeOutcome::Failed(format!("db update to merged: {e}"));
    }

    MergeOutcome::Merged
}
