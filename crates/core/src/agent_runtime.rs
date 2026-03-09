//! Resolve agent binaries for ACP sessions.
//!
//! The built-in `claude-agent-acp` agent is installed via npm into
//! `~/.enki/agents/claude-agent-acp/` on first use. Custom agents specified
//! in config are resolved from PATH or as absolute paths.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::config::AgentConfig;

const BUILTIN_COMMAND: &str = "claude-agent-acp";
const PACKAGE: &str = "@zed-industries/claude-agent-acp";
const ENTRY_POINT: &str = "node_modules/@zed-industries/claude-agent-acp/dist/index.js";

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("node not found on PATH — install Node.js first")]
    NodeNotFound,
    #[error("npm install failed: {0}")]
    NpmInstallFailed(String),
    #[error("home directory not found")]
    NoHomeDir,
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolved agent command ready to spawn.
#[derive(Debug, Clone)]
pub struct AgentCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Find `node` on PATH.
fn find_node() -> Result<PathBuf, ResolveError> {
    which::which("node").map_err(|_| ResolveError::NodeNotFound)
}

/// Directory where enki caches agent packages: `~/.enki/agents/claude-agent-acp/`
fn cache_dir() -> Result<PathBuf, ResolveError> {
    let home = dirs::home_dir().ok_or(ResolveError::NoHomeDir)?;
    Ok(home.join(".enki").join("agents").join("claude-agent-acp"))
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

/// Resolve the built-in claude-agent-acp agent.
fn resolve_builtin(extra_args: &[String], env: &HashMap<String, String>) -> Result<AgentCommand, ResolveError> {
    let node = find_node()?;
    let cache = cache_dir()?;
    let entry = cache.join(ENTRY_POINT);

    if !entry.exists() {
        tracing::info!(package = PACKAGE, path = %cache.display(), "installing agent package");
        npm_install(&cache)?;

        if !entry.exists() {
            return Err(ResolveError::NpmInstallFailed(format!(
                "entry point not found after install: {}",
                entry.display()
            )));
        }
    }

    let mut args = vec![entry.to_string_lossy().into_owned()];
    args.extend_from_slice(extra_args);

    Ok(AgentCommand {
        program: node,
        args,
        env: env.clone(),
    })
}

/// Resolve an agent command from config.
///
/// If `command` is `"claude-agent-acp"` (the default), uses the built-in
/// npm-based resolution. Otherwise, resolves the command from PATH or as
/// an absolute path.
pub fn resolve_from_config(config: &AgentConfig) -> Result<AgentCommand, ResolveError> {
    if config.command == BUILTIN_COMMAND {
        return resolve_builtin(&config.args, &config.env);
    }

    let program = if config.command.contains('/') {
        // Absolute or relative path — use directly
        PathBuf::from(&config.command)
    } else {
        // Look up on PATH
        which::which(&config.command)
            .map_err(|_| ResolveError::AgentNotFound(config.command.clone()))?
    };

    Ok(AgentCommand {
        program,
        args: config.args.clone(),
        env: config.env.clone(),
    })
}

/// Resolve the built-in claude-agent-acp agent with default config.
///
/// Convenience wrapper for callers that don't have config (e.g. tests).
pub fn resolve() -> Result<AgentCommand, ResolveError> {
    resolve_builtin(&[], &HashMap::new())
}
