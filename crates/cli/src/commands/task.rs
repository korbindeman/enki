use clap::Subcommand;

use super::open_db;

#[derive(Subcommand)]
pub enum TaskCmd {
    /// List tasks (current or most recent session).
    List {
        /// Show tasks from all sessions.
        #[arg(long)]
        all: bool,
    },
}

pub async fn task(cmd: TaskCmd) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::List { all } => list_tasks(all).await,
    }
}

async fn list_tasks(all: bool) -> anyhow::Result<()> {
    let db = open_db()?;

    let tasks = if all {
        db.list_tasks()?
    } else {
        match db.get_latest_session()? {
            Some(session) => db.list_session_tasks(session.id.as_str())?,
            None => {
                println!("no sessions found. Run `enki` to start one.");
                return Ok(());
            }
        }
    };

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
