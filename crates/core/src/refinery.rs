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

/// RAII guard that removes a directory on drop.
struct CleanupGuard(PathBuf);

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Process a single merge request.
///
/// Dispatches to git-based merge or filesystem merge depending on whether
/// the source project is a git repo.
pub fn process_merge(
    copy_mgr: &CopyManager,
    copy_path: &Path,
    branch: &str,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
) -> MergeOutcome {
    if copy_mgr.is_git() {
        process_git_merge(copy_mgr, copy_path, branch, default_branch, db, mr_id)
    } else {
        process_fs_merge(copy_mgr, copy_path, branch, db, mr_id)
    }
}

/// Filesystem merge: diff the copy against its baseline and copy changed files back.
///
/// The copy has its own internal git (created at copy time), so we use
/// `git diff --name-status <base> HEAD` to find what the worker changed,
/// then copy those files back to the source directory.
fn process_fs_merge(
    copy_mgr: &CopyManager,
    copy_path: &Path,
    branch: &str,
    db: &Db,
    mr_id: &Id,
) -> MergeOutcome {
    let source = copy_mgr.project_root();

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Rebasing) {
        return MergeOutcome::Failed(format!("db update to rebasing: {e}"));
    }

    // Get the baseline commit (first commit on main, before the task branch).
    let base = match git(copy_path, &["rev-parse", "main"]) {
        Ok(sha) => sha,
        Err(e) => return MergeOutcome::Failed(format!("find baseline: {e}")),
    };

    // Get list of changed files: A=added, M=modified, D=deleted.
    let diff_output = match git(copy_path, &["diff", "--name-status", &base, "HEAD"]) {
        Ok(out) => out,
        Err(e) => return MergeOutcome::Failed(format!("diff: {e}")),
    };

    if diff_output.is_empty() {
        return MergeOutcome::Failed(format!(
            "branch {branch} has no file changes — worker produced no output"
        ));
    }

    // Run verify.sh if present.
    let verify_script = source.join(".enki/verify.sh");
    if verify_script.exists() {
        if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Verifying) {
            return MergeOutcome::Failed(format!("db update to verifying: {e}"));
        }
        let verify_result = Command::new("bash")
            .arg(&verify_script)
            .current_dir(copy_path)
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
            Err(e) => return MergeOutcome::Failed(format!("verify.sh execution error: {e}")),
            Ok(_) => {}
        }
    }

    // Apply changes to source.
    for line in diff_output.lines() {
        let mut parts = line.splitn(2, '\t');
        let status = parts.next().unwrap_or("");
        let path = match parts.next() {
            Some(p) => p,
            None => continue,
        };

        let src_file = copy_path.join(path);
        let dst_file = source.join(path);

        match status {
            "D" => {
                if dst_file.exists()
                    && let Err(e) = std::fs::remove_file(&dst_file)
                {
                    return MergeOutcome::Failed(format!("delete {path}: {e}"));
                }
            }
            _ => {
                // A (added) or M (modified) — copy file from copy to source.
                if let Some(parent) = dst_file.parent()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    return MergeOutcome::Failed(format!("mkdir for {path}: {e}"));
                }
                if let Err(e) = std::fs::copy(&src_file, &dst_file) {
                    return MergeOutcome::Failed(format!("copy {path}: {e}"));
                }
            }
        }
    }

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Merged) {
        return MergeOutcome::Failed(format!("db update to merged: {e}"));
    }

    MergeOutcome::Merged
}

/// Git-based merge for source repos that are git repositories.
///
/// All rebase/merge/verify work happens in a temporary `git clone --shared`
/// clone so the user's working tree is never touched. Only the final
/// `git merge --ff-only FETCH_HEAD` updates the source repo — this preserves
/// uncommitted working tree changes (git aborts if they conflict).
fn process_git_merge(
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

    // Step 4: Merge worker branch into default in the temp clone.
    // Three-way merge auto-resolves non-overlapping additions (the most common case).
    if let Err(e) = git(&tmp_dir, &["checkout", default_branch]) {
        return MergeOutcome::Failed(format!("checkout {default_branch} in temp: {e}"));
    }

    if let Err(_) = git(&tmp_dir, &["merge", branch, "--no-edit"]) {
        // Merge had conflicts — check if they're auto-resolvable or need agent help.
        let conflict_files = match git(&tmp_dir, &["diff", "--name-only", "--diff-filter=U"]) {
            Ok(files) => files.lines().map(String::from).collect::<Vec<_>>(),
            Err(e) => return MergeOutcome::Failed(format!("list conflict files: {e}")),
        };

        if conflict_files.is_empty() {
            // Merge failed but no unmerged files — shouldn't happen, treat as hard failure.
            let _ = git(&tmp_dir, &["merge", "--abort"]);
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

        // Prevent temp dir cleanup so the merger agent can work in it.
        std::mem::forget(_cleanup);

        return MergeOutcome::NeedsResolution {
            temp_dir: tmp_dir,
            conflict_files,
            conflict_diff,
        };
    }

    // Merge succeeded — continue to verify + bring back to source.
    finish_merge_inner(copy_mgr, &tmp_dir, default_branch, db, mr_id)
}

/// Complete a merge after conflicts have been resolved by the merger agent.
///
/// Runs verify.sh then fetches the result back to the source repo via ff-only merge.
/// Cleans up the temp dir on all exit paths.
pub fn finish_merge(
    copy_mgr: &CopyManager,
    temp_dir: &Path,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
) -> MergeOutcome {
    let _cleanup = CleanupGuard(temp_dir.to_path_buf());
    finish_merge_inner(copy_mgr, temp_dir, default_branch, db, mr_id)
}

/// Shared logic for steps 5-7: verify.sh, then fetch+ff-only back to source.
fn finish_merge_inner(
    copy_mgr: &CopyManager,
    tmp_dir: &Path,
    default_branch: &str,
    db: &Db,
    mr_id: &Id,
) -> MergeOutcome {
    let source = copy_mgr.project_root();
    let tmp_str = tmp_dir.to_string_lossy();

    // Step 5: Run verify.sh if present.
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

    // Step 6: Bring result back to source safely.
    if let Err(e) = git(source, &["fetch", &tmp_str, default_branch]) {
        return MergeOutcome::Failed(format!("fetch result back to source: {e}"));
    }

    if let Err(e) = git(source, &["merge", "--ff-only", "FETCH_HEAD"]) {
        return MergeOutcome::Failed(format!("ff-only merge into source: {e}"));
    }

    // Clean up the task branch from source if it was fetched in earlier flows.
    let _ = git(source, &["branch", "-D", &format!("task/{}", mr_id)]);

    if let Err(e) = db.update_merge_status(mr_id, MergeStatus::Merged) {
        return MergeOutcome::Failed(format!("db update to merged: {e}"));
    }

    MergeOutcome::Merged
}
