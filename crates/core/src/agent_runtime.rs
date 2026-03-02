//! Resolve and cache the claude-code-acp agent binary.
//!
//! On first use, installs `@zed-industries/claude-code-acp` via npm into
//! `~/.enki/agents/claude-code-acp/`. On subsequent launches, reuses the
//! cached install. Returns the `node` binary path and the entry-point script
//! so callers can spawn `node <script>` directly — no `bunx` overhead.

use std::path::PathBuf;
use std::process::Command;

const PACKAGE: &str = "@zed-industries/claude-code-acp";
const ENTRY_POINT: &str = "node_modules/@zed-industries/claude-code-acp/dist/index.js";

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("node not found on PATH — install Node.js first")]
    NodeNotFound,
    #[error("npm install failed: {0}")]
    NpmInstallFailed(String),
    #[error("home directory not found")]
    NoHomeDir,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolved agent command ready to spawn.
#[derive(Debug, Clone)]
pub struct AgentCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Find `node` on PATH.
fn find_node() -> Result<PathBuf, ResolveError> {
    which::which("node").map_err(|_| ResolveError::NodeNotFound)
}

/// Directory where enki caches agent packages: `~/.enki/agents/claude-code-acp/`
fn cache_dir() -> Result<PathBuf, ResolveError> {
    let home = dirs::home_dir().ok_or(ResolveError::NoHomeDir)?;
    Ok(home.join(".enki").join("agents").join("claude-code-acp"))
}

/// Install the package into the cache directory using npm.
fn npm_install(cache: &PathBuf) -> Result<(), ResolveError> {
    std::fs::create_dir_all(cache)?;

    let output = Command::new("npm")
        .args(["install", "--prefix", cache.to_str().unwrap(), PACKAGE])
        .output()
        .map_err(|e| ResolveError::NpmInstallFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ResolveError::NpmInstallFailed(stderr.to_string()));
    }

    Ok(())
}

/// Resolve the claude-code-acp agent command.
///
/// Returns `node <path-to-index.js>` for direct spawning.
/// Installs the package on first use.
pub fn resolve() -> Result<AgentCommand, ResolveError> {
    let node = find_node()?;
    let cache = cache_dir()?;
    let entry = cache.join(ENTRY_POINT);

    if !entry.exists() {
        tracing::info!("installing {PACKAGE} into {}", cache.display());
        npm_install(&cache)?;

        if !entry.exists() {
            return Err(ResolveError::NpmInstallFailed(format!(
                "entry point not found after install: {}",
                entry.display()
            )));
        }
    }

    Ok(AgentCommand {
        program: node,
        args: vec![entry.to_string_lossy().into_owned()],
    })
}
