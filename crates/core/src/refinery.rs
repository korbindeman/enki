use std::path::{Path, PathBuf};
use std::process::Command;

use crate::copy::CopyManager;
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

/// RAII guard that removes a directory on drop.
struct CleanupGuard(PathBuf);

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Process a single merge request.
///
/// All rebase/merge/verify work happens in a temporary `git clone --shared`
/// clone so the user's working tree is never touched. Only the final
/// `git merge --ff-only FETCH_HEAD` updates the source repo — this preserves
/// uncommitted working tree changes (git aborts if they conflict).
///
/// Flow:
/// 1. Create a temporary shared clone of the source repo
/// 2. Fetch the worker's branch from the copy into the temp clone
/// 3. Rebase onto the default branch
/// 4. Run verify.sh if present
/// 5. Fast-forward merge in the temp clone
/// 6. Fetch result back to source and ff-only merge
pub fn process_merge(
    copy_mgr: &CopyManager,
    copy_path: &Path,
    branch: &str,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
) -> MergeOutcome {
    let source = copy_mgr.project_root();

    // Step 1: Create a temporary shared clone of the source repo.
    // `--shared` reuses the object store — fast and space-efficient.
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

    // Step 2: Fetch worker's branch from the copy into the temp clone.
    let copy_str = copy_path.to_string_lossy();
    if let Err(e) = git(&tmp_dir, &["fetch", &copy_str, &format!("{branch}:{branch}")]) {
        return MergeOutcome::Failed(format!("fetch worker branch: {e}"));
    }

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Rebasing) {
        return MergeOutcome::Failed(format!("db update to rebasing: {e}"));
    }

    // Step 3: Verify the feature branch actually changed files.
    let default_tree = git(&tmp_dir, &["rev-parse", &format!("{default_branch}^{{tree}}")]);
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

    // Step 4: Rebase worker branch onto default in the temp clone.
    if let Err(e) = git(&tmp_dir, &["checkout", "-b", "temp-merge", branch]) {
        return MergeOutcome::Failed(format!("checkout temp-merge from {branch}: {e}"));
    }

    if git(&tmp_dir, &["rebase", default_branch]).is_err() {
        let _ = git(&tmp_dir, &["rebase", "--abort"]);
        return MergeOutcome::Conflicted(format!(
            "rebase of {branch} onto {default_branch} had conflicts"
        ));
    }

    // Step 5: Run verify.sh if present.
    // verify.sh lives at .enki/verify.sh in the source (gitignored, so not in the clone).
    // Run it via absolute path with cwd set to the temp clone's working tree.
    let verify_script = source.join(".enki/verify.sh");
    if verify_script.exists() {
        if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Verifying) {
            return MergeOutcome::Failed(format!("db update to verifying: {e}"));
        }

        let verify_result = Command::new("bash")
            .arg(&verify_script)
            .current_dir(&tmp_dir)
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

    // Step 6: Complete the merge in the temp clone.
    if let Err(e) = git(&tmp_dir, &["checkout", default_branch]) {
        return MergeOutcome::Failed(format!("checkout {default_branch} in temp: {e}"));
    }

    if let Err(e) = git(&tmp_dir, &["merge", "--ff-only", "temp-merge"]) {
        return MergeOutcome::Failed(format!("ff-only merge in temp: {e}"));
    }

    // Step 7: Bring result back to source safely.
    // Fetch the updated default branch from the temp clone into the source repo.
    if let Err(e) = git(source, &["fetch", &tmp_str, default_branch]) {
        return MergeOutcome::Failed(format!("fetch result back to source: {e}"));
    }

    // ff-only merge FETCH_HEAD into source. This preserves dirty working tree
    // files that don't conflict. If there IS a conflict with uncommitted changes,
    // git will abort with a clear error — correct behavior.
    if let Err(e) = git(source, &["merge", "--ff-only", "FETCH_HEAD"]) {
        return MergeOutcome::Failed(format!("ff-only merge into source: {e}"));
    }

    // Clean up the task branch from source if it was fetched in earlier flows.
    let _ = git(source, &["branch", "-D", branch]);

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Merged) {
        return MergeOutcome::Failed(format!("db update to merged: {e}"));
    }

    MergeOutcome::Merged
}
