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
            },
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
