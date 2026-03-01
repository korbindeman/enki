use chrono::Utc;
use clap::Subcommand;
use enki_core::types::{Id, Task, TaskStatus, Tier};

use super::open_db;

#[derive(Subcommand)]
pub enum TaskCmd {
    /// Create a new task.
    Create {
        /// Project ID.
        project: String,
        /// Task title.
        title: String,
        /// Task description.
        #[arg(long)]
        description: Option<String>,
        /// Complexity tier: light, standard, heavy.
        #[arg(long, default_value = "standard")]
        tier: String,
    },
    /// List tasks for a project.
    List {
        /// Project ID.
        project: String,
    },
}

pub async fn task(cmd: TaskCmd) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::Create {
            project,
            title,
            description,
            tier,
        } => create_task(project, title, description, tier).await,
        TaskCmd::List { project } => list_tasks(project).await,
    }
}

async fn create_task(
    project_id: String,
    title: String,
    description: Option<String>,
    tier_str: String,
) -> anyhow::Result<()> {
    let tier = Tier::from_str(&tier_str)
        .ok_or_else(|| anyhow::anyhow!("invalid tier: {tier_str}. Use light, standard, or heavy"))?;

    let db = open_db()?;
    let project_id = Id(project_id);

    // Verify project exists
    db.get_project(&project_id)?;

    let now = Utc::now();
    let task = Task {
        id: Id::new("task"),
        project_id,
        title: title.clone(),
        description,
        status: TaskStatus::Open,
        assigned_to: None,
        worktree: None,
        branch: None,
        tier: Some(tier),
        created_at: now,
        updated_at: now,
    };

    db.insert_task(&task)?;
    println!("created task '{}' ({})", title, task.id);
    Ok(())
}

async fn list_tasks(project_id: String) -> anyhow::Result<()> {
    let db = open_db()?;
    let project_id = Id(project_id);
    let tasks = db.list_tasks(&project_id)?;

    if tasks.is_empty() {
        println!("no tasks for this project.");
        return Ok(());
    }

    for t in &tasks {
        let tier = t.tier.map(|t| t.as_str()).unwrap_or("-");
        let status = t.status.as_str();
        println!(
            "{} | {} | {} | {}",
            t.id, status, tier, t.title
        );
    }
    Ok(())
}
