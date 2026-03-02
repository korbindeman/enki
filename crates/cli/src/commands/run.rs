use std::path::PathBuf;

use enki_acp::{AgentManager, SessionUpdate};
use enki_core::types::{Id, TaskStatus};
use enki_core::worktree::WorktreeManager;

use super::open_db;

pub async fn run(task_id: &str, agent_cmd: &str, agent_args: &str, keep: bool) -> anyhow::Result<()> {
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

    // Load project
    let project = db.get_project(&task.project_id)?;
    println!("project: {} ({})", project.name, project.local_path);
    println!("task: {} — {}", task.title, task.description.as_deref().unwrap_or(""));

    // Create worktree
    let branch = format!("task/{}", task_id);
    let bare_path = PathBuf::from(&project.bare_repo);
    let worktree_dir = bare_path.parent().unwrap().join(".enki-worktrees");
    std::fs::create_dir_all(&worktree_dir)?;

    let wt_mgr = WorktreeManager::new(&bare_path)?;

    // Warn if source repo has uncommitted work (workers won't see it).
    if let Ok(status) = wt_mgr.check_source_status() {
        if !status.is_clean() {
            eprintln!("warning: source repo has uncommitted work — workers won't see these changes");
            eprintln!("  {}", status.summary());
        }
    }

    // Sync bare repo before branching so the worker starts from current code.
    if let Err(e) = wt_mgr.sync() {
        eprintln!("warning: bare repo sync failed: {e}");
    }

    println!("creating worktree on branch '{}'...", branch);
    let worktree_path = wt_mgr.create(&branch, "origin/main", &worktree_dir)?;
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
) -> anyhow::Result<String> {
    let mgr = AgentManager::new();

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
