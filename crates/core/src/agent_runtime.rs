//! Resolve agent binaries for ACP sessions.
//!
//! Built-in agents are installed via npm into `~/.enki/agents/<name>/` on
//! first use. Custom agents specified in config are resolved from PATH or
//! as absolute paths.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::config::AgentConfig;

/// A built-in agent that can be resolved by short name.
struct BuiltinAgent {
    /// Short name used in config and CLI (e.g. "claude", "codex").
    name: &'static str,
    /// npm package to install.
    package: &'static str,
    /// Entry point relative to the cache directory.
    entry_point: &'static str,
    /// Subdirectory name under `~/.enki/agents/`.
    cache_name: &'static str,
}

const BUILTINS: &[BuiltinAgent] = &[
    BuiltinAgent {
        name: "claude",
        package: "@zed-industries/claude-agent-acp",
        entry_point: "node_modules/@zed-industries/claude-agent-acp/dist/index.js",
        cache_name: "claude-agent-acp",
    },
    BuiltinAgent {
        name: "codex",
        package: "@zed-industries/codex-acp",
        entry_point: "node_modules/@zed-industries/codex-acp/dist/index.js",
        cache_name: "codex-acp",
    },
];

/// Default built-in agent name.
pub const DEFAULT_AGENT: &str = "claude";

/// Return the short names of all built-in agents.
pub fn builtin_names() -> Vec<&'static str> {
    BUILTINS.iter().map(|b| b.name).collect()
}

fn find_builtin(name: &str) -> Option<&'static BuiltinAgent> {
    BUILTINS.iter().find(|b| b.name == name)
}

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

/// Directory where enki caches a specific agent package.
fn cache_dir(cache_name: &str) -> Result<PathBuf, ResolveError> {
    let home = dirs::home_dir().ok_or(ResolveError::NoHomeDir)?;
    Ok(home.join(".enki").join("agents").join(cache_name))
}

/// Install an npm package into the cache directory.
fn npm_install(cache: &PathBuf, package: &str) -> Result<(), ResolveError> {
    std::fs::create_dir_all(cache)?;

    let output = Command::new("npm")
        .args(["install", "--prefix", cache.to_str().unwrap(), package])
        .output()
        .map_err(|e| ResolveError::NpmInstallFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ResolveError::NpmInstallFailed(stderr.to_string()));
    }

    Ok(())
}

/// Resolve a built-in agent by its registry entry.
fn resolve_builtin(
    builtin: &BuiltinAgent,
    extra_args: &[String],
    env: &HashMap<String, String>,
) -> Result<AgentCommand, ResolveError> {
    let node = find_node()?;
    let cache = cache_dir(builtin.cache_name)?;
    let entry = cache.join(builtin.entry_point);

    if !entry.exists() {
        tracing::info!(package = builtin.package, path = %cache.display(), "installing agent package");
        npm_install(&cache, builtin.package)?;

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
/// If `command` matches a built-in name ("claude", "codex"), uses npm-based
/// resolution. Also accepts the legacy value "claude-agent-acp" for backwards
/// compatibility. Otherwise, resolves the command from PATH or as an absolute path.
pub fn resolve_from_config(config: &AgentConfig) -> Result<AgentCommand, ResolveError> {
    // Backwards compat: "claude-agent-acp" → "claude"
    let name = if config.command == "claude-agent-acp" {
        "claude"
    } else {
        &config.command
    };

    if let Some(builtin) = find_builtin(name) {
        return resolve_builtin(builtin, &config.args, &config.env);
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

/// Resolve the default built-in agent with default config.
///
/// Convenience wrapper for callers that don't have config (e.g. tests).
pub fn resolve() -> Result<AgentCommand, ResolveError> {
    let builtin = find_builtin(DEFAULT_AGENT).unwrap();
    resolve_builtin(builtin, &[], &HashMap::new())
}
