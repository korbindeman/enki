use std::path::Path;
use std::process::Command;

use crate::db::Db;
use crate::types::{Id, MergeStatus};

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

/// Process a single merge request programmatically in the refinery worktree.
///
/// Steps (matching Gastown's `Engineer.doMerge()`):
/// 1. Reset refinery worktree to latest default branch
/// 2. Rebase feature branch onto default branch via temp-merge
/// 3. Run verify.sh if present
/// 4. Fast-forward merge into default branch
pub fn process_merge(
    refinery_worktree: &Path,
    branch: &str,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
) -> MergeOutcome {
    // Step 1: Reset refinery to latest default branch
    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Rebasing) {
        return MergeOutcome::Failed(format!("db update to rebasing: {e}"));
    }

    if let Err(e) = git(refinery_worktree, &["checkout", default_branch]) {
        return MergeOutcome::Failed(format!("checkout {default_branch}: {e}"));
    }
    if let Err(e) = git(refinery_worktree, &["reset", "--hard", default_branch]) {
        return MergeOutcome::Failed(format!("reset to {default_branch}: {e}"));
    }

    // Step 2: Verify the feature branch actually changed files.
    //
    // We compare tree objects rather than counting commits because workers
    // branch from origin/<default_branch>, which may include a dirty-snapshot
    // commit. That snapshot commit makes the branch appear "1 commit ahead"
    // of local <default_branch> even when the worker produced no output.
    // Comparing trees catches both cases: zero commits AND snapshot-only branches.
    let origin_ref = format!("origin/{default_branch}");
    let origin_tree = git(refinery_worktree, &["rev-parse", &format!("{origin_ref}^{{tree}}")]);
    let branch_tree = git(refinery_worktree, &["rev-parse", &format!("{branch}^{{tree}}")]);

    match (origin_tree, branch_tree) {
        (Ok(ot), Ok(bt)) => {
            if ot == bt {
                return MergeOutcome::Failed(format!(
                    "branch {branch} has no file changes vs {origin_ref} — worker produced no output"
                ));
            }
            tracing::info!(branch, "feature branch has file changes, proceeding with merge");
        }
        (Err(e), _) | (_, Err(e)) => {
            return MergeOutcome::Failed(format!("tree comparison failed: {e}"));
        }
    }

    // Create temp branch from feature branch and rebase onto default
    if let Err(e) = git(refinery_worktree, &["checkout", "-b", "temp-merge", branch]) {
        cleanup_temp_merge(refinery_worktree, default_branch);
        return MergeOutcome::Failed(format!("checkout temp-merge from {branch}: {e}"));
    }

    if let Err(_) = git(refinery_worktree, &["rebase", default_branch]) {
        // Rebase failed — conflict
        let _ = git(refinery_worktree, &["rebase", "--abort"]);
        cleanup_temp_merge(refinery_worktree, default_branch);
        return MergeOutcome::Conflicted(format!(
            "rebase of {branch} onto {default_branch} had conflicts"
        ));
    }

    // Step 3: Run verify.sh if present
    let verify_script = refinery_worktree.join(".enki/verify.sh");
    if verify_script.exists() {
        if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Verifying) {
            cleanup_temp_merge(refinery_worktree, default_branch);
            return MergeOutcome::Failed(format!("db update to verifying: {e}"));
        }

        let verify_result = Command::new("bash")
            .arg(".enki/verify.sh")
            .current_dir(refinery_worktree)
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
                cleanup_temp_merge(refinery_worktree, default_branch);
                return MergeOutcome::VerifyFailed(detail.trim().to_string());
            }
            Err(e) => {
                cleanup_temp_merge(refinery_worktree, default_branch);
                return MergeOutcome::Failed(format!("verify.sh execution error: {e}"));
            }
            Ok(_) => {} // verification passed
        }
    }

    // Step 4: Fast-forward merge into default branch
    if let Err(e) = git(refinery_worktree, &["checkout", default_branch]) {
        cleanup_temp_merge(refinery_worktree, default_branch);
        return MergeOutcome::Failed(format!("checkout {default_branch} for merge: {e}"));
    }

    if let Err(e) = git(refinery_worktree, &["merge", "--ff-only", "temp-merge"]) {
        cleanup_temp_merge(refinery_worktree, default_branch);
        return MergeOutcome::Failed(format!("ff-only merge: {e}"));
    }

    let _ = git(refinery_worktree, &["branch", "-d", "temp-merge"]);

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Merged) {
        return MergeOutcome::Failed(format!("db update to merged: {e}"));
    }

    MergeOutcome::Merged
}
