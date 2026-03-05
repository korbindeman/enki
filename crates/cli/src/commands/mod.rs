mod init;
pub mod mcp;

pub use init::init;

use std::path::PathBuf;

use enki_core::db::Db;

/// Find the `.enki/` directory by checking `ENKI_DIR` env var first,
/// then looking in the current directory.
pub fn enki_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("ENKI_DIR") {
        let p = PathBuf::from(dir);
        if p.join("db.sqlite").exists() {
            return Ok(p);
        }
        anyhow::bail!("ENKI_DIR={} does not contain db.sqlite", p.display());
    }

    let candidate = std::env::current_dir()?.join(".enki");
    if candidate.join("db.sqlite").exists() {
        return Ok(candidate);
    }
    anyhow::bail!("not an enki project (no .enki/ found in current directory). Run `enki` first.");
}

/// Path to the project's SQLite database.
pub fn db_path() -> anyhow::Result<PathBuf> {
    Ok(enki_dir()?.join("db.sqlite"))
}

/// Base directory for worker copies.
pub fn copies_dir() -> anyhow::Result<PathBuf> {
    Ok(enki_dir()?.join("copies"))
}

/// Project root (parent of `.enki/`).
pub fn project_root() -> anyhow::Result<PathBuf> {
    Ok(enki_dir()?.parent().unwrap().to_path_buf())
}

/// Global enki directory for logs and shared caches.
pub fn global_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".enki")
}

/// Open the project database, or error if not initialized.
pub fn open_db() -> anyhow::Result<Db> {
    let path = db_path()?;
    if !path.exists() {
        anyhow::bail!(
            "not an enki project (no .enki/ found). Run `enki` first."
        );
    }
    Ok(Db::open(path.to_str().unwrap())?)
}
