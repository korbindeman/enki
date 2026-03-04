use std::path::PathBuf;
use std::process::Command;

use chrono::Utc;
use clap::Subcommand;
use enki_core::types::{Id, Task, TaskStatus, Tier};
use enki_core::worktree::CopyManager;

use super::open_db;

#[derive(Subcommand)]
pub enum TaskCmd {
    /// Create a new task.
    Create {
        /// Task title.
        title: String,
        /// Task description.
        #[arg(long)]
        description: Option<String>,
        /// Complexity tier: light, standard, heavy.
        #[arg(long, default_value = "standard")]
        tier: String,
    },
    /// List all tasks.
    List,
    /// Update a task's status.
    UpdateStatus {
        /// Task ID.
        task_id: String,
        /// New status: open, ready, running, done, failed, blocked.
        status: String,
    },
    /// Retry a blocked task after a merge conflict.
    ///
    /// Re-fetches the task's branch from its copy and rebases onto main.
    Retry {
        /// Task ID.
        task_id: String,
    },
}

pub async fn task(cmd: TaskCmd) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::Create {
            title,
            description,
            tier,
        } => create_task(title, description, tier).await,
        TaskCmd::List => list_tasks().await,
        TaskCmd::UpdateStatus { task_id, status } => update_status(task_id, status).await,
        TaskCmd::Retry { task_id } => retry_task(task_id).await,
    }
}

async fn create_task(
    title: String,
    description: Option<String>,
    tier_str: String,
) -> anyhow::Result<()> {
    let tier = Tier::from_str(&tier_str)
        .ok_or_else(|| anyhow::anyhow!("invalid tier: {tier_str}. Use light, standard, or heavy"))?;

    let db = open_db()?;

    let now = Utc::now();
    let task = Task {
        id: Id::new("task"),
        title: title.clone(),
        description,
        status: TaskStatus::Ready,
        assigned_to: None,
        worktree: None,
        branch: None,
        tier: Some(tier),
        current_activity: None,
        created_at: now,
        updated_at: now,
    };

    db.insert_task(&task)?;
    println!("created task '{}' ({})", title, task.id);
    Ok(())
}

async fn update_status(task_id: String, status_str: String) -> anyhow::Result<()> {
    let status = TaskStatus::from_str(&status_str)
        .ok_or_else(|| anyhow::anyhow!("invalid status: {status_str}. Use open, ready, running, done, failed, or blocked"))?;

    let db = open_db()?;
    let task_id = Id(task_id);
    let task = db.get_task(&task_id)?;
    let old_status = task.status.as_str();
    db.update_task_status(&task_id, status)?;
    println!("task {} status: {} → {}", task_id, old_status, status.as_str());
    Ok(())
}

async fn list_tasks() -> anyhow::Result<()> {
    let db = open_db()?;
    let tasks = db.list_tasks()?;

    if tasks.is_empty() {
        println!("no tasks.");
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

async fn retry_task(task_id_str: String) -> anyhow::Result<()> {
    let db = open_db()?;
    let task_id = Id(task_id_str);
    let task = db.get_task(&task_id)?;

    if task.status != TaskStatus::Blocked {
        anyhow::bail!(
            "task {} is '{}', expected 'blocked'. Only conflicted tasks can be retried.",
            task_id,
            task.status.as_str()
        );
    }

    let copy_str = task
        .worktree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("task {} has no copy path recorded", task_id))?;
    let branch = task
        .branch
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("task {} has no branch recorded", task_id))?
        .to_string();

    let copy_path = PathBuf::from(copy_str);
    if !copy_path.exists() {
        anyhow::bail!(
            "copy no longer exists at '{}'. Cannot retry.",
            copy_path.display()
        );
    }

    let project_root = super::project_root()?;
    let copies_dir = super::copies_dir()?;
    let copy_mgr = CopyManager::new(project_root.clone(), copies_dir);

    // Fetch the branch from the copy into source repo.
    println!("fetching '{}' from copy...", branch);
    copy_mgr.fetch_branch(&copy_path, &branch)?;

    // Rebase in source repo.
    println!("rebasing '{}' onto main...", branch);
    let output = Command::new("git")
        .args(["checkout", &branch])
        .current_dir(&project_root)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("checkout {branch} failed: {stderr}");
    }

    let output = Command::new("git")
        .args(["rebase", "main"])
        .current_dir(&project_root)
        .output()?;
    if !output.status.success() {
        let _ = Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(&project_root)
            .output();
        let _ = Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&project_root)
            .output();
        anyhow::bail!(
            "rebase conflict. Resolve manually:\n  \
             cd {} && git checkout {branch} && git rebase main\n  \
             Then re-run `enki task retry {task_id}`",
            project_root.display()
        );
    }

    // Merge into main.
    println!("merging '{}' into main...", branch);
    let _ = Command::new("git")
        .args(["checkout", "main"])
        .current_dir(&project_root)
        .output()?;
    let output = Command::new("git")
        .args(["merge", "--ff-only", &branch])
        .current_dir(&project_root)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ff-only merge failed: {stderr}");
    }

    // Clean up.
    println!("cleaning up...");
    let _ = copy_mgr.delete_branch(&branch);
    let _ = copy_mgr.remove_copy(&copy_path);

    db.update_task_status(&task_id, TaskStatus::Done)?;
    println!("task {} marked done.", task_id);

    // Promote dependents whose all deps are now done.
    let dependents = db.get_dependents(&task_id).unwrap_or_default();
    for dep_id in &dependents {
        let Ok(dep_task) = db.get_task(dep_id) else { continue };
        if dep_task.status != TaskStatus::Open {
            continue;
        }
        let all_deps = db.get_dependencies(dep_id).unwrap_or_default();
        let all_done = all_deps.iter().all(|d| {
            db.get_task(d)
                .map(|t| t.status == TaskStatus::Done)
                .unwrap_or(false)
        });
        if all_done {
            let _ = db.update_task_status(dep_id, TaskStatus::Ready);
            println!("promoted dependent task {} to ready.", dep_id);
        }
    }

    Ok(())
}
