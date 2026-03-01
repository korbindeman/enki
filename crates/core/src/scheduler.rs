use std::collections::HashMap;

use crate::dag::{Dag, NodeStatus};
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
            max_workers: 5,
            max_heavy: 1,
            max_standard: 3,
            max_light: 5,
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
    },
    /// A task completed — update downstream.
    TaskCompleted {
        task_id: Id,
        execution_id: Id,
        step_id: String,
    },
    /// A task failed.
    TaskFailed {
        task_id: Id,
        execution_id: Id,
        step_id: String,
        reason: String,
    },
    /// An execution is fully complete (all nodes done).
    ExecutionComplete {
        execution_id: Id,
    },
    /// An execution has failures.
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
            },
        );
    }

    /// One scheduling pass. Returns actions for the runtime to execute.
    pub fn tick(&mut self) -> Vec<SchedulerAction> {
        let mut actions = Vec::new();

        let exec_keys: Vec<String> = self.executions.keys().cloned().collect();

        for key in exec_keys {
            let exec = self.executions.get_mut(&key).unwrap();

            // Check for ready nodes we can assign
            let ready: Vec<String> = exec
                .dag
                .ready_nodes()
                .iter()
                .map(|s| s.to_string())
                .collect();

            for step_id in ready {
                // Extract node data before mutating the DAG
                let (title, description, tier) = {
                    let node = exec.dag.get(&step_id).unwrap();
                    (
                        node.title.clone(),
                        node.description.clone(),
                        node.tier.unwrap_or(Tier::Standard),
                    )
                };

                if !self.running.can_run(tier, &self.limits) {
                    continue;
                }

                exec.dag.mark_running(&step_id);
                self.running.increment(tier);

                let task_id = exec.step_tasks.get(&step_id).cloned().unwrap_or_else(|| {
                    Id::new("task")
                });

                actions.push(SchedulerAction::SpawnWorker {
                    task_id,
                    project_id: exec.project_id.clone(),
                    title,
                    description,
                    tier,
                    execution_id: exec.execution_id.clone(),
                    step_id: step_id.clone(),
                });
            }

            // Check for completed/failed executions
            if exec.dag.is_complete() {
                actions.push(SchedulerAction::ExecutionComplete {
                    execution_id: exec.execution_id.clone(),
                });
            } else if exec.dag.has_failures() {
                // Check if there's nothing left to run (all remaining are blocked by failures)
                let has_running = exec
                    .dag
                    .nodes()
                    .iter()
                    .any(|n| n.status == NodeStatus::Running);
                let has_ready = !exec.dag.ready_nodes().is_empty();
                if !has_running && !has_ready {
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

    /// Notify the scheduler that a step has completed.
    pub fn step_completed(&mut self, execution_id: &str, step_id: &str) {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            let tier = exec
                .dag
                .get(step_id)
                .map(|n| n.tier.unwrap_or(Tier::Standard))
                .unwrap_or(Tier::Standard);
            exec.dag.mark_done(step_id);
            exec.step_sessions.remove(step_id);
            self.running.decrement(tier);
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

    /// Record that a step is being handled by a specific ACP session.
    pub fn set_step_session(&mut self, execution_id: &str, step_id: &str, session_id: String) {
        if let Some(exec) = self.executions.get_mut(execution_id) {
            exec.step_sessions.insert(step_id.to_string(), session_id);
        }
    }

    /// Get current worker counts.
    pub fn worker_counts(&self) -> (usize, usize, usize, usize) {
        (
            self.running.total(),
            self.running.heavy,
            self.running.standard,
            self.running.light,
        )
    }

    /// Get all active execution IDs.
    pub fn active_executions(&self) -> Vec<&str> {
        self.executions.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::Dag;
    use crate::template::Template;
    use std::collections::HashMap as StdMap;

    const SHINY: &str = r#"
name = "shiny"
description = "Design before code"

[vars.feature]
description = "Feature"
required = true

[[steps]]
id = "design"
title = "Design"
description = "Design {{feature}}"
tier = "heavy"

[[steps]]
id = "implement"
title = "Implement"
description = "Implement {{feature}}"
needs = ["design"]
tier = "standard"

[[steps]]
id = "test"
title = "Test"
description = "Test {{feature}}"
needs = ["design"]
tier = "light"

[[steps]]
id = "review"
title = "Review"
description = "Review {{feature}}"
needs = ["implement", "test"]
tier = "standard"

[[steps]]
id = "merge"
title = "Merge"
description = "Merge {{feature}}"
needs = ["review"]
tier = "light"
"#;

    fn make_scheduler_and_dag() -> (Scheduler, Id, StdMap<String, Id>) {
        let template = Template::from_toml(SHINY).unwrap();
        let mut vars = StdMap::new();
        vars.insert("feature".into(), "auth".into());
        let rendered = template.render(&vars).unwrap();
        let dag = Dag::from_template(&rendered);

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

    #[test]
    fn initial_tick_spawns_design() {
        let (mut scheduler, _exec_id, _) = make_scheduler_and_dag();
        let actions = scheduler.tick();

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SchedulerAction::SpawnWorker { title, tier, .. } => {
                assert_eq!(title, "Design");
                assert_eq!(*tier, Tier::Heavy);
            }
            other => panic!("expected SpawnWorker, got {other:?}"),
        }
    }

    #[test]
    fn parallel_after_design_completes() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();
        scheduler.tick(); // spawns design

        scheduler.step_completed(&exec_id.0, "design");
        let actions = scheduler.tick();

        // implement (standard) and test (light) should both spawn
        let spawn_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawn_actions.len(), 2);

        let titles: Vec<&str> = spawn_actions
            .iter()
            .map(|a| match a {
                SchedulerAction::SpawnWorker { title, .. } => title.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert!(titles.contains(&"Implement"));
        assert!(titles.contains(&"Test"));
    }

    #[test]
    fn full_execution_lifecycle() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();

        // Tick 1: design
        let actions = scheduler.tick();
        assert_eq!(actions.len(), 1);

        scheduler.step_completed(&exec_id.0, "design");

        // Tick 2: implement + test in parallel
        let actions = scheduler.tick();
        assert_eq!(actions.len(), 2);

        scheduler.step_completed(&exec_id.0, "implement");
        scheduler.step_completed(&exec_id.0, "test");

        // Tick 3: review
        let actions = scheduler.tick();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], SchedulerAction::SpawnWorker { title, .. } if title == "Review"));

        scheduler.step_completed(&exec_id.0, "review");

        // Tick 4: merge
        let actions = scheduler.tick();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], SchedulerAction::SpawnWorker { title, .. } if title == "Merge"));

        scheduler.step_completed(&exec_id.0, "merge");

        // Tick 5: execution complete
        let actions = scheduler.tick();
        assert!(actions.iter().any(|a| matches!(a, SchedulerAction::ExecutionComplete { .. })));
    }

    #[test]
    fn tier_limits_respected() {
        let limits = Limits {
            max_workers: 5,
            max_heavy: 1,
            max_standard: 1,
            max_light: 5,
        };

        // Two executions both needing standard-tier work
        let template = Template::from_toml(
            r#"
name = "simple"
description = "one step"

[[steps]]
id = "work"
title = "Work"
description = "do work"
tier = "standard"
"#,
        )
        .unwrap();

        let mut scheduler = Scheduler::new(limits);

        // Add two executions
        for _ in 0..2 {
            let dag = Dag::from_template(&template);
            let exec_id = Id::new("exec");
            let mut step_tasks = StdMap::new();
            step_tasks.insert("work".into(), Id::new("task"));
            scheduler.add_execution(exec_id, Id::new("proj"), dag, step_tasks);
        }

        let actions = scheduler.tick();
        // Only 1 should spawn (max_standard = 1)
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert_eq!(spawns.len(), 1);
    }

    #[test]
    fn failure_propagation() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();
        scheduler.tick(); // design

        scheduler.step_failed(&exec_id.0, "design");
        let actions = scheduler.tick();

        // No new spawns (implement and test depend on design)
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::SpawnWorker { .. }))
            .collect();
        assert!(spawns.is_empty());

        // Execution should be marked failed
        assert!(actions.iter().any(|a| matches!(a, SchedulerAction::ExecutionFailed { .. })));
    }

    #[test]
    fn worker_counts_tracking() {
        let (mut scheduler, exec_id, _) = make_scheduler_and_dag();

        assert_eq!(scheduler.worker_counts(), (0, 0, 0, 0));

        scheduler.tick(); // design (heavy)
        assert_eq!(scheduler.worker_counts(), (1, 1, 0, 0));

        scheduler.step_completed(&exec_id.0, "design");
        assert_eq!(scheduler.worker_counts(), (0, 0, 0, 0));

        scheduler.tick(); // implement (standard) + test (light)
        assert_eq!(scheduler.worker_counts(), (2, 0, 1, 1));
    }
}
