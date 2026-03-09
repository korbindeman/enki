use crate::types::*;

use super::{Event, Orchestrator, WorkerOutcome, WorkerResult};

impl Orchestrator {
    pub(crate) fn worker_done(&mut self, result: WorkerResult) -> Vec<Event> {
        let mut events = Vec::new();

        match result.outcome {
            WorkerOutcome::Success { output } => {
                // Store output.
                if let Some(ref out) = output {
                    if let Err(e) = self.db.insert_task_output(&result.task_id, out) {
                        tracing::warn!(task_id = %result.task_id, error = %e, "failed to persist task output");
                    }
                }
                self.monitor.clear_retries(&result.task_id.0);

                // Create merge request.
                let base_branch = self
                    .db
                    .get_task(&result.task_id)
                    .ok()
                    .and_then(|t| t.base_branch)
                    .unwrap_or_else(|| "main".to_string());
                let mr_id = Id::new("mr");
                let mr = MergeRequest {
                    id: mr_id.clone(),
                    task_id: result.task_id.clone(),
                    branch: result.branch.clone(),
                    base_branch,
                    status: MergeStatus::Queued,
                    priority: 2,
                    diff_stats: None,
                    review_note: None,
                    execution_id: result.execution_id.clone(),
                    step_id: result.step_id.clone(),
                    queued_at: chrono::Utc::now(),
                    started_at: None,
                    merged_at: None,
                };
                if let Err(e) = self.db.insert_merge_request(&mr) {
                    tracing::warn!(mr_id = %mr.id, task_id = %result.task_id, error = %e, "failed to persist merge request");
                    events.push(Event::StatusMessage(format!("failed to persist merge request for task {}: {e}", result.task_id)));
                }
                if let Err(e) = self.db.update_task_status(&result.task_id, TaskStatus::Done) {
                    tracing::warn!(task_id = %result.task_id, error = %e, "failed to update task status to Done");
                    events.push(Event::StatusMessage(format!("failed to update task status: {e}")));
                }

                // Mark worker done in the DAG — enables Completed/Started edges.
                // step_completed() is called later in merge_done() when the merge lands.
                if let (Some(eid), Some(sid)) = (&result.execution_id, &result.step_id) {
                    self.scheduler.step_worker_done(&eid.0, sid, output.clone());
                }

                events.push(Event::WorkerCompleted {
                    task_id: result.task_id.0.clone(),
                    title: result.title.clone(),
                });
                events.push(Event::QueueMerge(mr));
            }
            WorkerOutcome::Artifact { output } => {
                // Store output.
                if let Some(ref out) = output {
                    if let Err(e) = self.db.insert_task_output(&result.task_id, out) {
                        tracing::warn!(task_id = %result.task_id, error = %e, "failed to persist task output");
                    }
                }
                self.monitor.clear_retries(&result.task_id.0);
                if let Err(e) = self.db.update_task_status(&result.task_id, TaskStatus::Done) {
                    tracing::warn!(task_id = %result.task_id, error = %e, "failed to update task status to Done");
                    events.push(Event::StatusMessage(format!("failed to update task status: {e}")));
                }

                // Artifact steps skip the merge — mark both worker_done and
                // step_completed immediately so dependents can proceed.
                let is_checkpoint = if let (Some(eid), Some(sid)) = (&result.execution_id, &result.step_id) {
                    let cp = self.scheduler.is_checkpoint(&eid.0, sid);
                    self.scheduler.step_worker_done(&eid.0, sid, output.clone());
                    self.scheduler.step_completed(&eid.0, sid, None);
                    cp
                } else {
                    false
                };

                events.push(Event::WorkerCompleted {
                    task_id: result.task_id.0.clone(),
                    title: result.title.clone(),
                });

                if is_checkpoint {
                    if let (Some(eid), Some(sid)) = (&result.execution_id, &result.step_id) {
                        self.scheduler.pause_execution(&eid.0);
                        events.push(Event::CheckpointReached {
                            execution_id: eid.0.clone(),
                            step_id: sid.clone(),
                            title: result.title.clone(),
                            output,
                        });
                    }
                }

                // No merge step — tick scheduler now to dispatch dependents.
                events.extend(self.tick_scheduler());
            }
            WorkerOutcome::NoChanges => {
                let error = "completed without committing changes".to_string();
                self.notify_scheduler_failed(&result.execution_id, &result.step_id);

                let retried = self.maybe_retry(
                    &result.task_id,
                    &result.title,
                    &error,
                    &result.execution_id,
                    &result.step_id,
                );

                if retried {
                    events.push(Event::WorkerFailed {
                        task_id: result.task_id.0.clone(),
                        title: result.title.clone(),
                        error: format!("{error} (retrying)"),
                    });
                } else {
                    if let Err(e) = self
                        .db
                        .update_task_status(&result.task_id, TaskStatus::Failed)
                    {
                        tracing::warn!(task_id = %result.task_id, error = %e, "failed to update task status to Failed");
                        events.push(Event::StatusMessage(format!("failed to update task status: {e}")));
                    }

                    events.push(Event::WorkerFailed {
                        task_id: result.task_id.0.clone(),
                        title: result.title.clone(),
                        error,
                    });
                }
            }
            WorkerOutcome::Failed { ref error } => {
                // Always mark the DAG node as failed first (consistent state machine).
                self.notify_scheduler_failed(&result.execution_id, &result.step_id);

                let retried = self.maybe_retry(
                    &result.task_id,
                    &result.title,
                    error,
                    &result.execution_id,
                    &result.step_id,
                );

                if retried {
                    events.push(Event::WorkerFailed {
                        task_id: result.task_id.0.clone(),
                        title: result.title.clone(),
                        error: format!("{error} (retrying)"),
                    });
                } else {
                    if let Err(e) = self
                        .db
                        .update_task_status(&result.task_id, TaskStatus::Failed)
                    {
                        tracing::warn!(task_id = %result.task_id, error = %e, "failed to update task status to Failed");
                        events.push(Event::StatusMessage(format!("failed to update task status: {e}")));
                    }

                    events.push(Event::WorkerFailed {
                        task_id: result.task_id.0.clone(),
                        title: result.title.clone(),
                        error: error.clone(),
                    });
                }
            }
        }

        // Tick scheduler after any worker completion.
        events.extend(self.tick_scheduler());
        events
    }

    pub(crate) fn retry_task(&mut self, task_id: Id) -> Vec<Event> {
        // Reset the task to Pending in the DB.
        if let Err(e) = self.db.update_task_status(&task_id, TaskStatus::Pending) {
            tracing::warn!(task_id = %task_id, error = %e, "failed to update task status to Pending for retry");
        }
        self.monitor.clear_retries(&task_id.0);

        // Find the execution and step for this task, then retry in the DAG.
        // This resets the Failed node and un-blocks its transitive dependents.
        if let Some((exec_id, step_id)) = self.scheduler.find_task(&task_id) {
            let exec_id = exec_id.to_string();
            let step_id = step_id.to_string();
            self.scheduler.retry_node(&exec_id, &step_id);
        }

        self.tick_scheduler()
    }

    /// Notify the scheduler that a step failed.
    fn notify_scheduler_failed(&mut self, execution_id: &Option<Id>, step_id: &Option<String>) {
        if let (Some(eid), Some(sid)) = (execution_id, step_id) {
            self.scheduler.step_failed(&eid.0, sid);
        }
    }

    /// Check if a failed task should be retried.
    /// Returns true if the task was re-queued. The DAG node must already be
    /// marked Failed before calling this.
    fn maybe_retry(
        &mut self,
        task_id: &Id,
        title: &str,
        error: &str,
        execution_id: &Option<Id>,
        step_id: &Option<String>,
    ) -> bool {
        let is_retryable = error.contains("timed out")
            || error.contains("stuck")
            || error.contains("without committing changes");
        if !is_retryable {
            return false;
        }
        if self.monitor.should_block_retry(&task_id.0) {
            return false;
        }
        let retry_count = self.monitor.record_retry(&task_id.0);
        if let Err(e) = self.db.update_task_status(task_id, TaskStatus::Pending) {
            tracing::warn!(task_id = %task_id, error = %e, "failed to update task status to Pending for retry");
        }

        // Reset the DAG node from Failed back to Pending/Ready.
        if let (Some(eid), Some(sid)) = (execution_id, step_id) {
            self.scheduler.retry_node(&eid.0, sid);
        }

        tracing::info!(
            task_id = %task_id, title,
            retry = retry_count,
            "retrying failed task"
        );
        true
    }
}
