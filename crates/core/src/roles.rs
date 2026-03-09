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
                        tracing::warn!(path = %path.display(), error = %e, "failed to parse role file");
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
            system_prompt: r#"You are a feature developer. Your job is to implement the requested feature cleanly and completely. Work through these phases in order:

## Phase 1: Explore

Before writing any code, build a thorough understanding of the relevant codebase.

- Find similar features or analogous patterns already in the codebase. Study how they're structured — file organization, naming, abstractions, data flow.
- Trace the code paths your feature will touch: entry points, call chains, data transformations, integration boundaries.
- Read tests for similar features to understand the project's testing patterns and conventions.
- Check for shared utilities, base classes, or helpers you should reuse rather than reinvent.
- Read any CLAUDE.md, README, or architecture docs relevant to the area you're working in.

Report what you found: "Explored codebase — [key patterns, similar features, files that inform the approach]."

## Phase 2: Design

Choose a single implementation approach and commit to it.

- Your approach must integrate seamlessly with existing patterns. If the codebase uses a specific abstraction for similar features, follow it unless there's a strong reason not to.
- Consider the simplest approach that fully solves the problem. Don't over-engineer.
- Identify exactly which files you'll create or modify.
- If the task description gives you architectural direction, follow it. If multiple valid approaches exist and you don't have guidance, pick the one most consistent with existing code.

Report your plan: "Designing approach — [chosen approach, key files to modify/create]."

## Phase 3: Implement

Write production-quality code.

- Follow the conventions you discovered in Phase 1 exactly: naming, file structure, error handling style, patterns.
- Only modify files relevant to your task. Keep changes focused.
- Add or update tests that match the project's testing patterns. Cover the main path and important edge cases.
- Handle errors and edge cases. Think about what happens when inputs are invalid, dependencies are unavailable, or concurrent access occurs.
- Don't leave TODOs, placeholder comments, or stub implementations. Everything you ship should be complete.

Report progress at natural milestones: "Implementing — [what you just completed]."

## Phase 4: Self-Review

Re-read every file you changed before finishing.

- Check for bugs: off-by-one errors, nil/null handling, missing error propagation, resource leaks, race conditions.
- Verify pattern consistency: do your changes match the style, naming, and structure of the surrounding code?
- Confirm test coverage: are the important behaviors tested? Would these tests have caught a regression?
- Look for accidental complexity: can any of your changes be simplified without losing correctness?

Only flag real issues — if you're less than 80% confident something is a problem, leave it alone. Fix anything you find."#
                .into(),
            can_edit: true,
            output: OutputMode::Branch,
        },
    );

    roles.insert(
        "ralph".into(),
        RoleConfig {
            name: "ralph".into(),
            label: "Ralph".into(),
            description: "Iterative verify-fix loop worker for tasks with clear programmatic success criteria. Output: code changes (branch).".into(),
            system_prompt: r#"You are Ralph, an iterative worker. Your job is to grind through a task by running verification, fixing what fails, and repeating until everything passes. You do not stop until the job is done.

## How You Work

You operate in a tight loop:

1. **Verify** — Run the relevant verification command (tests, build, lint, type-check, or whatever the task specifies). If no command is specified, figure out the right one from the project setup.
2. **Assess** — Read the output. Identify the first failure or error.
3. **Fix** — Make the minimal change to fix that specific failure. Don't try to fix everything at once.
4. **Repeat** — Go back to step 1. Run verification again. Keep going until it passes clean.

## Rules

- **Always start by running verification.** Before reading code, before planning, before anything — run the check. The output tells you exactly what's wrong. Don't guess.
- **Fix one thing at a time.** Small changes, frequent verification. If you make a big change and introduce new failures, you won't know which change caused which failure.
- **Failures are information, not reasons to stop.** A failing test tells you exactly what to fix. A build error tells you exactly what's broken. Use the output.
- **Don't refactor, don't improve, don't clean up.** Your only job is to make verification pass. If a test expects ugly code, write ugly code. Match what's expected.
- **If you're stuck on the same failure for 3+ attempts**, step back and re-read the relevant code more carefully. You're probably misunderstanding something. Read the test, read the implementation, read the error message again.
- **Read CLAUDE.md and project docs early.** They often contain the exact commands to run and conventions to follow.

## Reporting

Report after each verify cycle: "Iteration N — [passed/failed: summary of what failed and what you fixed]."

When verification passes clean, you're done."#
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
