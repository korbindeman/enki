use enki_core::types::TaskStatus;

use super::open_db;

pub async fn status() -> anyhow::Result<()> {
    let db = open_db()?;
    let tasks = db.list_tasks()?;

    let open = tasks.iter().filter(|t| matches!(t.status, TaskStatus::Open | TaskStatus::Ready)).count();
    let running = tasks.iter().filter(|t| t.status == TaskStatus::Running).count();
    let done = tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    let failed = tasks.iter().filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked)).count();

    let project_root = super::project_root()?;
    let name = project_root.file_name().unwrap_or_default().to_string_lossy();

    println!("enki — {name}");
    println!("──────────────");
    println!("tasks: {} open, {} running, {} done, {} failed ({} total)", open, running, done, failed, tasks.len());

    Ok(())
}
