use enki_core::types::TaskStatus;

use super::open_db;

pub async fn history(session_id: Option<String>) -> anyhow::Result<()> {
    let db = open_db()?;

    if let Some(sid) = session_id {
        // Show details for a specific session.
        let tasks = db.list_session_tasks(&sid)?;
        if tasks.is_empty() {
            println!("no tasks found for session {sid}.");
            return Ok(());
        }

        println!("session: {sid}");
        println!("──────────────");
        for t in &tasks {
            let tier = t.tier.map(|t| t.as_str()).unwrap_or("-");
            let status = t.status.as_str();
            println!("  {} | {} | {} | {}", t.id, status, tier, t.title);
        }
        return Ok(());
    }

    // List all sessions.
    let sessions = db.list_sessions()?;
    if sessions.is_empty() {
        println!("no sessions found.");
        return Ok(());
    }

    for s in &sessions {
        let tasks = db.list_session_tasks(s.id.as_str())?;
        let done = tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
        let failed = tasks.iter().filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked)).count();
        let abandoned = tasks.iter().filter(|t| t.status == TaskStatus::Abandoned).count();
        let status = if s.ended_at.is_some() { "ended" } else { "active" };
        let date = s.started_at.format("%Y-%m-%d %H:%M");

        println!(
            "{} | {} | {} | {} tasks ({} done, {} failed, {} abandoned)",
            s.id, date, status, tasks.len(), done, failed, abandoned
        );
    }

    Ok(())
}
