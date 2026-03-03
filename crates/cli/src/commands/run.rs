use std::path::PathBuf;

use enki_acp::{AgentManager, SessionUpdate};
use enki_core::types::{Id, TaskStatus};
use enki_core::worktree::WorktreeManager;

use super::open_db;

pub async fn run(task_id: &str, agent_cmd: &str, agent_args: &str, keep: bool, enki_bin: std::path::PathBuf) -> anyhow::Result<()> {
    let db = open_db()?;
    let task_id = Id(task_id.to_string());

    // Load task
    let task = db.get_task(&task_id)?;
    if task.status != TaskStatus::Open && task.status != TaskStatus::Ready {
        anyhow::bail!(
            "task {} is in state '{}', expected 'open' or 'ready'",
            task_id,
            task.status.as_str()
        );
    }

    println!("task: {} — {}", task.title, task.description.as_deref().unwrap_or(""));

    // Create worktree
    let branch = format!("task/{}", task_id);
    let bare_path = super::bare_path()?;
    let worktree_dir = super::worktree_base()?;
    std::fs::create_dir_all(&worktree_dir)?;

    let wt_mgr = WorktreeManager::new(&bare_path)?;

    // Sync bare repo before branching so the worker starts from current code
    // (including uncommitted/untracked files).
    if let Err(e) = wt_mgr.sync() {
        eprintln!("warning: bare repo sync failed: {e}");
    }

    let start_ref = wt_mgr.default_start_ref()?;
    println!("creating worktree on branch '{}' from '{}'...", branch, start_ref);
    let worktree_path = wt_mgr.create(&branch, &start_ref, &worktree_dir)?;
    println!("worktree: {}", worktree_path.display());

    // Update task status
    db.assign_task(
        &task_id,
        &Id("cli-direct".into()),
        worktree_path.to_str().unwrap(),
        &branch,
    )?;

    // Build the prompt
    let prompt = build_prompt(&task.title, task.description.as_deref().unwrap_or(""));

    // Run ACP session
    println!("spawning ACP agent: {} {}...", agent_cmd, agent_args);
    let args: Vec<&str> = agent_args.split_whitespace().collect();

    // ACP futures are !Send, so we need a LocalSet
    let local = tokio::task::LocalSet::new();
    let result = local
        .run_until(run_acp_session(
            agent_cmd,
            &args,
            worktree_path.clone(),
            &prompt,
            &enki_bin,
        ))
        .await;

    match result {
        Ok(stop_reason) => {
            println!("\nagent finished ({})", stop_reason);
            db.update_task_status(&task_id, TaskStatus::Done)?;
            println!("task marked as done.");

            if keep {
                println!("worktree kept at: {}", worktree_path.display());
            } else {
                print!("cleaning up worktree... ");
                match wt_mgr.remove(&worktree_path, false) {
                    Ok(()) => println!("done."),
                    Err(e) => println!("failed: {e}\nworktree left at: {}", worktree_path.display()),
                }
            }
        }
        Err(e) => {
            eprintln!("\nagent error: {e}");
            db.update_task_status(&task_id, TaskStatus::Failed)?;
            println!("task marked as failed.");
            println!("worktree left at: {} (inspect and clean up manually)", worktree_path.display());
        }
    }

    Ok(())
}

async fn run_acp_session(
    agent_cmd: &str,
    agent_args: &[&str],
    cwd: PathBuf,
    prompt: &str,
    enki_bin: &std::path::Path,
) -> anyhow::Result<String> {
    let mut mgr = AgentManager::new();

    // Set env vars so the agent can call `enki` and find the project DB.
    let mut env = std::collections::HashMap::new();
    env.insert("ENKI_BIN".to_string(), enki_bin.display().to_string());
    if let Ok(enki_dir) = super::enki_dir() {
        env.insert("ENKI_DIR".to_string(), enki_dir.display().to_string());
    }
    mgr.set_env(env);

    mgr.on_update(|_session_id, update| match update {
        SessionUpdate::Text(text) => {
            print!("{}", text);
        }
        SessionUpdate::ToolCallStarted { title, .. } => {
            println!("\n[tool] {}", title);
        }
        SessionUpdate::ToolCallDone { .. } => {}
        SessionUpdate::Plan(plan) => {
            if let Some(entries) = plan.as_array() {
                println!("\n[plan]");
                for entry in entries {
                    if let Some(content) = entry.get("content").and_then(|c| c.as_str()) {
                        println!("  - {}", content);
                    }
                }
            }
        }
    });

    let session_id = mgr.start_session(agent_cmd, agent_args, cwd).await?;
    let stop_reason = mgr.prompt(&session_id, prompt).await?;
    mgr.kill_session(&session_id);

    Ok(stop_reason)
}

fn build_prompt(title: &str, description: &str) -> String {
    format!(
        r#"You are a focused coding agent working on a single task.

TASK: {title}
{description}

APPROACH:
1. Read the task description and understand what needs to be done
2. Understand the existing code relevant to your task
3. Write a failing test for the expected behavior
4. Implement the minimum code to make the test pass
5. Run the full test suite to verify no regressions
6. Clean up (lint, format, review your own diff)
7. Commit with a clear message

RULES:
- Only modify files relevant to your task
- If something is ambiguous, make a reasonable choice and note it
- Keep your changes minimal and focused"#
    )
}
