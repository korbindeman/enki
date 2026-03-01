use std::path::PathBuf;

use chrono::Utc;
use clap::Subcommand;
use enki_core::types::{Id, Project};
use enki_core::worktree::WorktreeManager;

use super::open_db;

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Register a git repository as a project.
    Add {
        /// Path to the git repository.
        path: PathBuf,
        /// Project name (defaults to directory name).
        #[arg(long)]
        name: Option<String>,
    },
    /// List registered projects.
    List,
}

pub async fn project(cmd: ProjectCmd) -> anyhow::Result<()> {
    match cmd {
        ProjectCmd::Add { path, name } => add_project(path, name).await,
        ProjectCmd::List => list_projects().await,
    }
}

async fn add_project(path: PathBuf, name: Option<String>) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(&path)?;

    if !path.join(".git").exists() {
        anyhow::bail!("{} is not a git repository", path.display());
    }

    let project_name = name.unwrap_or_else(|| {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });

    // Create bare repo for worktree isolation
    let bare_path = path.join(".enki.git");
    if !bare_path.exists() {
        println!("creating bare repo at {}...", bare_path.display());
        WorktreeManager::init_bare(&path, &bare_path)?;
    }

    let db = open_db()?;

    let project = Project {
        id: Id::new("proj"),
        name: project_name.clone(),
        repo_url: None,
        local_path: path.to_string_lossy().to_string(),
        bare_repo: bare_path.to_string_lossy().to_string(),
        created_at: Utc::now(),
    };

    db.insert_project(&project)?;
    println!("added project '{}' ({})", project_name, project.id);
    Ok(())
}

async fn list_projects() -> anyhow::Result<()> {
    let db = open_db()?;
    let projects = db.list_projects()?;

    if projects.is_empty() {
        println!("no projects registered. Use `enki project add <path>` to add one.");
        return Ok(());
    }

    for p in &projects {
        println!("{} | {} | {}", p.id, p.name, p.local_path);
    }
    Ok(())
}
