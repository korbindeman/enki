use crate::refinery::MergeOutcome;
use crate::types::*;

use super::{Event, MergeResult, Orchestrator};

impl Orchestrator {
    pub(crate) fn merge_done(&mut self, result: MergeResult) -> Vec<Event> {
        let mut events = Vec::new();

        // Read the MR from DB.
        let mr = match self.db.get_merge_request(&result.mr_id) {
            Ok(mr) => mr,
            Err(e) => {
                tracing::error!(mr_id = %result.mr_id, error = %e, "failed to read MR after merge");
                return events;
            }
        };

        match result.outcome {
            MergeOutcome::Merged => {
                // Check checkpoint BEFORE step_completed (in case execution completes).
                let is_checkpoint = if let (Some(eid), Some(sid)) = (&mr.execution_id, &mr.step_id)
                {
                    self.scheduler.is_checkpoint(&eid.0, sid)
                } else {
                    false
                };

                // Advance the DAG (WorkerDone → Done, fires Merged edges).
                let output = if let (Some(eid), Some(sid)) = (&mr.execution_id, &mr.step_id) {
                    let output = self.db.get_task_output(&mr.task_id).ok().flatten();
                    self.scheduler.step_completed(&eid.0, sid, output.clone());
                    output
                } else {
                    None
                };

                events.push(Event::MergeLanded {
                    mr_id: mr.id.0.clone(),
                    task_id: mr.task_id.0.clone(),
                    branch: mr.branch.clone(),
                });

                // If checkpoint: pause execution and notify coordinator.
                if is_checkpoint
                    && let (Some(eid), Some(sid)) = (&mr.execution_id, &mr.step_id)
                {
                    self.scheduler.pause_execution(&eid.0);
                    events.push(Event::CheckpointReached {
                        execution_id: eid.0.clone(),
                        step_id: sid.clone(),
                        title: mr.branch.clone(),
                        output,
                    });
                }
            }
            MergeOutcome::Conflicted(ref detail) => {
                if let Err(e) = self.db.update_merge_status(&mr.id, MergeStatus::Conflicted) {
                    tracing::warn!(mr_id = %mr.id, error = %e, "failed to update merge status to Conflicted");
                }
                if let Err(e) = self.db.update_merge_review_note(&mr.id, detail) {
                    tracing::warn!(mr_id = %mr.id, error = %e, "failed to update merge review note");
                }
                if let Err(e) = self.db.update_task_status(&mr.task_id, TaskStatus::Blocked) {
                    tracing::warn!(task_id = %mr.task_id, error = %e, "failed to update task status to Blocked");
                }
                if let (Some(eid), Some(sid)) = (&mr.execution_id, &mr.step_id) {
                    self.scheduler.step_failed(&eid.0, sid);
                }
                events.push(Event::MergeConflicted {
                    mr_id: mr.id.0.clone(),
                    task_id: mr.task_id.0.clone(),
                    branch: mr.branch.clone(),
                });
            }
            MergeOutcome::NeedsResolution {
                ref temp_dir,
                ref conflict_files,
                ref conflict_diff,
            } => {
                if let Err(e) = self.db.update_merge_status(&mr.id, MergeStatus::Resolving) {
                    tracing::warn!(mr_id = %mr.id, error = %e, "failed to update merge status to Resolving");
                }
                let detail = format!(
                    "merge conflict in {} file(s): {}",
                    conflict_files.len(),
                    conflict_files.join(", ")
                );
                if let Err(e) = self.db.update_merge_review_note(&mr.id, &detail) {
                    tracing::warn!(mr_id = %mr.id, error = %e, "failed to update merge review note");
                }
                events.push(Event::MergeNeedsResolution {
                    mr_id: mr.id.0.clone(),
                    task_id: mr.task_id.clone(),
                    temp_dir: temp_dir.clone(),
                    default_branch: mr.base_branch.clone(),
                    conflict_files: conflict_files.clone(),
                    conflict_diff: conflict_diff.clone(),
                });
            }
            MergeOutcome::VerifyFailed(ref detail) => {
                if let Err(e) = self.db.update_merge_status(&mr.id, MergeStatus::Failed) {
                    tracing::warn!(mr_id = %mr.id, error = %e, "failed to update merge status to Failed");
                }
                if let Err(e) = self.db.update_merge_review_note(&mr.id, detail) {
                    tracing::warn!(mr_id = %mr.id, error = %e, "failed to update merge review note");
                }
                if let Err(e) = self.db.update_task_status(&mr.task_id, TaskStatus::Failed) {
                    tracing::warn!(task_id = %mr.task_id, error = %e, "failed to update task status to Failed");
                }
                if let (Some(eid), Some(sid)) = (&mr.execution_id, &mr.step_id) {
                    self.scheduler.step_failed(&eid.0, sid);
                }
                events.push(Event::MergeFailed {
                    mr_id: mr.id.0.clone(),
                    task_id: mr.task_id.0.clone(),
                    branch: mr.branch.clone(),
                    reason: format!("verify.sh failed: {detail}"),
                });
            }
            MergeOutcome::Failed(ref detail) => {
                if let Err(e) = self.db.update_merge_status(&mr.id, MergeStatus::Failed) {
                    tracing::warn!(mr_id = %mr.id, error = %e, "failed to update merge status to Failed");
                }
                if let Err(e) = self.db.update_merge_review_note(&mr.id, detail) {
                    tracing::warn!(mr_id = %mr.id, error = %e, "failed to update merge review note");
                }
                if let Err(e) = self.db.update_task_status(&mr.task_id, TaskStatus::Failed) {
                    tracing::warn!(task_id = %mr.task_id, error = %e, "failed to update task status to Failed");
                }
                if let (Some(eid), Some(sid)) = (&mr.execution_id, &mr.step_id) {
                    self.scheduler.step_failed(&eid.0, sid);
                }
                events.push(Event::MergeFailed {
                    mr_id: mr.id.0.clone(),
                    task_id: mr.task_id.0.clone(),
                    branch: mr.branch.clone(),
                    reason: detail.clone(),
                });
            }
        }

        // Tick scheduler to dispatch downstream tasks.
        events.extend(self.tick_scheduler());
        events
    }

    /// Reconcile scheduler DAG with DB: check for merges that landed
    /// while we weren't looking (missed refinery signals).
    pub fn reconcile_merges(&mut self) -> Vec<Event> {
        let running = self.scheduler.running_steps();
        for (exec_id, step_id, task_id) in running {
            if let Ok(Some(mr)) = self.db.get_merge_request_for_task(&task_id)
                && mr.status == MergeStatus::Merged
            {
                tracing::info!(
                    task_id = %task_id, mr_id = %mr.id,
                    "reconciliation: MR already merged, advancing DAG"
                );
                let output = self.db.get_task_output(&task_id).ok().flatten();
                self.scheduler.step_completed(&exec_id, &step_id, output);
            }
        }
        self.tick_scheduler()
    }
}
