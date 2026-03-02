use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Utc;
use clap::Subcommand;
use enki_core::template::Template;
use enki_core::types::{Execution, ExecutionStatus, Id, Task, TaskStatus};

use super::open_db;

#[derive(Subcommand)]
pub enum ExecCmd {
    /// Run a workflow template, creating all tasks and wiring up dependencies.
    Run {
        /// Project ID to run the template against.
        project_id: String,
        /// Path to the TOML template file.
        template_path: PathBuf,
        /// Template variables as key=value pairs (e.g. --var feature=auth).
        #[arg(long = "var", value_name = "KEY=VALUE")]
        vars: Vec<String>,
    },
}

pub async fn exec(cmd: ExecCmd) -> anyhow::Result<()> {
    match cmd {
        ExecCmd::Run {
            project_id,
            template_path,
            vars,
        } => run_template(project_id, template_path, vars).await,
    }
}

async fn run_template(
    project_id: String,
    template_path: PathBuf,
    raw_vars: Vec<String>,
) -> anyhow::Result<()> {
    let db = open_db()?;
    let project_id = Id(project_id);

    // Verify project exists
    let project = db.get_project(&project_id)?;

    // Parse template
    let content = std::fs::read_to_string(&template_path)
        .map_err(|e| anyhow::anyhow!("failed to read template '{}': {e}", template_path.display()))?;

    let template = Template::from_toml(&content)
        .map_err(|e| anyhow::anyhow!("invalid template: {e}"))?;

    // Parse --var key=value args
    let mut vars: HashMap<String, String> = HashMap::new();
    for raw in &raw_vars {
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid --var '{}': expected key=value", raw))?;
        vars.insert(key.to_string(), value.to_string());
    }

    // Render variable substitutions
    let rendered = template
        .render(&vars)
        .map_err(|e| anyhow::anyhow!("template render failed: {e}"))?;

    // Create execution record
    let execution_id = Id::new("exec");
    let execution = Execution {
        id: execution_id.clone(),
        template: template_path.to_string_lossy().to_string(),
        group_id: None,
        status: ExecutionStatus::Running,
        vars: if vars.is_empty() {
            None
        } else {
            Some(serde_json::to_value(&vars)?)
        },
        created_at: Utc::now(),
    };
    db.insert_execution(&execution)?;

    // Create a task for each step, tracking step_id -> task_id
    let mut step_tasks: HashMap<String, Id> = HashMap::new();
    let now = Utc::now();

    for step in &rendered.steps {
        let task_id = Id::new("task");
        let task = Task {
            id: task_id.clone(),
            project_id: project_id.clone(),
            title: step.title.clone(),
            description: Some(step.description.clone()),
            // Leaf tasks (no dependencies) start ready; others start open.
            status: if step.needs.is_empty() {
                TaskStatus::Ready
            } else {
                TaskStatus::Open
            },
            assigned_to: None,
            worktree: None,
            branch: None,
            tier: step.tier,
            created_at: now,
            updated_at: now,
        };
        db.insert_task(&task)?;
        db.insert_execution_step(&execution_id, &step.id, &task_id)?;
        step_tasks.insert(step.id.clone(), task_id);
    }

    // Wire up task dependencies in the DB
    for step in &rendered.steps {
        let task_id = &step_tasks[&step.id];
        for dep_step_id in &step.needs {
            let dep_task_id = &step_tasks[dep_step_id];
            db.insert_dependency(task_id, dep_task_id)?;
        }
    }

    // Print summary
    println!(
        "execution {} started for project '{}' ({})",
        execution_id, project.name, project_id
    );
    println!("template: {} — {}", rendered.name, rendered.description);
    println!();
    println!("{} tasks created:", rendered.steps.len());
    for step in &rendered.steps {
        let task_id = &step_tasks[&step.id];
        let status = if step.needs.is_empty() { "ready" } else { "open" };
        let tier = step.tier.map(|t| t.as_str()).unwrap_or("standard");
        let deps = if step.needs.is_empty() {
            String::new()
        } else {
            format!(" (needs: {})", step.needs.join(", "))
        };
        println!(
            "  {} | {} | {} | {}{}",
            task_id, status, tier, step.title, deps
        );
    }
    println!();
    println!("Tasks marked ready will be picked up by the coordinator automatically.");

    Ok(())
}
