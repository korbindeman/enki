use super::open_db;

pub async fn status() -> anyhow::Result<()> {
    let db = open_db()?;
    let projects = db.list_projects()?;

    println!("enki workspace");
    println!("──────────────");
    println!("{} project(s)", projects.len());

    for p in &projects {
        let tasks = db.list_tasks(&p.id)?;
        let open = tasks.iter().filter(|t| t.status.as_str() == "open").count();
        let running = tasks.iter().filter(|t| t.status.as_str() == "running").count();
        let done = tasks.iter().filter(|t| t.status.as_str() == "done").count();
        let failed = tasks.iter().filter(|t| t.status.as_str() == "failed").count();

        println!();
        println!("  {} ({})", p.name, p.id);
        println!("    tasks: {} open, {} running, {} done, {} failed", open, running, done, failed);
    }

    Ok(())
}
