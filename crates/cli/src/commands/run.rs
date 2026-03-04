use std::path::PathBuf;

use enki_acp::{AgentManager, SessionUpdate};
use enki_core::types::{Id, TaskStatus};
use enki_core::copy::{CopyManager, GitIdentity};

use super::open_db;

pub async fn run(task_id: &str, agent_cmd: &str, agent_args: &str, keep: bool, enki_bin: std::path::PathBuf) -> anyhow::Result<()> {
    let db = open_db()?;
    let task_id = Id(task_id.to_string());

    // Load task
    let task = db.get_task(&task_id)?;
    if task.status != TaskStatus::Pending {
        anyhow::bail!(
            "task {} is in state '{}', expected 'pending'",
            task_id,
            task.status.as_str()
        );
    }

    println!("task: {} — {}", task.title, task.description.as_deref().unwrap_or(""));

    // Create APFS copy for worker isolation.
    let project_root = super::project_root()?;
    let copies_dir = super::copies_dir()?;
    let git_identity = GitIdentity::from_git_config(&project_root)?;
    let copy_mgr = CopyManager::new(project_root, copies_dir, git_identity);

    let branch = format!("task/{}", task_id);
    println!("creating copy for branch '{}'...", branch);
    let (copy_path, _base_commit, base_branch) = copy_mgr.create_copy(&task_id.0)?;
    println!("copy: {}", copy_path.display());

    // Update task status
    db.assign_task(
        &task_id,
        &Id("cli-direct".into()),
        copy_path.to_str().unwrap(),
        &branch,
        &base_branch,
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
            copy_path.clone(),
            &prompt,
            &enki_bin,
            &task_id.0,
        ))
        .await;

    match result {
        Ok(stop_reason) => {
            println!("\nagent finished ({})", stop_reason);
            db.update_task_status(&task_id, TaskStatus::Done)?;
            println!("task marked as done.");

            if keep {
                println!("copy kept at: {}", copy_path.display());
            } else {
                print!("cleaning up copy... ");
                match copy_mgr.remove_copy(&copy_path) {
                    Ok(()) => println!("done."),
                    Err(e) => println!("failed: {e}\ncopy left at: {}", copy_path.display()),
                }
            }
        }
        Err(e) => {
            eprintln!("\nagent error: {e}");
            db.update_task_status(&task_id, TaskStatus::Failed)?;
            println!("task marked as failed.");
            println!("copy left at: {} (inspect and clean up manually)", copy_path.display());
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
    task_id: &str,
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

    let label = format!("run-{task_id}");
    let session_id = mgr.start_session(agent_cmd, agent_args, cwd, &label).await?;
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
