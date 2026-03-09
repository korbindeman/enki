use std::collections::HashMap;

use crate::dag::Dag;
use crate::types::*;

use super::Orchestrator;

impl Orchestrator {
    pub(crate) fn create_execution(&mut self, steps: Vec<super::StepDef>) -> Vec<super::Event> {
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
            return vec![super::Event::StatusMessage(format!(
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
                    if let Err(e) = self.db.insert_dependency(task_id, dep_task_id) {
                        tracing::warn!(task_id = %task_id, dep = %dep_task_id, error = %e, "failed to persist dependency");
                    }
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
                    s.role.clone(),
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

    pub(crate) fn create_task(
        &mut self,
        title: String,
        description: Option<String>,
        tier: Tier,
    ) -> Vec<super::Event> {
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
            return vec![super::Event::StatusMessage(format!(
                "failed to create task: {e}"
            ))];
        }

        let execution = Execution {
            id: exec_id.clone(),
            session_id: Some(self.session_id.clone()),
            status: ExecutionStatus::Running,
            created_at: now,
        };
        if let Err(e) = self.db.insert_execution(&execution) {
            tracing::warn!(execution_id = %exec_id, error = %e, "failed to persist execution");
            return vec![super::Event::StatusMessage(format!("failed to persist execution: {e}"))];
        }
        if let Err(e) = self.db.insert_execution_step(&exec_id, step_id, &task_id) {
            tracing::warn!(execution_id = %exec_id, error = %e, "failed to persist execution step");
            return vec![super::Event::StatusMessage(format!("failed to persist execution step: {e}"))];
        }

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

    pub(crate) fn add_steps(&mut self, execution_id: Id, steps: Vec<super::StepDef>) -> Vec<super::Event> {
        let now = chrono::Utc::now();

        // Look up existing step→task mappings for cross-dep wiring.
        let existing_steps = match self.db.get_execution_steps(&execution_id) {
            Ok(s) => s,
            Err(e) => {
                return vec![super::Event::StatusMessage(format!(
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
                    if let Err(e) = self.db.insert_dependency(task_id, dep_task_id) {
                        tracing::warn!(task_id = %task_id, dep = %dep_task_id, error = %e, "failed to persist dependency");
                    }
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
                    s.role.clone(),
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
            return vec![super::Event::StatusMessage(format!(
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

    pub(crate) fn discover_from_db(&mut self) -> Vec<super::Event> {
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
                if let Err(e) = self.db.insert_execution(&execution) {
                    tracing::warn!(execution_id = %exec_id, error = %e, "failed to persist standalone execution");
                }
                if let Err(e) = self.db.insert_execution_step(&exec_id, step_id, &task.id) {
                    tracing::warn!(execution_id = %exec_id, error = %e, "failed to persist standalone execution step");
                }

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
}
