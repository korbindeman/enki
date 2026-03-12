use std::collections::{HashMap, HashSet};

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
        role: Option<String>,
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

/// Tracks a single global task DAG and decides what to run next.
/// Executions are soft grouping labels over tasks in the DAG.
pub struct Scheduler {
    limits: Limits,
    /// Single global DAG. Nodes are keyed by task_id.
    dag: Dag,
    /// Execution groups: execution_id → group state.
    groups: HashMap<String, GroupState>,
    /// task_id → execution_id (reverse lookup).
    task_groups: HashMap<String, String>,
    /// task_id → step_id (display metadata).
    task_steps: HashMap<String, String>,
    /// task_id → ACP session_id for running tasks.
    task_sessions: HashMap<String, String>,
    /// Captured outputs from completed tasks, keyed by task_id.
    task_outputs: HashMap<String, String>,
    /// Task IDs already reported as blocked (dedup across ticks).
    reported_blocked: HashSet<String>,
    /// How many workers are currently running per tier.
    running: TierCounts,
}

#[derive(Debug, Clone)]
struct GroupState {
    execution_id: Id,
    project_id: Id,
    task_ids: HashSet<String>,
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
            dag: Dag::from_steps(&[]),
            groups: HashMap::new(),
            task_groups: HashMap::new(),
            task_steps: HashMap::new(),
            task_sessions: HashMap::new(),
            task_outputs: HashMap::new(),
            reported_blocked: HashSet::new(),
            running: TierCounts::default(),
        }
    }

    /// Check whether an execution group is already tracked.
    pub fn has_execution(&self, execution_id: &str) -> bool {
        self.groups.contains_key(execution_id)
    }

    /// Register a new execution by absorbing its DAG into the global graph.
    /// The incoming `dag` uses step_ids as node IDs. `step_tasks` maps
    /// step_id → task_id. Nodes are re-keyed to task_ids in the global DAG.
    pub fn add_execution(
        &mut self,
        execution_id: Id,
        project_id: Id,
        dag: Dag,
        step_tasks: HashMap<String, Id>,
    ) {
        // Build step_id → task_id string map for absorb.
        let id_map: HashMap<String, String> = step_tasks
            .iter()
            .map(|(step_id, task_id)| (step_id.clone(), task_id.0.clone()))
            .collect();

        if let Err(e) = self.dag.absorb(&dag, &id_map) {
            tracing::warn!(execution_id = %execution_id, error = %e, "failed to absorb DAG");
            return;
        }

        // Register group.
        let mut task_id_set = HashSet::new();
        for (step_id, task_id) in &step_tasks {
            task_id_set.insert(task_id.0.clone());
            self.task_groups
                .insert(task_id.0.clone(), execution_id.0.clone());
            self.task_steps
                .insert(task_id.0.clone(), step_id.clone());
        }

        self.groups.insert(
            execution_id.0.clone(),
            GroupState {
                execution_id,
                project_id,
                task_ids: task_id_set,
                paused: false,
            },
        );
    }

    /// Resolve a (execution_id, step_id) pair to a task_id in the global DAG.
    fn resolve_task_id(&self, execution_id: &str, step_id: &str) -> Option<&str> {
        let group = self.groups.get(execution_id)?;
        for tid in &group.task_ids {
            if self.task_steps.get(tid).is_some_and(|s| s == step_id) {
                return Some(tid);
            }
        }
        None
    }

    /// One scheduling pass. Returns actions for the runtime to execute.
    pub fn tick(&mut self) -> Vec<SchedulerAction> {
        let mut actions = Vec::new();

        // Emit TaskBlocked for newly blocked nodes.
        let blocked: Vec<String> = self
            .dag
            .blocked_nodes()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for task_id in blocked {
            if !self.reported_blocked.insert(task_id.clone()) {
                continue;
            }
            if let Some(exec_id) = self.task_groups.get(&task_id) {
                let step_id = self
                    .task_steps
                    .get(&task_id)
                    .cloned()
                    .unwrap_or_default();
                if let Some(group) = self.groups.get(exec_id) {
                    actions.push(SchedulerAction::TaskBlocked {
                        task_id: Id(task_id),
                        execution_id: group.execution_id.clone(),
                        step_id,
                    });
                }
            }
        }

        // Check for ready nodes we can assign.
        let ready: Vec<String> = self
            .dag
            .ready_nodes()
            .iter()
            .map(|s| s.to_string())
            .collect();

        for task_id in ready {
            // Skip if the task's group is paused.
            if let Some(exec_id) = self.task_groups.get(&task_id) {
                if self.groups.get(exec_id).is_some_and(|g| g.paused) {
                    continue;
                }
            }

            let (title, description, tier, role, upstream_outputs) = {
                let node = self.dag.get(&task_id).unwrap();
                let tier = node.tier.unwrap_or(Tier::Standard);

                // Collect outputs from all completed upstream deps.
                let upstream: Vec<(String, String)> = node
                    .deps
                    .iter()
                    .filter_map(|edge| {
                        let dep = &self.dag.nodes()[edge.target];
                        self.task_outputs
                            .get(&dep.id)
                            .map(|out| (dep.title.clone(), out.clone()))
                    })
                    .collect();

                (
                    node.title.clone(),
                    node.description.clone(),
                    tier,
                    node.role.clone(),
                    upstream,
                )
            };

            if !self.running.can_run(tier, &self.limits) {
                continue;
            }

            self.dag.mark_running(&task_id);
            self.running.increment(tier);

            let exec_id = self
                .task_groups
                .get(&task_id)
                .cloned()
                .unwrap_or_default();
            let step_id = self
                .task_steps
                .get(&task_id)
                .cloned()
                .unwrap_or_default();
            let project_id = self
                .groups
                .get(&exec_id)
                .map(|g| g.project_id.clone())
                .unwrap_or_else(|| Id("project".into()));

            actions.push(SchedulerAction::SpawnWorker {
                task_id: Id(task_id),
                project_id,
                title,
                description,
                tier,
                execution_id: Id(exec_id),
                step_id,
                upstream_outputs,
                role,
            });
        }

        // Check group completion/failure.
        let group_keys: Vec<String> = self.groups.keys().cloned().collect();
        let mut completed_groups = Vec::new();

        for gk in group_keys {
            let group = self.groups.get(&gk).unwrap();
            if group.paused {
                continue;
            }

            let all_terminal = group.task_ids.iter().all(|tid| {
                self.dag
                    .get(tid)
                    .is_some_and(|n| matches!(n.status, NodeStatus::Done | NodeStatus::Cancelled))
            });

            if all_terminal {
                actions.push(SchedulerAction::ExecutionComplete {
                    execution_id: group.execution_id.clone(),
                });
                completed_groups.push(gk);
                continue;
            }

            let has_failure = group.task_ids.iter().any(|tid| {
                self.dag
                    .get(tid)
                    .is_some_and(|n| n.status == NodeStatus::Failed)
            });
            if has_failure {
                let has_in_progress = group.task_ids.iter().any(|tid| {
                    self.dag.get(tid).is_some_and(|n| {
                        matches!(n.status, NodeStatus::Running | NodeStatus::WorkerDone)
                    })
                });
                let has_ready = group.task_ids.iter().any(|tid| {
                    self.dag
                        .get(tid)
                        .is_some_and(|n| n.status == NodeStatus::Ready)
                });
                if !has_in_progress && !has_ready {
                    actions.push(SchedulerAction::ExecutionFailed {
                        execution_id: group.execution_id.clone(),
                    });
                    completed_groups.push(gk);
                }
            }
        }

        // Clean up completed/failed groups.
        // Nodes stay in the global DAG (other groups may depend on them).
        for gk in completed_groups {
            self.groups.remove(&gk);
        }

        actions
    }

    /// Revert a SpawnWorker action that the runtime couldn't execute.
    pub fn revert_spawn(&mut self, execution_id: &str, step_id: &str) {
        let Some(task_id) = self.resolve_task_id(execution_id, step_id) else {
            return;
        };
        let task_id = task_id.to_string();
        if self.dag.revert_running(&task_id) {
            let tier = self
                .dag
                .get(&task_id)
                .map(|n| n.tier.unwrap_or(Tier::Standard))
                .unwrap_or(Tier::Standard);
            self.running.decrement(tier);
        }
    }

    /// Notify the scheduler that a worker has finished (but merge hasn't landed yet).
    /// Decrements the tier count, stores output, transitions node to WorkerDone.
    pub fn step_worker_done(
        &mut self,
        execution_id: &str,
        step_id: &str,
        output: Option<String>,
    ) {
        let Some(task_id) = self.resolve_task_id(execution_id, step_id) else {
            return;
        };
        let task_id = task_id.to_string();
        let tier = self
            .dag
            .get(&task_id)
            .map(|n| n.tier.unwrap_or(Tier::Standard))
            .unwrap_or(Tier::Standard);
        self.dag.mark_worker_done(&task_id);
        self.task_sessions.remove(&task_id);
        if let Some(out) = output {
            self.task_outputs.insert(task_id, out);
        }
        self.running.decrement(tier);
    }

    /// Notify the scheduler that a merge has landed (step fully complete).
    /// Transitions node from WorkerDone to Done. Does NOT decrement tier count.
    pub fn step_completed(
        &mut self,
        execution_id: &str,
        step_id: &str,
        output: Option<String>,
    ) {
        let Some(task_id) = self.resolve_task_id(execution_id, step_id) else {
            return;
        };
        let task_id = task_id.to_string();
        self.dag.mark_done(&task_id);
        if let Some(out) = output {
            self.task_outputs.insert(task_id, out);
        }
    }

    /// Notify the scheduler that a step has failed.
    pub fn step_failed(&mut self, execution_id: &str, step_id: &str) {
        let Some(task_id) = self.resolve_task_id(execution_id, step_id) else {
            return;
        };
        let task_id = task_id.to_string();
        let tier = self
            .dag
            .get(&task_id)
            .map(|n| n.tier.unwrap_or(Tier::Standard))
            .unwrap_or(Tier::Standard);
        self.dag.mark_failed(&task_id);
        self.task_sessions.remove(&task_id);
        self.running.decrement(tier);
    }

    /// Add new steps to a running execution's DAG.
    /// Steps use step_ids; `new_step_tasks` maps step_id → task_id.
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
            Option<String>,
            Vec<(String, EdgeCondition)>,
        )],
        new_step_tasks: HashMap<String, Id>,
    ) -> std::result::Result<(), String> {
        let group = self
            .groups
            .get_mut(execution_id)
            .ok_or_else(|| format!("execution not found: {}", execution_id))?;

        // Build a combined step_id → task_id map (existing + new).
        let mut all_step_to_task: HashMap<&str, &str> = HashMap::new();
        for tid in &group.task_ids {
            if let Some(sid) = self.task_steps.get(tid) {
                all_step_to_task.insert(sid, tid);
            }
        }
        for (sid, tid) in &new_step_tasks {
            all_step_to_task.insert(sid, &tid.0);
        }

        // Remap step data to use task_ids.
        let remapped: Vec<_> = steps
            .iter()
            .map(|(id, title, desc, tier, checkpoint, role, deps)| {
                let task_id = all_step_to_task
                    .get(id.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| id.clone());
                let remapped_deps: Vec<(String, EdgeCondition)> = deps
                    .iter()
                    .map(|(dep_id, cond)| {
                        let dep_task_id = all_step_to_task
                            .get(dep_id.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| dep_id.clone());
                        (dep_task_id, *cond)
                    })
                    .collect();
                (
                    task_id,
                    title.clone(),
                    desc.clone(),
                    *tier,
                    *checkpoint,
                    role.clone(),
                    remapped_deps,
                )
            })
            .collect();

        self.dag.add_steps(&remapped)?;

        // Register new tasks in the group.
        for (step_id, task_id) in &new_step_tasks {
            group.task_ids.insert(task_id.0.clone());
            self.task_groups
                .insert(task_id.0.clone(), execution_id.to_string());
            self.task_steps
                .insert(task_id.0.clone(), step_id.clone());
        }

        Ok(())
    }

    /// Check if a step is a checkpoint node.
    pub fn is_checkpoint(&self, execution_id: &str, step_id: &str) -> bool {
        self.resolve_task_id(execution_id, step_id)
            .and_then(|tid| self.dag.get(tid))
            .is_some_and(|node| node.checkpoint)
    }

    /// Record that a step is being handled by a specific ACP session.
    pub fn set_step_session(&mut self, execution_id: &str, step_id: &str, session_id: String) {
        if let Some(task_id) = self.resolve_task_id(execution_id, step_id) {
            self.task_sessions.insert(task_id.to_string(), session_id);
        }
    }

    /// Look up which execution and step a task belongs to.
    pub fn find_task(&self, task_id: &Id) -> Option<(&str, &str)> {
        let exec_id = self.task_groups.get(&task_id.0)?;
        let step_id = self.task_steps.get(&task_id.0)?;
        Some((exec_id, step_id))
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

    /// Abort all active groups immediately.
    /// Returns a list of (execution_id, step_id, task_id) for every running task.
    pub fn abort_all(&mut self) -> Vec<(String, String, Id)> {
        let mut aborted = Vec::new();
        for node in self.dag.nodes() {
            if node.status == NodeStatus::Running {
                let exec_id = self
                    .task_groups
                    .get(&node.id)
                    .cloned()
                    .unwrap_or_default();
                let step_id = self
                    .task_steps
                    .get(&node.id)
                    .cloned()
                    .unwrap_or_default();
                aborted.push((exec_id, step_id, Id(node.id.clone())));
            }
        }
        self.dag = Dag::from_steps(&[]);
        self.groups.clear();
        self.task_groups.clear();
        self.task_steps.clear();
        self.task_sessions.clear();
        self.task_outputs.clear();
        self.reported_blocked.clear();
        self.running = TierCounts::default();
        aborted
    }

    /// Get all (execution_id, step_id, task_id) triples where the task is Running.
    pub fn running_steps(&self) -> Vec<(String, String, Id)> {
        let mut result = Vec::new();
        for node in self.dag.nodes() {
            if node.status == NodeStatus::Running {
                let exec_id = self
                    .task_groups
                    .get(&node.id)
                    .cloned()
                    .unwrap_or_default();
                let step_id = self
                    .task_steps
                    .get(&node.id)
                    .cloned()
                    .unwrap_or_default();
                result.push((exec_id, step_id, Id(node.id.clone())));
            }
        }
        result
    }

    /// Get all active execution IDs.
    pub fn active_executions(&self) -> Vec<&str> {
        self.groups.keys().map(|s| s.as_str()).collect()
    }

    /// Pause an entire execution group. `tick()` will skip its tasks.
    pub fn pause_execution(&mut self, execution_id: &str) -> bool {
        if let Some(group) = self.groups.get_mut(execution_id) {
            group.paused = true;
            return true;
        }
        false
    }

    /// Resume a paused execution group.
    pub fn resume_execution(&mut self, execution_id: &str) -> bool {
        if let Some(group) = self.groups.get_mut(execution_id) {
            group.paused = false;
            return true;
        }
        false
    }

    /// Cancel an entire execution group. Returns `(task_id, session_id)` pairs
    /// for running tasks that need to be killed.
    pub fn cancel_execution(&mut self, execution_id: &str) -> Vec<(Id, String)> {
        let mut to_kill = Vec::new();
        if let Some(group) = self.groups.remove(execution_id) {
            for tid in &group.task_ids {
                if let Some(node) = self.dag.get(tid) {
                    if node.status == NodeStatus::Running {
                        let tier = node.tier.unwrap_or(Tier::Standard);
                        self.running.decrement(tier);
                        if let Some(session_id) = self.task_sessions.get(tid) {
                            to_kill.push((Id(tid.clone()), session_id.clone()));
                        }
                    }
                }
                // Cancel the node and cascade to dependents (even outside group).
                self.dag.cancel_node(tid);
            }
        }
        to_kill
    }

    /// Pause a single node.
    /// Returns `Some(session_id)` if the node was Running (caller must kill it).
    pub fn pause_node(&mut self, execution_id: &str, step_id: &str) -> Option<String> {
        let task_id = self.resolve_task_id(execution_id, step_id)?.to_string();
        let was_running = self.dag.pause_node(&task_id)?;
        if was_running {
            let tier = self
                .dag
                .get(&task_id)
                .map(|n| n.tier.unwrap_or(Tier::Standard))
                .unwrap_or(Tier::Standard);
            self.running.decrement(tier);
            return self.task_sessions.remove(&task_id);
        }
        None
    }

    /// Retry a failed/blocked node.
    pub fn retry_node(&mut self, execution_id: &str, step_id: &str) -> bool {
        let Some(task_id) = self.resolve_task_id(execution_id, step_id) else {
            return false;
        };
        let task_id = task_id.to_string();
        if self.dag.retry_node(&task_id) {
            // Clear reported_blocked for any nodes that were just unblocked.
            let unblocked: Vec<String> = self
                .dag
                .nodes()
                .iter()
                .filter(|n| matches!(n.status, NodeStatus::Pending | NodeStatus::Ready))
                .map(|n| n.id.clone())
                .collect();
            for id in unblocked {
                self.reported_blocked.remove(&id);
            }
            // Re-register the group if it was cleaned up on ExecutionFailed.
            if !self.groups.contains_key(execution_id) {
                let task_ids: HashSet<String> = self
                    .task_groups
                    .iter()
                    .filter(|(_, eid)| *eid == execution_id)
                    .map(|(tid, _)| tid.clone())
                    .collect();
                if !task_ids.is_empty() {
                    self.groups.insert(
                        execution_id.to_string(),
                        GroupState {
                            execution_id: Id(execution_id.to_string()),
                            project_id: Id("project".into()),
                            task_ids,
                            paused: false,
                        },
                    );
                }
            }
            return true;
        }
        false
    }

    /// Resume a paused node.
    pub fn resume_node(&mut self, execution_id: &str, step_id: &str) -> bool {
        let Some(task_id) = self.resolve_task_id(execution_id, step_id).map(|s| s.to_string()) else {
            return false;
        };
        self.dag.resume_node(&task_id)
    }

    /// Cancel a single node.
    /// Returns `Some(session_id)` if the node was Running (caller must kill it).
    pub fn cancel_node(&mut self, execution_id: &str, step_id: &str) -> Option<String> {
        let task_id = self.resolve_task_id(execution_id, step_id)?.to_string();
        let was_running = self.dag.cancel_node(&task_id)?;
        if was_running {
            let tier = self
                .dag
                .get(&task_id)
                .map(|n| n.tier.unwrap_or(Tier::Standard))
                .unwrap_or(Tier::Standard);
            self.running.decrement(tier);
            return self.task_sessions.remove(&task_id);
        }
        None
    }

    /// Get the global DAG (if the execution group exists).
    pub fn get_dag(&self, execution_id: &str) -> Option<&Dag> {
        if self.groups.contains_key(execution_id) {
            Some(&self.dag)
        } else {
            None
        }
    }

    /// Get a reference to the global DAG (unconditional).
    pub fn global_dag(&self) -> &Dag {
        &self.dag
    }

    pub fn is_execution_paused(&self, execution_id: &str) -> bool {
        self.groups
            .get(execution_id)
            .is_some_and(|g| g.paused)
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

    /// Simulate the full two-phase completion: worker done + merge landed.
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
        // No new spawns since review's deps aren't all done.
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert!(spawns.is_empty());
    }

    // --- New cross-group dependency tests ---

    #[test]
    fn cross_group_dependency() {
        let mut scheduler = Scheduler::new(Limits::default());

        // Group A: a single task "setup"
        let dag_a = Dag::from_steps(&[
            ("setup".into(), "Setup".into(), "Initialize".into(), Some(Tier::Light), vec![]),
        ]);
        let exec_a = Id::new("exec");
        let task_setup = Id::new("task");
        let mut st_a = StdMap::new();
        st_a.insert("setup".into(), task_setup.clone());
        scheduler.add_execution(exec_a.clone(), Id::new("proj"), dag_a, st_a);

        // Group B: "build" depends on "setup" from group A.
        // Build a DAG where "build" depends on "setup" (the step_id).
        // But "setup" won't be in group B's DAG — we need to add it via add_steps
        // using task_ids that reference the global DAG.
        let dag_b = Dag::from_steps(&[
            ("build".into(), "Build".into(), "Build it".into(), Some(Tier::Standard), vec![]),
        ]);
        let exec_b = Id::new("exec");
        let task_build = Id::new("task");
        let mut st_b = StdMap::new();
        st_b.insert("build".into(), task_build.clone());
        scheduler.add_execution(exec_b.clone(), Id::new("proj"), dag_b, st_b);

        // Tick: both setup and build should be ready (build has no deps yet in DAG).
        let actions = scheduler.tick();
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawns.len(), 2);
    }

    #[test]
    fn standalone_task() {
        let mut scheduler = Scheduler::new(Limits::default());

        let dag = Dag::single("task", "Standalone", "Do something", Some(Tier::Standard));
        let task_id = Id::new("task");
        let mut st = StdMap::new();
        st.insert("task".into(), task_id.clone());
        let exec_id = Id::new("exec");
        scheduler.add_execution(exec_id.clone(), Id::new("proj"), dag, st);

        let actions = scheduler.tick();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], SchedulerAction::SpawnWorker { title, .. } if title == "Standalone"));

        complete_step(&mut scheduler, &exec_id.0, "task", None);
        let actions = scheduler.tick();
        assert!(actions.iter().any(|a| matches!(a, SchedulerAction::ExecutionComplete { .. })));
    }

    #[test]
    fn group_completion_independent_of_external_dependents() {
        let mut scheduler = Scheduler::new(Limits::default());

        // Group A with one task
        let dag_a = Dag::from_steps(&[
            ("a".into(), "A".into(), "task a".into(), Some(Tier::Light), vec![]),
        ]);
        let exec_a = Id::new("exec");
        let task_a = Id::new("task");
        let mut st_a = StdMap::new();
        st_a.insert("a".into(), task_a.clone());
        scheduler.add_execution(exec_a.clone(), Id::new("proj"), dag_a, st_a);

        // Complete A
        scheduler.tick();
        complete_step(&mut scheduler, &exec_a.0, "a", None);
        let actions = scheduler.tick();

        // Group A should complete even if other groups depend on its tasks
        assert!(actions.iter().any(|a| matches!(a, SchedulerAction::ExecutionComplete { .. })));
    }

    #[test]
    fn pause_group_does_not_affect_other_groups() {
        let mut scheduler = Scheduler::new(Limits::default());

        // Group A
        let dag_a = Dag::from_steps(&[
            ("a".into(), "A".into(), "task a".into(), Some(Tier::Light), vec![]),
        ]);
        let exec_a = Id::new("exec");
        let task_a = Id::new("task");
        let mut st_a = StdMap::new();
        st_a.insert("a".into(), task_a.clone());
        scheduler.add_execution(exec_a.clone(), Id::new("proj"), dag_a, st_a);

        // Group B
        let dag_b = Dag::from_steps(&[
            ("b".into(), "B".into(), "task b".into(), Some(Tier::Light), vec![]),
        ]);
        let exec_b = Id::new("exec");
        let task_b = Id::new("task");
        let mut st_b = StdMap::new();
        st_b.insert("b".into(), task_b.clone());
        scheduler.add_execution(exec_b.clone(), Id::new("proj"), dag_b, st_b);

        // Pause group A
        scheduler.pause_execution(&exec_a.0);

        // Tick: only B should spawn
        let actions = scheduler.tick();
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawns.len(), 1);
        if let SchedulerAction::SpawnWorker { title, .. } = &spawns[0] {
            assert_eq!(title, "B");
        }
    }
}
