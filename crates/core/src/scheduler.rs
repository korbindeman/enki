use std::collections::HashMap;

use crate::dag::{Dag, EdgeCondition, NodeStatus};
use crate::types::*;

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("db: {0}")]
    Db(#[from] crate::db::DbError),
    #[error("execution not found: {0}")]
    ExecutionNotFound(String),
    #[error("no available worker slots")]
    NoWorkerSlots,
}

pub type Result<T> = std::result::Result<T, SchedulerError>;

/// Concurrency limits per model tier.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_workers: usize,
    pub max_heavy: usize,
    pub max_standard: usize,
    pub max_light: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_workers: 10,
            max_heavy: 5,
            max_standard: 5,
            max_light: 10,
        }
    }
}

/// An action the scheduler wants the runtime to take.
/// The scheduler produces these; the CLI/runtime executes them.
#[derive(Debug, Clone)]
pub enum SchedulerAction {
    /// Spawn a worker for this task.
    SpawnWorker {
        task_id: Id,
        project_id: Id,
        title: String,
        description: String,
        tier: Tier,
        execution_id: Id,
        step_id: String,
        /// Outputs from completed upstream steps: (step_title, output_text).
        upstream_outputs: Vec<(String, String)>,
    },
    /// A task was blocked because a dependency failed.
    TaskBlocked {
        task_id: Id,
        execution_id: Id,
        step_id: String,
    },
    /// An execution is fully complete (all nodes done).
    ExecutionComplete {
        execution_id: Id,
    },
    /// An execution has failures and nothing left to run.
    ExecutionFailed {
        execution_id: Id,
    },
}

/// Tracks in-flight executions and decides what to run next.
pub struct Scheduler {
    limits: Limits,
    /// Active DAGs keyed by execution ID.
    executions: HashMap<String, ExecutionState>,
    /// How many workers are currently running per tier.
    running: TierCounts,
}

#[derive(Debug, Clone)]
struct ExecutionState {
    execution_id: Id,
    project_id: Id,
    dag: Dag,
    /// Maps step_id -> task_id for tasks we've created.
    step_tasks: HashMap<String, Id>,
    /// Maps step_id -> agent session_id for running steps.
    step_sessions: HashMap<String, String>,
    /// Captured outputs from completed steps, keyed by step_id.
    step_outputs: HashMap<String, String>,
    /// Step IDs that have already been reported as blocked via TaskBlocked actions.
    /// Prevents emitting duplicate TaskBlocked actions across ticks.
    reported_blocked: std::collections::HashSet<String>,
    /// When true, `tick()` skips this execution entirely.
    paused: bool,
}

#[derive(Debug, Clone, Default)]
struct TierCounts {
    heavy: usize,
    standard: usize,
    light: usize,
}

impl TierCounts {
    fn total(&self) -> usize {
        self.heavy + self.standard + self.light
    }

    fn can_run(&self, tier: Tier, limits: &Limits) -> bool {
        if self.total() >= limits.max_workers {
            return false;
        }
        match tier {
            Tier::Heavy => self.heavy < limits.max_heavy,
            Tier::Standard => self.standard < limits.max_standard,
            Tier::Light => self.light < limits.max_light,
        }
    }

    fn increment(&mut self, tier: Tier) {
        match tier {
            Tier::Heavy => self.heavy += 1,
            Tier::Standard => self.standard += 1,
            Tier::Light => self.light += 1,
        }
    }

    fn decrement(&mut self, tier: Tier) {
        match tier {
            Tier::Heavy => self.heavy = self.heavy.saturating_sub(1),
            Tier::Standard => self.standard = self.standard.saturating_sub(1),
            Tier::Light => self.light = self.light.saturating_sub(1),
        }
    }
}

impl Scheduler {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            executions: HashMap::new(),
            running: TierCounts::default(),
        }
    }

    /// Check whether an execution is already tracked by this scheduler.
    pub fn has_execution(&self, execution_id: &str) -> bool {
        self.executions.contains_key(execution_id)
    }

    /// Register a new execution with its DAG.
    /// `step_tasks` maps step IDs to their corresponding task IDs in the DB.
    pub fn add_execution(
        &mut self,
        execution_id: Id,
        project_id: Id,
        dag: Dag,
        step_tasks: HashMap<String, Id>,
    ) {
        self.executions.insert(
            execution_id.0.clone(),
            ExecutionState {
                execution_id,
                project_id,
                dag,
                step_tasks,
                step_sessions: HashMap::new(),
                step_outputs: HashMap::new(),
                reported_blocked: std::collections::HashSet::new(),
                paused: false,
            },
        );
    }

    /// One scheduling pass. Returns actions for the runtime to execute.
    pub fn tick(&mut self) -> Vec<SchedulerAction> {
        let mut actions = Vec::new();

        let exec_keys: Vec<String> = self.executions.keys().cloned().collect();

        for key in exec_keys {
            let exec = self.executions.get_mut(&key).unwrap();

            // Skip paused executions entirely.
            if exec.paused {
                continue;
            }

            // Emit TaskBlocked for newly blocked nodes
            let blocked: Vec<String> = exec
                .dag
                .blocked_nodes()
                .iter()
                .map(|s| s.to_string())
                .collect();
            for step_id in blocked {
                if exec.reported_blocked.insert(step_id.clone())
                    && let Some(task_id) = exec.step_tasks.get(&step_id) {
                        actions.push(SchedulerAction::TaskBlocked {
                            task_id: task_id.clone(),
                            execution_id: exec.execution_id.clone(),
                            step_id,
                        });
                    }
            }

            // Check for ready nodes we can assign
            let ready: Vec<String> = exec
                .dag
                .ready_nodes()
                .iter()
                .map(|s| s.to_string())
                .collect();

            for step_id in ready {
                let (title, description, tier, upstream_outputs) = {
                    let node = exec.dag.get(&step_id).unwrap();
                    let tier = node.tier.unwrap_or(Tier::Standard);

                    // Collect outputs from all completed upstream deps
                    let upstream: Vec<(String, String)> = node
                        .deps
                        .iter()
                        .filter_map(|edge| {
                            let dep = &exec.dag.nodes()[edge.target];
                            exec.step_outputs
                                .get(&dep.id)
                                .map(|out| (dep.title.clone(), out.clone()))
                        })
                        .collect();

                    (node.title.clone(), node.description.clone(), tier, upstream)
                };

                if !self.running.can_run(tier, &self.limits) {
                    continue;
                }

                exec.dag.mark_running(&step_id);
                self.running.increment(tier);

                let task_id = exec
                    .step_tasks
                    .get(&step_id)
                    .cloned()
                    .unwrap_or_else(|| Id::new("task"));

                actions.push(SchedulerAction::SpawnWorker {
                    task_id,
                    project_id: exec.project_id.clone(),
                    title,
                    description,
                    tier,
                    execution_id: exec.execution_id.clone(),
                    step_id: step_id.clone(),
                    upstream_outputs,
                });
            }

            // Check for completed/failed executions
            if exec.dag.is_complete() {
                actions.push(SchedulerAction::ExecutionComplete {
                    execution_id: exec.execution_id.clone(),
                });
            } else if exec.dag.has_failures() {
                let has_in_progress = exec
                    .dag
                    .nodes()
                    .iter()
                    .any(|n| matches!(n.status, NodeStatus::Running | NodeStatus::WorkerDone));
                let has_ready = !exec.dag.ready_nodes().is_empty();
                if !has_in_progress && !has_ready {
                    actions.push(SchedulerAction::ExecutionFailed {
                        execution_id: exec.execution_id.clone(),
                    });
                }
            }
        }

        // Clean up completed/failed executions
        for action in &actions {
            match action {
                SchedulerAction::ExecutionComplete { execution_id }
                | SchedulerAction::ExecutionFailed { execution_id } => {
                    self.executions.remove(&execution_id.0);
                }
                _ => {}
            }
        }

        actions
    }

    /// Notify the scheduler that a worker has finished (but merge hasn't landed yet).
    /// Decrements the tier count (worker process is gone), stores output, and
    /// transitions the DAG node to WorkerDone (fires Completed/Started edges).
    pub fn step_worker_done(
        &mut self,
        execution_id: &str,
        step_id: &str,
        output: Option<String>,
    ) {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            let tier = exec
                .dag
                .get(step_id)
                .map(|n| n.tier.unwrap_or(Tier::Standard))
                .unwrap_or(Tier::Standard);
            exec.dag.mark_worker_done(step_id);
            exec.step_sessions.remove(step_id);
            if let Some(out) = output {
                exec.step_outputs.insert(step_id.to_string(), out);
            }
            self.running.decrement(tier);
        }
    }

    /// Notify the scheduler that a merge has landed (step fully complete).
    /// Transitions the DAG node from WorkerDone to Done (fires Merged edges).
    /// Does NOT decrement tier count — that was done in `step_worker_done`.
    pub fn step_completed(
        &mut self,
        execution_id: &str,
        step_id: &str,
        output: Option<String>,
    ) {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            exec.dag.mark_done(step_id);
            if let Some(out) = output {
                exec.step_outputs.insert(step_id.to_string(), out);
            }
        }
    }

    /// Notify the scheduler that a step has failed.
    pub fn step_failed(&mut self, execution_id: &str, step_id: &str) {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            let tier = exec
                .dag
                .get(step_id)
                .map(|n| n.tier.unwrap_or(Tier::Standard))
                .unwrap_or(Tier::Standard);
            exec.dag.mark_failed(step_id);
            exec.step_sessions.remove(step_id);
            self.running.decrement(tier);
        }
    }

    /// Add new steps to a running execution's DAG.
    #[allow(clippy::type_complexity)]
    pub fn add_steps_to_execution(
        &mut self,
        execution_id: &str,
        steps: &[(
            String,
            String,
            String,
            Option<Tier>,
            bool,
            Vec<(String, EdgeCondition)>,
        )],
        new_step_tasks: HashMap<String, Id>,
    ) -> std::result::Result<(), String> {
        let exec = self
            .executions
            .get_mut(execution_id)
            .ok_or_else(|| format!("execution not found: {}", execution_id))?;
        exec.dag.add_steps(steps)?;
        exec.step_tasks.extend(new_step_tasks);
        Ok(())
    }

    /// Check if a step is a checkpoint node.
    pub fn is_checkpoint(&self, execution_id: &str, step_id: &str) -> bool {
        self.executions
            .get(execution_id)
            .and_then(|exec| exec.dag.get(step_id))
            .is_some_and(|node| node.checkpoint)
    }

    /// Record that a step is being handled by a specific ACP session.
    pub fn set_step_session(&mut self, execution_id: &str, step_id: &str, session_id: String) {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            exec.step_sessions.insert(step_id.to_string(), session_id);
        }
    }

    /// Look up which execution and step a task belongs to.
    pub fn find_task(&self, task_id: &Id) -> Option<(&str, &str)> {
        for exec in self.executions.values() {
            for (step_id, tid) in &exec.step_tasks {
                if tid.0 == task_id.0 {
                    return Some((&exec.execution_id.0, step_id));
                }
            }
        }
        None
    }

    /// Get current worker counts: (total, heavy, standard, light).
    pub fn worker_counts(&self) -> (usize, usize, usize, usize) {
        (
            self.running.total(),
            self.running.heavy,
            self.running.standard,
            self.running.light,
        )
    }

    /// Abort all active executions immediately.
    /// Returns a list of (execution_id, step_id, task_id) for every running step
    /// so the caller can kill sessions and update the DB.
    pub fn abort_all(&mut self) -> Vec<(String, String, Id)> {
        let mut aborted = Vec::new();
        for exec in self.executions.values() {
            for node in exec.dag.nodes() {
                if node.status == NodeStatus::Running
                    && let Some(task_id) = exec.step_tasks.get(&node.id) {
                        aborted.push((
                            exec.execution_id.0.clone(),
                            node.id.clone(),
                            task_id.clone(),
                        ));
                    }
            }
        }
        self.executions.clear();
        self.running = TierCounts::default();
        aborted
    }

    /// Get all (execution_id, step_id, task_id) triples where the step is still Running.
    pub fn running_steps(&self) -> Vec<(String, String, Id)> {
        let mut result = Vec::new();
        for exec in self.executions.values() {
            for node in exec.dag.nodes() {
                if node.status == NodeStatus::Running
                    && let Some(task_id) = exec.step_tasks.get(&node.id) {
                        result.push((
                            exec.execution_id.0.clone(),
                            node.id.clone(),
                            task_id.clone(),
                        ));
                    }
            }
        }
        result
    }

    /// Get all active execution IDs.
    pub fn active_executions(&self) -> Vec<&str> {
        self.executions.keys().map(|s| s.as_str()).collect()
    }

    /// Pause an entire execution. `tick()` will skip it.
    pub fn pause_execution(&mut self, execution_id: &str) -> bool {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            exec.paused = true;
            return true;
        }
        false
    }

    /// Resume a paused execution. Next `tick()` will process it.
    pub fn resume_execution(&mut self, execution_id: &str) -> bool {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            exec.paused = false;
            return true;
        }
        false
    }

    /// Cancel an entire execution. Returns `(task_id, session_id)` pairs
    /// for running steps that need to be killed.
    pub fn cancel_execution(&mut self, execution_id: &str) -> Vec<(Id, String)> {
        let mut to_kill = Vec::new();
        if let Some(exec) = self.executions.remove(execution_id) {
            for node in exec.dag.nodes() {
                if node.status == NodeStatus::Running
                    && let Some(task_id) = exec.step_tasks.get(&node.id)
                        && let Some(session_id) = exec.step_sessions.get(&node.id) {
                            to_kill.push((task_id.clone(), session_id.clone()));
                        }
            }
            // Decrement running counts for killed workers.
            for node in exec.dag.nodes() {
                if node.status == NodeStatus::Running {
                    let tier = node.tier.unwrap_or(Tier::Standard);
                    self.running.decrement(tier);
                }
            }
        }
        to_kill
    }

    /// Pause a single node within an execution.
    /// Returns `Some(session_id)` if the node was Running (caller must kill it).
    pub fn pause_node(&mut self, execution_id: &str, step_id: &str) -> Option<String> {
        let exec = self.executions.get_mut(execution_id)?;
        let was_running = exec.dag.pause_node(step_id)?;
        if was_running {
            let tier = exec.dag.get(step_id).map(|n| n.tier.unwrap_or(Tier::Standard)).unwrap_or(Tier::Standard);
            self.running.decrement(tier);
            return exec.step_sessions.remove(step_id);
        }
        None
    }

    /// Resume a paused node within an execution.
    pub fn resume_node(&mut self, execution_id: &str, step_id: &str) -> bool {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            return exec.dag.resume_node(step_id);
        }
        false
    }

    /// Cancel a single node within an execution.
    /// Returns `Some(session_id)` if the node was Running (caller must kill it).
    pub fn cancel_node(&mut self, execution_id: &str, step_id: &str) -> Option<String> {
        let exec = self.executions.get_mut(execution_id)?;
        let was_running = exec.dag.cancel_node(step_id)?;
        if was_running {
            let tier = exec.dag.get(step_id).map(|n| n.tier.unwrap_or(Tier::Standard)).unwrap_or(Tier::Standard);
            self.running.decrement(tier);
            return exec.step_sessions.remove(step_id);
        }
        None
    }

    /// Check if an execution is paused.
    pub fn get_dag(&self, execution_id: &str) -> Option<&Dag> {
        self.executions.get(execution_id).map(|e| &e.dag)
    }

    pub fn is_execution_paused(&self, execution_id: &str) -> bool {
        self.executions.get(execution_id).is_some_and(|e| e.paused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::Dag;
    use std::collections::HashMap as StdMap;

    fn make_dag() -> Dag {
        Dag::from_steps(&[
            ("design".into(), "Design".into(), "Design auth".into(), Some(Tier::Heavy), vec![]),
            ("implement".into(), "Implement".into(), "Implement auth".into(), Some(Tier::Standard), vec!["design".into()]),
            ("test".into(), "Test".into(), "Test auth".into(), Some(Tier::Light), vec!["design".into()]),
            ("review".into(), "Review".into(), "Review auth".into(), Some(Tier::Standard), vec!["implement".into(), "test".into()]),
            ("merge".into(), "Merge".into(), "Merge auth".into(), Some(Tier::Light), vec!["review".into()]),
        ])
    }

    fn make_scheduler_and_dag() -> (Scheduler, Id, StdMap<String, Id>) {
        let dag = make_dag();
        let exec_id = Id::new("exec");
        let project_id = Id::new("proj");

        let step_tasks: StdMap<String, Id> = ["design", "implement", "test", "review", "merge"]
            .iter()
            .map(|s| (s.to_string(), Id::new("task")))
            .collect();

        let mut scheduler = Scheduler::new(Limits::default());
        scheduler.add_execution(exec_id.clone(), project_id, dag, step_tasks.clone());

        (scheduler, exec_id, step_tasks)
    }

    /// Simulate the full two-phase completion: worker done (tier freed) + merge landed (DAG advances).
    fn complete_step(scheduler: &mut Scheduler, exec_id: &str, step_id: &str, output: Option<String>) {
        scheduler.step_worker_done(exec_id, step_id, output.clone());
        scheduler.step_completed(exec_id, step_id, output);
    }

    #[test]
    fn initial_tick_spawns_design() {
        let (mut scheduler, _exec_id, _) = make_scheduler_and_dag();
        let actions = scheduler.tick();

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SchedulerAction::SpawnWorker {
                title,
                tier,
                upstream_outputs,
                ..
            } => {
                assert_eq!(title, "Design");
                assert_eq!(*tier, Tier::Heavy);
                assert!(upstream_outputs.is_empty());
            }
            other => panic!("expected SpawnWorker, got {other:?}"),
        }
    }

    #[test]
    fn parallel_after_design_completes() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();
        scheduler.tick(); // spawns design

        complete_step(&mut scheduler, &exec_id.0, "design", Some("design output".into()));
        let actions = scheduler.tick();

        let spawn_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawn_actions.len(), 2);

        // Both should have upstream output from design
        for action in &spawn_actions {
            if let SchedulerAction::SpawnWorker {
                upstream_outputs, ..
            } = action
            {
                assert_eq!(upstream_outputs.len(), 1);
                assert_eq!(upstream_outputs[0].0, "Design");
                assert_eq!(upstream_outputs[0].1, "design output");
            }
        }
    }

    #[test]
    fn upstream_outputs_accumulate() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();
        scheduler.tick();
        complete_step(&mut scheduler, &exec_id.0, "design", Some("design output".into()));
        scheduler.tick();
        complete_step(&mut scheduler, &exec_id.0, "implement", Some("impl output".into()));
        complete_step(&mut scheduler, &exec_id.0, "test", Some("test output".into()));

        let actions = scheduler.tick();
        let review_action = actions
            .iter()
            .find(|a| matches!(a, SchedulerAction::SpawnWorker { title, .. } if title == "Review"))
            .unwrap();

        if let SchedulerAction::SpawnWorker {
            upstream_outputs, ..
        } = review_action
        {
            // review depends on implement and test
            assert_eq!(upstream_outputs.len(), 2);
            let titles: Vec<&str> = upstream_outputs.iter().map(|(t, _)| t.as_str()).collect();
            assert!(titles.contains(&"Implement"));
            assert!(titles.contains(&"Test"));
        }
    }

    #[test]
    fn full_execution_lifecycle() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();

        // Tick 1: design
        let actions = scheduler.tick();
        assert_eq!(actions.len(), 1);

        complete_step(&mut scheduler, &exec_id.0, "design", None);

        // Tick 2: implement + test in parallel
        let actions = scheduler.tick();
        assert_eq!(actions.len(), 2);

        complete_step(&mut scheduler, &exec_id.0, "implement", None);
        complete_step(&mut scheduler, &exec_id.0, "test", None);

        // Tick 3: review
        let actions = scheduler.tick();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            SchedulerAction::SpawnWorker { title, .. } if title == "Review"
        ));

        complete_step(&mut scheduler, &exec_id.0, "review", None);

        // Tick 4: merge
        let actions = scheduler.tick();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            SchedulerAction::SpawnWorker { title, .. } if title == "Merge"
        ));

        complete_step(&mut scheduler, &exec_id.0, "merge", None);

        // Tick 5: execution complete
        let actions = scheduler.tick();
        assert!(actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::ExecutionComplete { .. })));
    }

    #[test]
    fn tier_limits_respected() {
        let limits = Limits {
            max_workers: 5,
            max_heavy: 1,
            max_standard: 1,
            max_light: 5,
        };

        let mut scheduler = Scheduler::new(limits);

        for _ in 0..2 {
            let dag = Dag::from_steps(&[
                ("work".into(), "Work".into(), "do work".into(), Some(Tier::Standard), vec![]),
            ]);
            let exec_id = Id::new("exec");
            let mut step_tasks = StdMap::new();
            step_tasks.insert("work".into(), Id::new("task"));
            scheduler.add_execution(exec_id, Id::new("proj"), dag, step_tasks);
        }

        let actions = scheduler.tick();
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawns.len(), 1);
    }

    #[test]
    fn failure_propagation_with_blocked_actions() {
        let (mut scheduler, exec_id, _step_tasks) = make_scheduler_and_dag();
        scheduler.tick(); // design

        scheduler.step_failed(&exec_id.0, "design");
        let actions = scheduler.tick();

        // No new spawns
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert!(spawns.is_empty());

        // Should have TaskBlocked for all downstream nodes
        let blocked: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::TaskBlocked { .. }))
            .collect();
        assert_eq!(blocked.len(), 4); // implement, test, review, merge

        // Execution should be marked failed
        assert!(actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::ExecutionFailed { .. })));
    }

    #[test]
    fn worker_counts_tracking() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();

        assert_eq!(scheduler.worker_counts(), (0, 0, 0, 0));

        scheduler.tick(); // design (heavy)
        assert_eq!(scheduler.worker_counts(), (1, 1, 0, 0));

        // worker_done frees the tier slot
        scheduler.step_worker_done(&exec_id.0, "design", None);
        assert_eq!(scheduler.worker_counts(), (0, 0, 0, 0));

        // step_completed (merge landed) does NOT change counts
        scheduler.step_completed(&exec_id.0, "design", None);
        assert_eq!(scheduler.worker_counts(), (0, 0, 0, 0));

        scheduler.tick(); // implement (standard) + test (light)
        assert_eq!(scheduler.worker_counts(), (2, 0, 1, 1));
    }

    #[test]
    fn find_task_lookup() {
        let (scheduler, exec_id, step_tasks) = make_scheduler_and_dag();
        let design_task_id = step_tasks.get("design").unwrap();

        let result = scheduler.find_task(design_task_id);
        assert!(result.is_some());
        let (found_exec, found_step) = result.unwrap();
        assert_eq!(found_exec, exec_id.0);
        assert_eq!(found_step, "design");
    }

    #[test]
    fn abort_all_clears_everything() {
        let (mut scheduler, exec_id, _step_tasks) = make_scheduler_and_dag();
        scheduler.tick(); // design spawned (running)

        complete_step(&mut scheduler, &exec_id.0, "design", None);
        scheduler.tick(); // implement + test spawned (running)

        assert_eq!(scheduler.worker_counts().0, 2);
        assert_eq!(scheduler.active_executions().len(), 1);

        let aborted = scheduler.abort_all();
        assert_eq!(aborted.len(), 2); // implement and test were running

        // Scheduler is fully reset
        assert_eq!(scheduler.worker_counts(), (0, 0, 0, 0));
        assert!(scheduler.active_executions().is_empty());

        // A tick produces no actions
        assert!(scheduler.tick().is_empty());
    }

    #[test]
    fn pause_execution_skips_tick() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();
        scheduler.tick(); // spawns design

        complete_step(&mut scheduler, &exec_id.0, "design", None);
        scheduler.pause_execution(&exec_id.0);

        // Tick while paused — should produce no actions.
        let actions = scheduler.tick();
        assert!(actions.is_empty());
        assert!(scheduler.is_execution_paused(&exec_id.0));

        // Resume and tick — should spawn implement + test.
        scheduler.resume_execution(&exec_id.0);
        let actions = scheduler.tick();
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawns.len(), 2);
    }

    #[test]
    fn cancel_execution_returns_running_sessions() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();
        scheduler.tick(); // spawns design
        scheduler.set_step_session(&exec_id.0, "design", "sess-1".into());

        let to_kill = scheduler.cancel_execution(&exec_id.0);
        assert_eq!(to_kill.len(), 1);
        assert_eq!(to_kill[0].1, "sess-1");

        // Execution is removed.
        assert!(scheduler.active_executions().is_empty());
        assert_eq!(scheduler.worker_counts().0, 0);
    }

    #[test]
    fn pause_resume_node() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();
        scheduler.tick(); // spawns design

        complete_step(&mut scheduler, &exec_id.0, "design", None);
        scheduler.tick(); // spawns implement + test

        // Pause implement (Running → Paused, returns session if registered)
        scheduler.set_step_session(&exec_id.0, "implement", "sess-impl".into());
        let session = scheduler.pause_node(&exec_id.0, "implement");
        assert_eq!(session, Some("sess-impl".into()));

        // Worker count decremented.
        let (_total, _, standard, _) = scheduler.worker_counts();
        assert_eq!(standard, 0);

        // Resume implement.
        assert!(scheduler.resume_node(&exec_id.0, "implement"));
    }

    #[test]
    fn cancel_node_cascades() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();
        scheduler.tick(); // spawns design

        complete_step(&mut scheduler, &exec_id.0, "design", None);
        scheduler.tick(); // spawns implement + test

        scheduler.set_step_session(&exec_id.0, "implement", "sess-impl".into());
        let session = scheduler.cancel_node(&exec_id.0, "implement");
        assert_eq!(session, Some("sess-impl".into()));

        // tick — test is still running, review/merge are cancelled by DAG cascade
        let actions = scheduler.tick();
        // No new spawns since review's deps aren't all done (implement cancelled, not done).
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert!(spawns.is_empty());
    }
}
