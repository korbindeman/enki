mod types;

pub use types::*;

use std::collections::HashMap;
use std::path::PathBuf;

use crate::dag::Dag;
use crate::db::Db;
use crate::monitor::{MonitorAction, MonitorState};
use crate::refinery::MergeOutcome;
use crate::scheduler::{Limits, Scheduler, SchedulerAction};
use crate::types::*;

pub struct Orchestrator {
    scheduler: Scheduler,
    db: Db,
    monitor: MonitorState,
    events_dir: Option<PathBuf>,
    session_id: String,
}

impl Orchestrator {
    pub fn new(db: Db, limits: Limits, session_id: String) -> Self {
        Self {
            scheduler: Scheduler::new(limits),
            db,
            monitor: MonitorState::new(),
            events_dir: None,
            session_id,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Set the directory where signal files are written by external processes.
    pub fn set_events_dir(&mut self, path: PathBuf) {
        self.events_dir = Some(path);
    }

    /// Get a reference to the database.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Get a reference to the scheduler.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Process a command and return events for the CLI layer.
    pub fn handle(&mut self, cmd: Command) -> Vec<Event> {
        let events = match cmd {
            Command::CreateExecution { steps } => self.create_execution(steps),
            Command::CreateTask {
                title,
                description,
                tier,
            } => self.create_task(title, description, tier),
            Command::WorkerDone(result) => self.worker_done(result),
            Command::MergeDone(result) => self.merge_done(result),
            Command::RetryTask { task_id } => self.retry_task(task_id),
            Command::Pause(target) => self.pause(target),
            Command::Resume(target) => self.resume(target),
            Command::Cancel(target) => self.cancel(target),
            Command::StopAll => self.stop_all(),
            Command::MonitorTick { workers } => self.monitor_tick(workers),
            Command::AddSteps {
                execution_id,
                steps,
            } => self.add_steps(execution_id, steps),
            Command::DiscoverFromDb => self.discover_from_db(),
            Command::CheckSignals => self.check_signals(),
        };
        self.persist_dags();
        events
    }

    /// Save all active execution DAGs to the database.
    fn persist_dags(&self) {
        for exec_id in self.scheduler.active_executions() {
            if let Some(dag) = self.scheduler.get_dag(exec_id)
                && let Err(e) = self.db.save_dag(&Id(exec_id.to_string()), dag) {
                    tracing::warn!(execution_id = %exec_id, error = %e, "failed to persist dag");
                }
        }
    }

    // -----------------------------------------------------------------------
    // Command handlers
    // -----------------------------------------------------------------------

    fn create_execution(&mut self, steps: Vec<StepDef>) -> Vec<Event> {
        let exec_id = Id::new("exec");
        let now = chrono::Utc::now();

        // Create execution record.
        let execution = Execution {
            id: exec_id.clone(),
            session_id: Some(self.session_id.clone()),
            status: ExecutionStatus::Running,
            created_at: now,
        };
        if let Err(e) = self.db.insert_execution(&execution) {
            return vec![Event::StatusMessage(format!(
                "failed to create execution: {e}"
            ))];
        }

        // Create tasks and execution_steps.
        let mut step_tasks: HashMap<String, Id> = HashMap::new();
        let mut task_ids: HashMap<String, Id> = HashMap::new(); // step_id → task_id

        for step in &steps {
            let task_id = Id::new("task");
            let task = Task {
                id: task_id.clone(),
                session_id: Some(self.session_id.clone()),
                title: step.title.clone(),
                description: Some(step.description.clone()),
                status: TaskStatus::Pending,
                assigned_to: None,
                copy_path: None,
                branch: None,
                base_branch: None,
                tier: Some(step.tier),
                current_activity: None,
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = self.db.insert_task(&task) {
                tracing::error!(step_id = %step.id, error = %e, "failed to insert task");
                continue;
            }
            if let Err(e) = self.db.insert_execution_step(&exec_id, &step.id, &task_id) {
                tracing::error!(step_id = %step.id, error = %e, "failed to insert execution step");
                continue;
            }
            step_tasks.insert(step.id.clone(), task_id.clone());
            task_ids.insert(step.id.clone(), task_id);
        }

        // Create task dependencies.
        for step in &steps {
            let Some(task_id) = task_ids.get(&step.id) else {
                continue;
            };
            for dep in &step.needs {
                if let Some(dep_task_id) = task_ids.get(&dep.step_id) {
                    let _ = self.db.insert_dependency(task_id, dep_task_id);
                }
            }
        }

        // Build DAG.
        let step_data: Vec<_> = steps
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    s.title.clone(),
                    s.description.clone(),
                    Some(s.tier),
                    s.checkpoint,
                    s.needs
                        .iter()
                        .map(|d| (d.step_id.clone(), d.condition))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let dag = Dag::from_steps_with_edges(&step_data);

        // Register with scheduler and tick.
        self.scheduler
            .add_execution(exec_id, Id("project".into()), dag, step_tasks);

        self.tick_scheduler()
    }

    fn create_task(
        &mut self,
        title: String,
        description: Option<String>,
        tier: Tier,
    ) -> Vec<Event> {
        let task_id = Id::new("task");
        let exec_id = Id::new("exec");
        let step_id = "task";
        let now = chrono::Utc::now();

        let task = Task {
            id: task_id.clone(),
            session_id: Some(self.session_id.clone()),
            title: title.clone(),
            description: description.clone(),
            status: TaskStatus::Pending,
            assigned_to: None,
            copy_path: None,
            branch: None,
            base_branch: None,
            tier: Some(tier),
            current_activity: None,
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = self.db.insert_task(&task) {
            return vec![Event::StatusMessage(format!(
                "failed to create task: {e}"
            ))];
        }

        let execution = Execution {
            id: exec_id.clone(),
            session_id: Some(self.session_id.clone()),
            status: ExecutionStatus::Running,
            created_at: now,
        };
        let _ = self.db.insert_execution(&execution);
        let _ = self.db.insert_execution_step(&exec_id, step_id, &task_id);

        let dag = Dag::single(
            step_id,
            &title,
            description.as_deref().unwrap_or(""),
            Some(tier),
        );
        let mut step_tasks = HashMap::new();
        step_tasks.insert(step_id.to_string(), task_id);
        self.scheduler
            .add_execution(exec_id, Id("project".into()), dag, step_tasks);

        self.tick_scheduler()
    }

    fn add_steps(&mut self, execution_id: Id, steps: Vec<StepDef>) -> Vec<Event> {
        let now = chrono::Utc::now();

        // Look up existing step→task mappings for cross-dep wiring.
        let existing_steps = match self.db.get_execution_steps(&execution_id) {
            Ok(s) => s,
            Err(e) => {
                return vec![Event::StatusMessage(format!(
                    "failed to load existing steps: {e}"
                ))];
            }
        };
        let mut existing_task_ids: HashMap<String, Id> = HashMap::new();
        for (step_id, task_id) in &existing_steps {
            existing_task_ids.insert(step_id.clone(), task_id.clone());
        }

        // Create tasks and execution_steps for new steps.
        let mut new_step_tasks: HashMap<String, Id> = HashMap::new();
        let mut new_task_ids: HashMap<String, Id> = HashMap::new();

        for step in &steps {
            let task_id = Id::new("task");
            let task = Task {
                id: task_id.clone(),
                session_id: Some(self.session_id.clone()),
                title: step.title.clone(),
                description: Some(step.description.clone()),
                status: TaskStatus::Pending,
                assigned_to: None,
                copy_path: None,
                branch: None,
                base_branch: None,
                tier: Some(step.tier),
                current_activity: None,
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = self.db.insert_task(&task) {
                tracing::error!(step_id = %step.id, error = %e, "failed to insert task for new step");
                continue;
            }
            if let Err(e) = self.db.insert_execution_step(&execution_id, &step.id, &task_id) {
                tracing::error!(step_id = %step.id, error = %e, "failed to insert execution step");
                continue;
            }
            new_step_tasks.insert(step.id.clone(), task_id.clone());
            new_task_ids.insert(step.id.clone(), task_id);
        }

        // Create task dependencies (referencing both new and existing steps).
        let all_task_ids: HashMap<String, Id> = existing_task_ids
            .into_iter()
            .chain(new_task_ids.iter().map(|(k, v)| (k.clone(), v.clone())))
            .collect();

        for step in &steps {
            let Some(task_id) = new_task_ids.get(&step.id) else {
                continue;
            };
            for dep in &step.needs {
                if let Some(dep_task_id) = all_task_ids.get(&dep.step_id) {
                    let _ = self.db.insert_dependency(task_id, dep_task_id);
                }
            }
        }

        // Build step data for DAG add_steps.
        let step_data: Vec<_> = steps
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    s.title.clone(),
                    s.description.clone(),
                    Some(s.tier),
                    s.checkpoint,
                    s.needs
                        .iter()
                        .map(|d| (d.step_id.clone(), d.condition))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        if let Err(e) =
            self.scheduler
                .add_steps_to_execution(&execution_id.0, &step_data, new_step_tasks)
        {
            return vec![Event::StatusMessage(format!(
                "failed to add steps to execution: {e}"
            ))];
        }

        tracing::info!(
            execution_id = %execution_id,
            new_steps = steps.len(),
            "added steps to running execution"
        );

        self.tick_scheduler()
    }

    fn worker_done(&mut self, result: WorkerResult) -> Vec<Event> {
        let mut events = Vec::new();

        match result.outcome {
            WorkerOutcome::Success { output } => {
                // Store output.
                if let Some(ref out) = output {
                    let _ = self.db.insert_task_output(&result.task_id, out);
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
                let _ = self.db.insert_merge_request(&mr);
                let _ = self.db.update_task_status(&result.task_id, TaskStatus::Done);

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
            WorkerOutcome::NoChanges => {
                let _ = self
                    .db
                    .update_task_status(&result.task_id, TaskStatus::Failed);
                self.notify_scheduler_failed(&result.execution_id, &result.step_id);

                events.push(Event::WorkerFailed {
                    task_id: result.task_id.0.clone(),
                    title: result.title.clone(),
                    error: "completed without committing changes".into(),
                });
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
                    let _ = self
                        .db
                        .update_task_status(&result.task_id, TaskStatus::Failed);

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

    fn merge_done(&mut self, result: MergeResult) -> Vec<Event> {
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
                let _ = self.db.update_merge_status(&mr.id, MergeStatus::Conflicted);
                let _ = self.db.update_merge_review_note(&mr.id, detail);
                let _ = self
                    .db
                    .update_task_status(&mr.task_id, TaskStatus::Blocked);
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
                let _ = self.db.update_merge_status(&mr.id, MergeStatus::Resolving);
                let detail = format!(
                    "merge conflict in {} file(s): {}",
                    conflict_files.len(),
                    conflict_files.join(", ")
                );
                let _ = self.db.update_merge_review_note(&mr.id, &detail);
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
                let _ = self.db.update_merge_status(&mr.id, MergeStatus::Failed);
                let _ = self.db.update_merge_review_note(&mr.id, detail);
                let _ = self
                    .db
                    .update_task_status(&mr.task_id, TaskStatus::Failed);
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
                let _ = self.db.update_merge_status(&mr.id, MergeStatus::Failed);
                let _ = self.db.update_merge_review_note(&mr.id, detail);
                let _ = self
                    .db
                    .update_task_status(&mr.task_id, TaskStatus::Failed);
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

    fn retry_task(&mut self, task_id: Id) -> Vec<Event> {
        // Reset the task to Pending in the DB.
        let _ = self.db.update_task_status(&task_id, TaskStatus::Pending);
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

    fn pause(&mut self, target: Target) -> Vec<Event> {
        let mut events = Vec::new();
        match target {
            Target::Execution(exec_id) => {
                self.scheduler.pause_execution(&exec_id);
                if let Ok(exec) = self.db.get_running_executions(None)
                    && let Some(e) = exec.iter().find(|e| e.id.0 == exec_id)
                {
                    let _ = self
                        .db
                        .update_execution_status(&e.id, ExecutionStatus::Paused);
                }
            }
            Target::Node {
                execution_id,
                step_id,
            } => {
                if let Some(session_id) = self.scheduler.pause_node(&execution_id, &step_id) {
                    events.push(Event::KillSession { session_id });
                }
            }
        }
        events
    }

    fn resume(&mut self, target: Target) -> Vec<Event> {
        match target {
            Target::Execution(exec_id) => {
                self.scheduler.resume_execution(&exec_id);
                if let Ok(exec) = self.db.get_running_executions(None)
                    && let Some(e) = exec.iter().find(|e| e.id.0 == exec_id)
                {
                    let _ = self
                        .db
                        .update_execution_status(&e.id, ExecutionStatus::Running);
                }
                self.tick_scheduler()
            }
            Target::Node {
                execution_id,
                step_id,
            } => {
                self.scheduler.resume_node(&execution_id, &step_id);
                self.tick_scheduler()
            }
        }
    }

    fn cancel(&mut self, target: Target) -> Vec<Event> {
        let mut events = Vec::new();
        match target {
            Target::Execution(exec_id) => {
                let to_kill = self.scheduler.cancel_execution(&exec_id);
                for (task_id, session_id) in to_kill {
                    let _ = self.db.update_task_status(&task_id, TaskStatus::Cancelled);
                    events.push(Event::KillSession { session_id });
                }
                // Update execution status.
                let eid = Id(exec_id);
                let _ = self
                    .db
                    .update_execution_status(&eid, ExecutionStatus::Aborted);
            }
            Target::Node {
                execution_id,
                step_id,
            } => {
                if let Some(session_id) = self.scheduler.cancel_node(&execution_id, &step_id) {
                    events.push(Event::KillSession { session_id });
                }
            }
        }
        events.extend(self.tick_scheduler());
        events
    }

    fn stop_all(&mut self) -> Vec<Event> {
        let aborted = self.scheduler.abort_all();
        let count = aborted.len();
        let mut events = Vec::new();
        for (_exec_id, _step_id, task_id) in &aborted {
            let _ = self.db.update_task_status(task_id, TaskStatus::Failed);
        }
        events.push(Event::AllStopped { count });
        events
    }

    fn monitor_tick(&mut self, workers: Vec<(String, String, std::time::Instant)>) -> Vec<Event> {
        let actions = self.monitor.tick(&workers);
        let mut events = Vec::new();
        for action in actions {
            match action {
                MonitorAction::CancelSession {
                    session_id,
                    task_id,
                    stale_secs,
                } => {
                    events.push(Event::MonitorCancel {
                        session_id,
                        task_id,
                        stale_secs,
                    });
                }
                MonitorAction::Escalation(msg) => {
                    events.push(Event::MonitorEscalation(msg));
                }
            }
        }
        events
    }

    fn discover_from_db(&mut self) -> Vec<Event> {
        let mut found_new = false;

        // Discover new executions created by MCP tools in this session.
        if let Ok(executions) = self.db.get_running_executions(Some(&self.session_id)) {
            for exec in executions {
                if self.scheduler.has_execution(&exec.id.0) {
                    continue;
                }
                self.register_execution_from_db(&exec.id);
                found_new = true;
            }
        }

        // Wrap orphan ready tasks (this session only) in single-node executions.
        if let Ok(orphans) = self.db.get_orphan_ready_tasks(&self.session_id) {
            for task in orphans {
                let exec_id = Id::new("exec");
                let step_id = "task";
                let dag = Dag::single(
                    step_id,
                    &task.title,
                    task.description.as_deref().unwrap_or(""),
                    task.tier,
                );
                let mut step_tasks = HashMap::new();
                step_tasks.insert(step_id.to_string(), task.id.clone());

                let execution = Execution {
                    id: exec_id.clone(),
                    session_id: Some(self.session_id.clone()),
                    status: ExecutionStatus::Running,
                    created_at: chrono::Utc::now(),
                };
                let _ = self.db.insert_execution(&execution);
                let _ = self.db.insert_execution_step(&exec_id, step_id, &task.id);

                self.scheduler.add_execution(
                    exec_id,
                    Id("project".into()),
                    dag,
                    step_tasks,
                );
                found_new = true;
            }
        }

        if found_new {
            self.tick_scheduler()
        } else {
            Vec::new()
        }
    }

    fn check_signals(&mut self) -> Vec<Event> {
        let events_dir = match &self.events_dir {
            Some(d) => d.clone(),
            None => return Vec::new(),
        };

        if !events_dir.exists() {
            return Vec::new();
        }

        let entries = match std::fs::read_dir(&events_dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let mut found = false;
        let mut events = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            // Read and delete the signal file.
            if let Ok(content) = std::fs::read_to_string(&path) {
                let _ = std::fs::remove_file(&path);
                if let Ok(signal) = serde_json::from_str::<serde_json::Value>(&content) {
                    let signal_type = signal
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match signal_type {
                        "execution_created" | "task_created" | "steps_added" => {
                            found = true;
                        }
                        "resume" => {
                            let target = parse_signal_target(&signal);
                            if let Some(t) = target {
                                events.extend(self.resume(t));
                            }
                        }
                        "stop_all" => {
                            return self.stop_all();
                        }
                        "pause" => {
                            let target = parse_signal_target(&signal);
                            if let Some(t) = target {
                                events.extend(self.pause(t));
                            }
                        }
                        "cancel" => {
                            let target = parse_signal_target(&signal);
                            if let Some(t) = target {
                                events.extend(self.cancel(t));
                            }
                        }
                        "worker_report" => {
                            if let (Some(tid), Some(status)) = (
                                signal.get("task_id").and_then(|v| v.as_str()),
                                signal.get("status").and_then(|v| v.as_str()),
                            ) {
                                events.push(Event::WorkerReport {
                                    task_id: tid.to_string(),
                                    status: status.to_string(),
                                });
                            }
                        }
                        "mail" => {
                            if let (Some(from), Some(to), Some(subject)) = (
                                signal.get("from").and_then(|v| v.as_str()),
                                signal.get("to").and_then(|v| v.as_str()),
                                signal.get("subject").and_then(|v| v.as_str()),
                            ) {
                                let msg_id = signal.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
                                let priority = signal.get("priority").and_then(|v| v.as_str()).unwrap_or("normal");
                                events.push(Event::Mail {
                                    message_id: msg_id.to_string(),
                                    from: from.to_string(),
                                    to: to.to_string(),
                                    subject: subject.to_string(),
                                    priority: priority.to_string(),
                                });
                            }
                        }
                        _ => {
                            tracing::warn!(path = %path.display(), "unknown signal type: {signal_type}");
                        }
                    }
                }
            }
        }

        if found {
            // Re-discover from DB to pick up whatever was created.
            events.extend(self.discover_from_db());
        }
        events
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Tick the scheduler and translate actions into events.
    fn tick_scheduler(&mut self) -> Vec<Event> {
        let actions = self.scheduler.tick();
        let mut events = Vec::new();

        for action in &actions {
            match action {
                SchedulerAction::SpawnWorker {
                    task_id,
                    title,
                    description,
                    tier,
                    execution_id,
                    step_id,
                    upstream_outputs,
                    ..
                } => {
                    // Update task status in DB.
                    let _ = self.db.update_task_status(task_id, TaskStatus::Running);

                    events.push(Event::SpawnWorker {
                        task_id: task_id.clone(),
                        title: title.clone(),
                        description: description.clone(),
                        tier: *tier,
                        execution_id: execution_id.clone(),
                        step_id: step_id.clone(),
                        upstream_outputs: upstream_outputs.clone(),
                    });
                }
                SchedulerAction::TaskBlocked {
                    task_id,
                    execution_id: _,
                    step_id: _,
                } => {
                    let _ = self.db.update_task_status(task_id, TaskStatus::Blocked);
                }
                SchedulerAction::ExecutionComplete { execution_id } => {
                    let _ = self
                        .db
                        .update_execution_status(execution_id, ExecutionStatus::Done);
                    events.push(Event::ExecutionComplete {
                        execution_id: execution_id.0.clone(),
                    });
                }
                SchedulerAction::ExecutionFailed { execution_id } => {
                    let _ = self
                        .db
                        .update_execution_status(execution_id, ExecutionStatus::Failed);
                    events.push(Event::ExecutionFailed {
                        execution_id: execution_id.0.clone(),
                    });
                }
            }
        }

        events
    }

    /// Register an execution created externally (via MCP) with the in-memory scheduler.
    fn register_execution_from_db(&mut self, execution_id: &Id) {
        let steps = match self.db.get_execution_steps(execution_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(execution_id = %execution_id, error = %e, "failed to load steps");
                return;
            }
        };

        let mut step_tasks = HashMap::new();
        for (step_id, task_id) in &steps {
            step_tasks.insert(step_id.clone(), task_id.clone());
        }

        // Prefer saved DAG (preserves edge conditions, checkpoint flags, WorkerDone status).
        let dag = if let Ok(Some(saved_dag)) = self.db.load_dag(execution_id) {
            saved_dag
        } else {
            // Fall back to rebuilding from steps for old executions.
            let mut task_to_step: HashMap<Id, String> = HashMap::new();
            for (step_id, task_id) in &steps {
                task_to_step.insert(task_id.clone(), step_id.clone());
            }
            let mut step_data = Vec::new();
            for (step_id, task_id) in &steps {
                let task = match self.db.get_task(task_id) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let dep_task_ids = self.db.get_dependencies(task_id).unwrap_or_default();
                let dep_step_ids: Vec<String> = dep_task_ids
                    .iter()
                    .filter_map(|dep_tid| task_to_step.get(dep_tid).cloned())
                    .collect();
                step_data.push((
                    step_id.clone(),
                    task.title,
                    task.description.unwrap_or_default(),
                    task.tier,
                    dep_step_ids,
                ));
            }
            Dag::from_steps(&step_data)
        };

        self.scheduler.add_execution(
            execution_id.clone(),
            Id("project".into()),
            dag,
            step_tasks,
        );
        tracing::info!(execution_id = %execution_id, steps = steps.len(), "execution registered from DB");
    }

    /// Notify the scheduler that a step failed.
    fn notify_scheduler_failed(&mut self, execution_id: &Option<Id>, step_id: &Option<String>) {
        if let (Some(eid), Some(sid)) = (execution_id, step_id) {
            self.scheduler.step_failed(&eid.0, sid);
        }
    }

    /// Check if a failed task should be retried (timeout/stuck only).
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
        let is_timeout = error.contains("timed out") || error.contains("stuck");
        if !is_timeout {
            return false;
        }
        if self.monitor.should_block_retry(&task_id.0) {
            return false;
        }
        let retry_count = self.monitor.record_retry(&task_id.0);
        let _ = self.db.update_task_status(task_id, TaskStatus::Pending);

        // Reset the DAG node from Failed back to Pending/Ready.
        if let (Some(eid), Some(sid)) = (execution_id, step_id) {
            self.scheduler.retry_node(&eid.0, sid);
        }

        tracing::info!(
            task_id = %task_id, title,
            retry = retry_count,
            "retrying timed-out task"
        );
        true
    }

    /// Register the session_id for a step (called by CLI after spawn).
    pub fn set_step_session(&mut self, execution_id: &str, step_id: &str, session_id: String) {
        self.scheduler
            .set_step_session(execution_id, step_id, session_id);
    }

    /// Notify the monitor that a session has ended.
    pub fn session_ended(&mut self, session_id: &str) {
        self.monitor.session_ended(session_id);
    }

    /// Get current worker counts.
    pub fn worker_counts(&self) -> (usize, usize, usize, usize) {
        self.scheduler.worker_counts()
    }

    /// Get all running steps: (execution_id, step_id, task_id).
    pub fn running_steps(&self) -> Vec<(String, String, Id)> {
        self.scheduler.running_steps()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor;

    fn test_orchestrator() -> Orchestrator {
        let db = Db::open_in_memory().unwrap();
        Orchestrator::new(db, Limits::default(), "test-session".into())
    }

    #[test]
    fn create_single_task_spawns_worker() {
        let mut orch = test_orchestrator();
        let events = orch.handle(Command::CreateTask {
            title: "Fix bug".into(),
            description: Some("Fix the auth bug".into()),
            tier: Tier::Standard,
        });

        let spawns: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawns.len(), 1);

        if let Event::SpawnWorker { title, tier, .. } = &spawns[0] {
            assert_eq!(title, "Fix bug");
            assert_eq!(*tier, Tier::Standard);
        }
    }

    #[test]
    fn create_execution_spawns_root_tasks() {
        let mut orch = test_orchestrator();
        let events = orch.handle(Command::CreateExecution {
            steps: vec![
                StepDef {
                    id: "design".into(),
                    title: "Design".into(),
                    description: "Design the feature".into(),
                    tier: Tier::Heavy,
                    needs: vec![],
                    checkpoint: false,
                },
                StepDef {
                    id: "implement".into(),
                    title: "Implement".into(),
                    description: "Implement the feature".into(),
                    tier: Tier::Standard,
                    needs: vec!["design".into()],
                    checkpoint: false,
                },
                StepDef {
                    id: "test".into(),
                    title: "Test".into(),
                    description: "Write tests".into(),
                    tier: Tier::Light,
                    needs: vec!["design".into()],
                    checkpoint: false,
                },
            ],
        });

        // Only design should spawn (it's the root with no deps).
        let spawns: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawns.len(), 1);
        if let Event::SpawnWorker { title, tier, .. } = &spawns[0] {
            assert_eq!(title, "Design");
            assert_eq!(*tier, Tier::Heavy);
        }

        // DB should have 3 tasks and 1 execution.
        let tasks = orch.db().list_tasks().unwrap();
        assert_eq!(tasks.len(), 3);
    }

    #[test]
    fn worker_success_queues_merge() {
        let mut orch = test_orchestrator();

        // Create a task to get an execution/step context.
        let events = orch.handle(Command::CreateTask {
            title: "Fix bug".into(),
            description: Some("desc".into()),
            tier: Tier::Standard,
        });
        let (task_id, exec_id, step_id) = match &events[0] {
            Event::SpawnWorker {
                task_id,
                execution_id,
                step_id,
                ..
            } => (task_id.clone(), execution_id.clone(), step_id.clone()),
            _ => panic!("expected SpawnWorker"),
        };

        // Worker succeeds.
        let events = orch.handle(Command::WorkerDone(WorkerResult {
            task_id: task_id.clone(),
            execution_id: Some(exec_id),
            step_id: Some(step_id),
            title: "Fix bug".into(),
            branch: "task/fix-bug".into(),
            outcome: WorkerOutcome::Success {
                output: Some("Fixed the bug".into()),
            },
        }));

        assert!(events.iter().any(|e| matches!(e, Event::WorkerCompleted { .. })));
        assert!(events.iter().any(|e| matches!(e, Event::QueueMerge(_))));

        // Task output stored in DB.
        let output = orch.db().get_task_output(&task_id).unwrap();
        assert_eq!(output.as_deref(), Some("Fixed the bug"));
    }

    #[test]
    fn worker_no_changes_fails() {
        let mut orch = test_orchestrator();
        let events = orch.handle(Command::CreateTask {
            title: "Fix bug".into(),
            description: None,
            tier: Tier::Standard,
        });
        let task_id = match &events[0] {
            Event::SpawnWorker { task_id, .. } => task_id.clone(),
            _ => panic!("expected SpawnWorker"),
        };

        let events = orch.handle(Command::WorkerDone(WorkerResult {
            task_id: task_id.clone(),
            execution_id: None,
            step_id: None,
            title: "Fix bug".into(),
            branch: "task/fix-bug".into(),
            outcome: WorkerOutcome::NoChanges,
        }));

        assert!(events
            .iter()
            .any(|e| matches!(e, Event::WorkerFailed { .. })));
        let task = orch.db().get_task(&task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
    }

    #[test]
    fn merge_landed_advances_dag() {
        let mut orch = test_orchestrator();

        // Create a 2-step execution: a → b.
        let events = orch.handle(Command::CreateExecution {
            steps: vec![
                StepDef {
                    id: "a".into(),
                    title: "Step A".into(),
                    description: "First".into(),
                    tier: Tier::Standard,
                    needs: vec![],
                    checkpoint: false,
                },
                StepDef {
                    id: "b".into(),
                    title: "Step B".into(),
                    description: "Second".into(),
                    tier: Tier::Standard,
                    needs: vec!["a".into()],
                    checkpoint: false,
                },
            ],
        });

        let (task_id_a, exec_id, step_id_a) = match &events[0] {
            Event::SpawnWorker {
                task_id,
                execution_id,
                step_id,
                ..
            } => (task_id.clone(), execution_id.clone(), step_id.clone()),
            _ => panic!("expected SpawnWorker"),
        };

        // Worker A succeeds.
        let events = orch.handle(Command::WorkerDone(WorkerResult {
            task_id: task_id_a.clone(),
            execution_id: Some(exec_id.clone()),
            step_id: Some(step_id_a),
            title: "Step A".into(),
            branch: "task/step-a".into(),
            outcome: WorkerOutcome::Success {
                output: Some("A output".into()),
            },
        }));
        // Should get QueueMerge but NOT SpawnWorker for B yet (merge hasn't landed).
        assert!(events.iter().any(|e| matches!(e, Event::QueueMerge(_))));
        let b_spawns: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::SpawnWorker { title, .. } if title == "Step B"))
            .collect();
        assert!(b_spawns.is_empty());

        // Get the merge request.
        let mr = match events.iter().find(|e| matches!(e, Event::QueueMerge(_))) {
            Some(Event::QueueMerge(mr)) => mr.clone(),
            _ => panic!("expected QueueMerge"),
        };

        // Merge lands.
        let events = orch.handle(Command::MergeDone(MergeResult {
            mr_id: mr.id.clone(),
            outcome: MergeOutcome::Merged,
        }));

        // Now B should spawn.
        let b_spawns: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::SpawnWorker { title, .. } if title == "Step B"))
            .collect();
        assert_eq!(b_spawns.len(), 1);

        // B should have upstream output from A.
        if let Event::SpawnWorker {
            upstream_outputs, ..
        } = &b_spawns[0]
        {
            assert_eq!(upstream_outputs.len(), 1);
            assert_eq!(upstream_outputs[0].0, "Step A");
            assert_eq!(upstream_outputs[0].1, "A output");
        }
    }

    #[test]
    fn worker_timeout_retries() {
        let mut orch = test_orchestrator();
        let events = orch.handle(Command::CreateTask {
            title: "Slow task".into(),
            description: None,
            tier: Tier::Standard,
        });
        let (task_id, exec_id, step_id) = match &events[0] {
            Event::SpawnWorker {
                task_id,
                execution_id,
                step_id,
                ..
            } => (task_id.clone(), execution_id.clone(), step_id.clone()),
            _ => panic!("expected SpawnWorker"),
        };

        // First failure: timeout → should retry.
        let events = orch.handle(Command::WorkerDone(WorkerResult {
            task_id: task_id.clone(),
            execution_id: Some(exec_id.clone()),
            step_id: Some(step_id.clone()),
            title: "Slow task".into(),
            branch: "task/slow".into(),
            outcome: WorkerOutcome::Failed {
                error: "worker timed out after 30 minutes".into(),
            },
        }));
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::WorkerFailed { error, .. } if error.contains("retrying"))));

        // Retry resets the DAG node, and tick_scheduler re-dispatches it immediately.
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::SpawnWorker { .. })));
        let task = orch.db().get_task(&task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Running);
    }

    #[test]
    fn worker_non_timeout_failure_no_retry() {
        let mut orch = test_orchestrator();
        let events = orch.handle(Command::CreateTask {
            title: "Bad task".into(),
            description: None,
            tier: Tier::Standard,
        });
        let task_id = match &events[0] {
            Event::SpawnWorker { task_id, .. } => task_id.clone(),
            _ => panic!("expected SpawnWorker"),
        };

        let events = orch.handle(Command::WorkerDone(WorkerResult {
            task_id: task_id.clone(),
            execution_id: None,
            step_id: None,
            title: "Bad task".into(),
            branch: "task/bad".into(),
            outcome: WorkerOutcome::Failed {
                error: "compilation error".into(),
            },
        }));
        // Should NOT retry non-timeout errors.
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::WorkerFailed { error, .. } if !error.contains("retrying"))));

        let task = orch.db().get_task(&task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
    }

    #[test]
    fn stop_all_aborts_everything() {
        let mut orch = test_orchestrator();
        orch.handle(Command::CreateTask {
            title: "Task 1".into(),
            description: None,
            tier: Tier::Standard,
        });

        let events = orch.handle(Command::StopAll);
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::AllStopped { count } if *count > 0)));
    }

    #[test]
    fn monitor_tick_produces_cancel() {
        let mut orch = test_orchestrator();
        let stale_time =
            std::time::Instant::now() - std::time::Duration::from_secs(monitor::STALE_CANCEL_SECS + 10);
        let workers = vec![("sess-1".into(), "task-1".into(), stale_time)];

        let events = orch.handle(Command::MonitorTick { workers });
        assert!(events.iter().any(|e| matches!(
            e,
            Event::MonitorCancel {
                session_id, ..
            } if session_id == "sess-1"
        )));
    }

    #[test]
    fn discover_from_db_wraps_orphan_tasks() {
        let mut orch = test_orchestrator();
        let now = chrono::Utc::now();

        // Insert an orphan ready task in the current session (not part of any execution).
        let task_id = Id::new("task");
        let task = Task {
            id: task_id.clone(),
            session_id: Some("test-session".into()),
            title: "Orphan task".into(),
            description: None,
            status: TaskStatus::Pending,
            assigned_to: None,
            copy_path: None,
            branch: None,
            base_branch: None,
            tier: Some(Tier::Light),
            current_activity: None,
            created_at: now,
            updated_at: now,
        };
        orch.db().insert_task(&task).unwrap();

        let events = orch.handle(Command::DiscoverFromDb);
        let spawns: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawns.len(), 1);
    }

    #[test]
    fn merge_conflicted_marks_task_blocked() {
        let mut orch = test_orchestrator();
        let events = orch.handle(Command::CreateTask {
            title: "Conflict task".into(),
            description: None,
            tier: Tier::Standard,
        });
        let (task_id, exec_id, step_id) = match &events[0] {
            Event::SpawnWorker {
                task_id,
                execution_id,
                step_id,
                ..
            } => (task_id.clone(), execution_id.clone(), step_id.clone()),
            _ => panic!("expected SpawnWorker"),
        };

        // Worker succeeds → merge queued.
        let events = orch.handle(Command::WorkerDone(WorkerResult {
            task_id: task_id.clone(),
            execution_id: Some(exec_id),
            step_id: Some(step_id),
            title: "Conflict task".into(),
            branch: "task/conflict".into(),
            outcome: WorkerOutcome::Success { output: None },
        }));
        let mr = match events.iter().find(|e| matches!(e, Event::QueueMerge(_))) {
            Some(Event::QueueMerge(mr)) => mr.clone(),
            _ => panic!("expected QueueMerge"),
        };

        // Merge conflicts.
        let events = orch.handle(Command::MergeDone(MergeResult {
            mr_id: mr.id.clone(),
            outcome: MergeOutcome::Conflicted("conflict in main.rs".into()),
        }));
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::MergeConflicted { .. })));

        let task = orch.db().get_task(&task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Blocked);
    }

    #[test]
    fn pause_resume_execution() {
        let mut orch = test_orchestrator();
        let events = orch.handle(Command::CreateExecution {
            steps: vec![
                StepDef {
                    id: "a".into(),
                    title: "A".into(),
                    description: "first".into(),
                    tier: Tier::Standard,
                    needs: vec![],
                    checkpoint: false,
                },
                StepDef {
                    id: "b".into(),
                    title: "B".into(),
                    description: "second".into(),
                    tier: Tier::Standard,
                    needs: vec!["a".into()],
                    checkpoint: false,
                },
            ],
        });
        let exec_id = match &events[0] {
            Event::SpawnWorker { execution_id, .. } => execution_id.0.clone(),
            _ => panic!("expected SpawnWorker"),
        };

        // Pause should produce no errors.
        let events = orch.handle(Command::Pause(Target::Execution(exec_id.clone())));
        assert!(events.is_empty());

        // Resume should re-evaluate.
        let events = orch.handle(Command::Resume(Target::Execution(exec_id)));
        // No new spawns expected since A is still running (not completed).
        let spawns: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::SpawnWorker { .. }))
            .collect();
        assert!(spawns.is_empty());
    }
}
