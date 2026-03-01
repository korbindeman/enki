mod init;
mod project;
mod run;
mod status;
mod task;

pub use init::init;
pub use project::{project, ProjectCmd};
pub use run::run;
pub use status::status;
pub use task::{task, TaskCmd};

use std::path::PathBuf;

use enki_core::db::Db;

/// Default workspace directory.
fn workspace_dir() -> PathBuf {
    dirs().join(".enki")
}

/// Default database path.
fn db_path() -> PathBuf {
    workspace_dir().join("db.sqlite")
}

fn dirs() -> PathBuf {
    home::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Open the workspace database, or error if not initialized.
fn open_db() -> anyhow::Result<Db> {
    let path = db_path();
    if !path.exists() {
        anyhow::bail!(
            "workspace not initialized. Run `enki init` first."
        );
    }
    Ok(Db::open(path.to_str().unwrap())?)
}
