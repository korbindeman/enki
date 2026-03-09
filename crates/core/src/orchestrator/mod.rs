mod execution;
mod merges;
mod signals;
mod types;
mod workers;

pub use types::*;

use std::path::PathBuf;

use crate::db::Db;
use crate::monitor::{MonitorAction, MonitorState};
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

    pub fn set_events_dir(&mut self, path: PathBuf) {
        self.events_dir = Some(path);
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

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

    fn persist_dags(&self) {
        for exec_id in self.scheduler.active_executions() {
            if let Some(dag) = self.scheduler.get_dag(exec_id)
                && let Err(e) = self.db.save_dag(&Id(exec_id.to_string()), dag) {
                    tracing::warn!(execution_id = %exec_id, error = %e, "failed to persist dag");
                }
        }
    }

    fn pause(&mut self, target: Target) -> Vec<Event> {
        let mut events = Vec::new();
        match target {
            Target::Execution(exec_id) => {
                self.scheduler.pause_execution(&exec_id);
                if let Ok(exec) = self.db.get_running_executions(None)
                    && let Some(e) = exec.iter().find(|e| e.id.0 == exec_id)
                {
                    if let Err(e2) = self
                        .db
                        .update_execution_status(&e.id, ExecutionStatus::Paused)
                    {
                        tracing::warn!(execution_id = %e.id, error = %e2, "failed to update execution status to Paused");
                    }
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
                    if let Err(e2) = self
                        .db
                        .update_execution_status(&e.id, ExecutionStatus::Running)
                    {
                        tracing::warn!(execution_id = %e.id, error = %e2, "failed to update execution status to Running");
                    }
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
                    if let Err(e) = self.db.update_task_status(&task_id, TaskStatus::Cancelled) {
                        tracing::warn!(task_id = %task_id, error = %e, "failed to update task status to Cancelled");
                    }
                    events.push(Event::KillSession { session_id });
                }
                // Update execution status.
                let eid = Id(exec_id);
                if let Err(e) = self
                    .db
                    .update_execution_status(&eid, ExecutionStatus::Aborted)
                {
                    tracing::warn!(execution_id = %eid, error = %e, "failed to update execution status to Aborted");
                }
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
            if let Err(e) = self.db.update_task_status(task_id, TaskStatus::Failed) {
                tracing::warn!(task_id = %task_id, error = %e, "failed to update task status to Failed during stop_all");
            }
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
                    role,
                    ..
                } => {
                    // Update task status in DB.
                    if let Err(e) = self.db.update_task_status(task_id, TaskStatus::Running) {
                        tracing::warn!(task_id = %task_id, error = %e, "failed to update task status to Running");
                        events.push(Event::StatusMessage(format!("failed to update task status: {e}")));
                    }

                    events.push(Event::SpawnWorker {
                        task_id: task_id.clone(),
                        title: title.clone(),
                        description: description.clone(),
                        tier: *tier,
                        execution_id: execution_id.clone(),
                        step_id: step_id.clone(),
                        upstream_outputs: upstream_outputs.clone(),
                        role: role.clone(),
                    });
                }
                SchedulerAction::TaskBlocked {
                    task_id,
                    execution_id: _,
                    step_id: _,
                } => {
                    if let Err(e) = self.db.update_task_status(task_id, TaskStatus::Blocked) {
                        tracing::warn!(task_id = %task_id, error = %e, "failed to update task status to Blocked");
                    }
                }
                SchedulerAction::ExecutionComplete { execution_id } => {
                    if let Err(e) = self
                        .db
                        .update_execution_status(execution_id, ExecutionStatus::Done)
                    {
                        tracing::warn!(execution_id = %execution_id, error = %e, "failed to update execution status to Done");
                    }
                    events.push(Event::ExecutionComplete {
                        execution_id: execution_id.0.clone(),
                    });
                }
                SchedulerAction::ExecutionFailed { execution_id } => {
                    if let Err(e) = self
                        .db
                        .update_execution_status(execution_id, ExecutionStatus::Failed)
                    {
                        tracing::warn!(execution_id = %execution_id, error = %e, "failed to update execution status to Failed");
                    }
                    events.push(Event::ExecutionFailed {
                        execution_id: execution_id.0.clone(),
                    });
                }
            }
        }
        events
    }
    pub fn set_step_session(&mut self, execution_id: &str, step_id: &str, session_id: String) {
        self.scheduler
            .set_step_session(execution_id, step_id, session_id);
    }

    pub fn session_ended(&mut self, session_id: &str) {
        self.monitor.session_ended(session_id);
    }

    pub fn worker_counts(&self) -> (usize, usize, usize, usize) {
        self.scheduler.worker_counts()
    }

    pub fn running_steps(&self) -> Vec<(String, String, Id)> {
        self.scheduler.running_steps()
    }
}

#[cfg(test)]
mod tests;
