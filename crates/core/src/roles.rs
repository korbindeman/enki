use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputMode {
    /// Worker produces code changes on a branch (merged by refinery).
    Branch,
    /// Worker produces a markdown artifact file (no merge).
    Artifact,
}

impl Default for OutputMode {
    fn default() -> Self {
        OutputMode::Branch
    }
}

#[derive(Debug, Clone)]
pub struct RoleConfig {
    pub name: String,
    pub label: String,
    pub description: String,
    pub system_prompt: String,
    pub can_edit: bool,
    pub output: OutputMode,
}

#[derive(Deserialize)]
struct RoleToml {
    name: String,
    label: String,
    description: String,
    system_prompt: String,
    #[serde(default = "default_true")]
    can_edit: bool,
    #[serde(default)]
    output: Option<String>,
}

fn default_true() -> bool {
    true
}

impl From<RoleToml> for RoleConfig {
    fn from(t: RoleToml) -> Self {
        let output = match t.output.as_deref() {
            Some("artifact") => OutputMode::Artifact,
            _ => OutputMode::Branch,
        };
        RoleConfig {
            name: t.name,
            label: t.label,
            description: t.description,
            system_prompt: t.system_prompt,
            can_edit: t.can_edit,
            output,
        }
    }
}

/// Load roles: built-in defaults, then ~/.enki/roles/*.toml, then <project>/.enki/roles/*.toml.
/// Later entries override earlier ones by name.
pub fn load_roles(project_dir: &Path) -> HashMap<String, RoleConfig> {
    let mut roles = builtin_roles();

    // Global overrides.
    if let Some(home) = dirs::home_dir() {
        load_from_dir(&home.join(".enki").join("roles"), &mut roles);
    }

    // Per-project overrides.
    load_from_dir(&project_dir.join(".enki").join("roles"), &mut roles);

    roles
}

fn load_from_dir(dir: &Path, roles: &mut HashMap<String, RoleConfig>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                match toml::from_str::<RoleToml>(&contents) {
                    Ok(role_toml) => {
                        let config: RoleConfig = role_toml.into();
                        roles.insert(config.name.clone(), config);
                    }
                    Err(e) => {
                        tracing::warn!("failed to parse role file {}: {e}", path.display());
                    }
                }
            }
        }
    }
}

fn builtin_roles() -> HashMap<String, RoleConfig> {
    let mut roles = HashMap::new();

    roles.insert(
        "feature_developer".into(),
        RoleConfig {
            name: "feature_developer".into(),
            label: "Feature Developer".into(),
            description: "Implements new features, writes production code, adds tests. Output: code changes (branch).".into(),
            system_prompt: r#"You are a feature developer. Your job is to implement the requested feature cleanly and completely.

- Write production-quality code that follows the existing patterns and conventions in the codebase
- Add or update tests to cover your changes
- Only modify files relevant to your task
- If you need to understand existing code before changing it, read it first
- Think about edge cases and error handling"#
                .into(),
            can_edit: true,
            output: OutputMode::Branch,
        },
    );

    roles.insert(
        "bug_fixer".into(),
        RoleConfig {
            name: "bug_fixer".into(),
            label: "Bug Fixer".into(),
            description: "Diagnoses root causes, writes surgical fixes and regression tests. Output: code changes (branch).".into(),
            system_prompt: r#"You are a bug fixer. Your job is to find the root cause and fix it surgically.

- Start by understanding the bug: read the relevant code, reproduce the issue mentally
- Find the root cause — don't patch symptoms
- Make the minimal change needed to fix the bug
- Write a regression test that would have caught this bug
- Verify your fix doesn't break adjacent functionality"#
                .into(),
            can_edit: true,
            output: OutputMode::Branch,
        },
    );

    roles.insert(
        "researcher".into(),
        RoleConfig {
            name: "researcher".into(),
            label: "Researcher".into(),
            description:
                "Investigates code, reads files, traces execution paths. Output: markdown artifact (no code changes)."
                    .into(),
            system_prompt: r#"You are a research agent. Your job is to investigate the codebase thoroughly and report your findings.

- Read files, trace code paths, search for patterns
- Do NOT edit any files — you are read-only
- Be precise: include file paths, line numbers, and code snippets in your report
- Structure your findings clearly with markdown headings
- Answer the specific question or investigate the specific area described in your task
- Your findings will be saved as a markdown artifact for the team to reference"#
                .into(),
            can_edit: false,
            output: OutputMode::Artifact,
        },
    );

    roles.insert(
        "code_referencer".into(),
        RoleConfig {
            name: "code_referencer".into(),
            label: "Code Referencer".into(),
            description:
                "Fetches and references code from GitHub repos and external sources. Output: markdown artifact (no code changes)."
                    .into(),
            system_prompt: r#"You are a code reference agent. Your job is to look up external code, documentation, and patterns from GitHub repositories and the internet to inform the team's work.

- Use `git clone --depth 1` for shallow checkouts of reference repositories
- Report relevant patterns, APIs, interfaces, and code snippets
- Do NOT modify the project's files — you are read-only
- Clean up any cloned repos when you're done (rm -rf)
- Cite your sources: include repo URLs, file paths, and relevant code excerpts
- Your findings will be saved as a markdown artifact for the team to reference"#
                .into(),
            can_edit: false,
            output: OutputMode::Artifact,
        },
    );

    roles
}
