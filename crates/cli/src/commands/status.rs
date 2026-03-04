use enki_core::types::TaskStatus;

use super::open_db;

pub async fn status() -> anyhow::Result<()> {
    let db = open_db()?;

    let session = match db.get_latest_session()? {
        Some(s) => s,
        None => {
            println!("no sessions found. Run `enki` to start one.");
            return Ok(());
        }
    };

    let is_active = session.ended_at.is_none();
    let tasks = db.list_session_tasks(session.id.as_str())?;

    let pending = tasks.iter().filter(|t| t.status == TaskStatus::Pending).count();
    let running = tasks.iter().filter(|t| t.status == TaskStatus::Running).count();
    let done = tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    let failed = tasks.iter().filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked)).count();

    let project_root = super::project_root()?;
    let name = project_root.file_name().unwrap_or_default().to_string_lossy();

    println!("enki — {name}");
    println!("──────────────");
    if is_active {
        println!("session: {} (active)", session.id);
    } else {
        println!("session: {} (ended)", session.id);
    }
    println!("tasks: {} pending, {} running, {} done, {} failed ({} total)", pending, running, done, failed, tasks.len());

    Ok(())
}
