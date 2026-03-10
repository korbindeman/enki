use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::scheduler::Limits;

#[derive(Debug, Clone)]
pub struct Config {
    pub git: GitConfig,
    pub workers: WorkersConfig,
    pub agent: AgentConfig,
}

#[derive(Debug, Clone)]
pub struct GitConfig {
    pub commit_suffix: String,
}

#[derive(Debug, Clone)]
pub struct WorkersConfig {
    pub limits: Limits,
    pub sonnet_only: bool,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub overrides: HashMap<String, AgentOverride>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentOverride {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            git: GitConfig { commit_suffix: "created by enki".into() },
            workers: WorkersConfig {
                limits: Limits::default(),
                sonnet_only: false,
            },
            agent: AgentConfig {
                command: "claude".into(),
                args: vec![],
                env: HashMap::new(),
                overrides: HashMap::new(),
            },
        }
    }
}

impl Config {
    /// Return an `AgentConfig` for a specific role, merging the base config with
    /// any role-specific override. The returned config has no `overrides` map,
    /// making it ready to pass to `resolve_from_config`.
    pub fn agent_for_role(&self, role: &str) -> AgentConfig {
        let base = &self.agent;
        match base.overrides.get(role) {
            None => AgentConfig {
                command: base.command.clone(),
                args: base.args.clone(),
                env: base.env.clone(),
                overrides: HashMap::new(),
            },
            Some(ov) => {
                let env = if let Some(ref ov_env) = ov.env {
                    let mut merged = base.env.clone();
                    merged.extend(ov_env.iter().map(|(k, v)| (k.clone(), v.clone())));
                    merged
                } else {
                    base.env.clone()
                };
                AgentConfig {
                    command: ov.command.clone().unwrap_or_else(|| base.command.clone()),
                    args: ov.args.clone().unwrap_or_else(|| base.args.clone()),
                    env,
                    overrides: HashMap::new(),
                }
            }
        }
    }
}

// TOML deserialization types (all fields optional for overlay merging)

#[derive(Deserialize, Default)]
#[serde(default)]
struct ConfigToml {
    git: GitToml,
    workers: WorkersToml,
    agent: AgentToml,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GitToml {
    commit_suffix: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WorkersToml {
    max_workers: Option<usize>,
    max_heavy: Option<usize>,
    max_standard: Option<usize>,
    max_light: Option<usize>,
    sonnet_only: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AgentToml {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    coordinator: Option<AgentOverrideToml>,
    worker: Option<AgentOverrideToml>,
    sidecar: Option<AgentOverrideToml>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AgentOverrideToml {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
}

impl ConfigToml {
    /// Overlay this TOML onto an existing Config, replacing only the fields that are set.
    fn apply_to(self, config: &mut Config) {
        if let Some(v) = self.git.commit_suffix {
            config.git.commit_suffix = v;
        }
        if let Some(v) = self.workers.max_workers {
            config.workers.limits.max_workers = v;
        }
        if let Some(v) = self.workers.max_heavy {
            config.workers.limits.max_heavy = v;
        }
        if let Some(v) = self.workers.max_standard {
            config.workers.limits.max_standard = v;
        }
        if let Some(v) = self.workers.max_light {
            config.workers.limits.max_light = v;
        }
        if let Some(v) = self.workers.sonnet_only {
            config.workers.sonnet_only = v;
        }
        if let Some(v) = self.agent.command {
            config.agent.command = v;
        }
        if let Some(v) = self.agent.args {
            config.agent.args = v;
        }
        if let Some(v) = self.agent.env {
            config.agent.env = v;
        }
        for (role, toml) in [
            ("coordinator", self.agent.coordinator),
            ("worker", self.agent.worker),
            ("sidecar", self.agent.sidecar),
        ] {
            if let Some(ov) = toml {
                let entry = config.agent.overrides.entry(role.to_string()).or_default();
                if let Some(v) = ov.command {
                    entry.command = Some(v);
                }
                if let Some(v) = ov.args {
                    entry.args = Some(v);
                }
                if let Some(v) = ov.env {
                    entry.env = Some(v);
                }
            }
        }
    }
}

fn load_file(path: &Path, config: &mut Config) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    match toml::from_str::<ConfigToml>(&contents) {
        Ok(toml) => toml.apply_to(config),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "failed to parse config file");
        }
    }
}

/// Load config: defaults → `~/.config/enki.toml` → `<project>/.enki/enki.toml`.
/// Later files override earlier ones field-by-field.
pub fn load_config(project_dir: &Path) -> Config {
    let mut config = Config::default();

    if let Some(config_dir) = dirs::config_dir() {
        load_file(&config_dir.join("enki.toml"), &mut config);
    }

    load_file(&project_dir.join(".enki").join("enki.toml"), &mut config);

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let config = Config::default();
        assert_eq!(config.git.commit_suffix, "created by enki");
        assert_eq!(config.workers.limits.max_workers, 10);
        assert!(!config.workers.sonnet_only);
        assert_eq!(config.agent.command, "claude");
    }

    #[test]
    fn parse_partial_toml() {
        let toml_str = r#"
[git]
commit_suffix = ""

[workers]
sonnet_only = true
max_heavy = 2
"#;
        let parsed: ConfigToml = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        parsed.apply_to(&mut config);

        assert_eq!(config.git.commit_suffix, "");
        assert!(config.workers.sonnet_only);
        assert_eq!(config.workers.limits.max_heavy, 2);
        // Unset fields keep defaults
        assert_eq!(config.workers.limits.max_workers, 10);
        assert_eq!(config.agent.command, "claude");
    }

    #[test]
    fn parse_custom_agent() {
        let toml_str = r#"
[agent]
command = "/usr/local/bin/my-agent"
args = ["--verbose"]
env = { MY_API_KEY = "secret" }
"#;
        let parsed: ConfigToml = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        parsed.apply_to(&mut config);

        assert_eq!(config.agent.command, "/usr/local/bin/my-agent");
        assert_eq!(config.agent.args, vec!["--verbose"]);
        assert_eq!(config.agent.env.get("MY_API_KEY").unwrap(), "secret");
    }

    #[test]
    fn role_override_worker() {
        let toml_str = r#"
[agent]
command = "claude"
args = ["--base"]
env = { BASE_KEY = "base_val" }

[agent.worker]
command = "opencode"
env = { OLLAMA_HOST = "http://192.168.1.x:11434" }
"#;
        let parsed: ConfigToml = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        parsed.apply_to(&mut config);

        // Worker gets overridden command, inherits args from base, merges env
        let worker = config.agent_for_role("worker");
        assert_eq!(worker.command, "opencode");
        assert_eq!(worker.args, vec!["--base"]);
        assert_eq!(worker.env.get("BASE_KEY").unwrap(), "base_val");
        assert_eq!(worker.env.get("OLLAMA_HOST").unwrap(), "http://192.168.1.x:11434");
        assert!(worker.overrides.is_empty());

        // Coordinator falls back to base
        let coord = config.agent_for_role("coordinator");
        assert_eq!(coord.command, "claude");
        assert_eq!(coord.args, vec!["--base"]);
        assert!(coord.overrides.is_empty());
    }

    #[test]
    fn role_override_partial_inherit() {
        let toml_str = r#"
[agent]
command = "claude"
args = ["--verbose"]
env = { KEY = "val" }

[agent.sidecar]
command = "fast-agent"
"#;
        let parsed: ConfigToml = toml::from_str(toml_str).unwrap();
        let mut config = Config::default();
        parsed.apply_to(&mut config);

        let sidecar = config.agent_for_role("sidecar");
        assert_eq!(sidecar.command, "fast-agent");
        // args and env inherited from base
        assert_eq!(sidecar.args, vec!["--verbose"]);
        assert_eq!(sidecar.env.get("KEY").unwrap(), "val");
    }

    #[test]
    fn role_override_overlay_merging() {
        // Global config sets base
        let global = r#"
[agent]
command = "claude"
args = ["--global"]
"#;
        // Project config adds worker override
        let project = r#"
[agent.worker]
command = "opencode"
"#;
        let mut config = Config::default();
        toml::from_str::<ConfigToml>(global).unwrap().apply_to(&mut config);
        toml::from_str::<ConfigToml>(project).unwrap().apply_to(&mut config);

        // Worker uses project override command, inherits global args
        let worker = config.agent_for_role("worker");
        assert_eq!(worker.command, "opencode");
        assert_eq!(worker.args, vec!["--global"]);

        // Coordinator uses global base
        let coord = config.agent_for_role("coordinator");
        assert_eq!(coord.command, "claude");
        assert_eq!(coord.args, vec!["--global"]);
    }

    #[test]
    fn overlay_order() {
        let base = r#"
[git]
commit_suffix = "base"
[workers]
max_workers = 5
"#;
        let overlay = r#"
[workers]
max_workers = 3
"#;
        let mut config = Config::default();
        toml::from_str::<ConfigToml>(base).unwrap().apply_to(&mut config);
        toml::from_str::<ConfigToml>(overlay).unwrap().apply_to(&mut config);

        assert_eq!(config.git.commit_suffix, "base");
        assert_eq!(config.workers.limits.max_workers, 3);
    }
}
