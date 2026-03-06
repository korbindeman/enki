use std::path::{Path, PathBuf};

use enki_core::copy::CopyManager;
use enki_core::orchestrator::{WorkerOutcome, WorkerResult};
use enki_core::types::{Id, MergeStatus};
use tokio::sync::mpsc;

use super::prompts::extract_output;

// ---------------------------------------------------------------------------
// Session log excerpt (for failure diagnostics)
// ---------------------------------------------------------------------------

/// Read the tail of a worker's session log, filtering out streaming chunks.
/// Returns (log_path, excerpt) so callers can reference the full log.
pub(super) fn read_session_log_excerpt(task_id: &str) -> Option<(String, String)> {
    let log_path = dirs::home_dir()?
        .join(".enki/logs/sessions")
        .join(format!("{task_id}.log"));

    let content = std::fs::read_to_string(&log_path).ok()?;

    // Filter out streaming message chunks — they're token-by-token fragments.
    let meaningful: Vec<&str> = content
        .lines()
        .filter(|line| !line.contains("session/update"))
        .collect();

    if meaningful.is_empty() {
        return None;
    }

    // Take last 30 meaningful lines, truncate to ~3000 chars.
    let tail: Vec<&str> = meaningful.iter().rev().take(30).copied().collect::<Vec<_>>().into_iter().rev().collect();
    let mut excerpt = tail.join("\n");
    if excerpt.len() > 3000 {
        excerpt = excerpt[excerpt.len() - 3000..].to_string();
    }
    Some((log_path.display().to_string(), excerpt))
}

// ---------------------------------------------------------------------------
// Internal channel types
// ---------------------------------------------------------------------------

pub(super) struct WorkerDone {
    pub task_id: Id,
    pub session_id: Option<String>,
    pub title: String,
    pub branch: String,
    pub copy_path: PathBuf,
    pub base_commit: Option<String>,
    pub result: Result<String, String>,
    pub execution_id: Option<Id>,
    pub step_id: Option<String>,
    /// If true, this worker produces a markdown artifact instead of code changes.
    pub artifact: bool,
}

pub(super) struct MergerDone {
    pub merge_request_id: Id,
    pub outcome: enki_core::refinery::MergeOutcome,
}

pub(super) struct MergerAgentDone {
    pub mr_id: Id,
    pub temp_dir: PathBuf,
    pub default_branch: String,
    pub session_id: String,
}

// ---------------------------------------------------------------------------
// Artifact discovery
// ---------------------------------------------------------------------------

/// Scan the artifacts directory for this execution and return (label, path) pairs
/// for any artifact files that exist from completed upstream steps.
pub(super) fn discover_artifact_files(
    project_root: &Path,
    execution_id: &str,
    _upstream_outputs: &[(String, String)],
) -> Vec<(String, PathBuf)> {
    let artifacts_dir = project_root.join(".enki").join("artifacts").join(execution_id);
    let entries = match std::fs::read_dir(&artifacts_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                result.push((stem.to_string(), path));
            }
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

// ---------------------------------------------------------------------------
// Worker done processing
// ---------------------------------------------------------------------------

/// Convert a raw WorkerDone (from the channel) into an orchestrator WorkerResult.
/// Handles auto-commit, change detection, and copy cleanup.
pub(super) fn process_worker_done(done: WorkerDone, copy_mgr: &CopyManager, enki_dir: &Path, commit_suffix: &str) -> WorkerResult {
    match done.result {
        Ok(ref stop_reason) => {
            // Artifact workers: the agent writes the file directly; [OUTPUT] is just a summary.
            if done.artifact {
                let _ = copy_mgr.remove_copy(&done.copy_path);

                if let (Some(eid), Some(sid)) = (&done.execution_id, &done.step_id) {
                    let artifact_path = enki_dir.join("artifacts").join(&eid.0).join(format!("{sid}.md"));
                    if artifact_path.exists() {
                        tracing::info!(
                            task_id = %done.task_id,
                            path = %artifact_path.display(),
                            "artifact file written by agent"
                        );
                        let summary = extract_output(stop_reason);
                        return WorkerResult {
                            task_id: done.task_id,
                            execution_id: done.execution_id,
                            step_id: done.step_id,
                            title: done.title,
                            branch: done.branch,
                            outcome: WorkerOutcome::Artifact { output: summary },
                        };
                    }
                }

                // Agent didn't write the artifact file — treat as failure.
                tracing::error!(
                    task_id = %done.task_id, title = %done.title,
                    "artifact worker completed but did not write the artifact file"
                );
                return WorkerResult {
                    task_id: done.task_id,
                    execution_id: done.execution_id,
                    step_id: done.step_id,
                    title: done.title,
                    branch: done.branch,
                    outcome: WorkerOutcome::Failed {
                        error: "artifact worker did not write the artifact file".into(),
                    },
                };
            }

            // Auto-commit uncommitted changes in the copy.
            let msg = if commit_suffix.is_empty() {
                done.title.clone()
            } else {
                format!("{}\n\n{commit_suffix}", done.title)
            };
            let committed = copy_mgr.commit_copy(&done.copy_path, &msg);

            // Check for actual changes vs the base commit.
            let has_changes = if committed {
                true
            } else {
                match &done.base_commit {
                    Some(base) => {
                        enki_core::copy::head_sha(&done.copy_path)
                            .as_deref() != Some(base.as_str())
                    }
                    None => enki_core::copy::head_sha(&done.copy_path).is_some(),
                }
            };

            if !has_changes {
                tracing::warn!(
                    task_id = %done.task_id, title = %done.title,
                    "worker completed but copy has no changes"
                );
                let _ = copy_mgr.remove_copy(&done.copy_path);
                return WorkerResult {
                    task_id: done.task_id,
                    execution_id: done.execution_id,
                    step_id: done.step_id,
                    title: done.title,
                    branch: done.branch,
                    outcome: WorkerOutcome::NoChanges,
                };
            }

            // Keep copy for refinery to fetch from.
            let output = extract_output(stop_reason);
            WorkerResult {
                task_id: done.task_id,
                execution_id: done.execution_id,
                step_id: done.step_id,
                title: done.title,
                branch: done.branch,
                outcome: WorkerOutcome::Success { output },
            }
        }
        Err(ref error) => {
            tracing::error!(
                task_id = %done.task_id, title = %done.title,
                error, "worker failed"
            );
            let _ = copy_mgr.remove_copy(&done.copy_path);
            WorkerResult {
                task_id: done.task_id,
                execution_id: done.execution_id,
                step_id: done.step_id,
                title: done.title,
                branch: done.branch,
                outcome: WorkerOutcome::Failed {
                    error: error.clone(),
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Refinery dispatch
// ---------------------------------------------------------------------------

pub(super) fn try_dispatch_merge(
    db: &enki_core::db::Db,
    db_path: &str,
    copy_mgr: &CopyManager,
    merger_done_tx: &mpsc::UnboundedSender<MergerDone>,
    merge_in_progress: &mut bool,
) {
    let queued = match db.get_queued_merge_requests() {
        Ok(q) => q,
        Err(_) => return,
    };
    let Some(mr) = queued.first() else { return };

    *merge_in_progress = true;
    let mr_id = mr.id.clone();
    let branch = mr.branch.clone();
    let base_branch = mr.base_branch.clone();
    tracing::info!(mr_id = %mr_id, task_id = %mr.task_id, branch = %mr.branch, "dispatching merge");

    let _ = db.update_merge_status(&mr_id, MergeStatus::Processing);

    let done_tx = merger_done_tx.clone();
    let project_root_owned = copy_mgr.project_root().to_path_buf();
    let copies_dir_owned = copy_mgr.copies_dir().to_path_buf();
    let db_path_clone = db_path.to_string();
    let git_identity_owned = copy_mgr.git_identity().clone();
    let is_git = copy_mgr.is_git();

    // Determine copy path from branch name (task/<task_id> → copies/<task_id>).
    let copy_path = copy_mgr.copies_dir().join(
        branch.strip_prefix("task/").unwrap_or(&branch),
    );

    tokio::task::spawn_blocking(move || {
        let db =
            enki_core::db::Db::open(&db_path_clone).expect("refinery: failed to open db");
        let copy_mgr = CopyManager::new(project_root_owned, copies_dir_owned, git_identity_owned, is_git);
        let outcome =
            enki_core::refinery::process_merge(&copy_mgr, &copy_path, &branch, &base_branch, &db, &mr_id);
        let _ = done_tx.send(MergerDone {
            merge_request_id: mr_id,
            outcome,
        });
    });
}
