use std::path::{Path, PathBuf};

use enki_acp::{AgentManager, SessionUpdate};
use enki_core::db::Db;
use enki_core::types::{Agent, AgentRole, AgentStatus, Id, TaskStatus, Tier};
use enki_core::worktree::{WorktreeError, WorktreeManager};
use tokio::sync::mpsc;

/// Tier concurrency limits for worker spawning.
struct TierLimits {
    max_workers: usize,
    max_heavy: usize,
    max_standard: usize,
    max_light: usize,
}

impl Default for TierLimits {
    fn default() -> Self {
        Self {
            max_workers: 5,
            max_heavy: 1,
            max_standard: 3,
            max_light: 5,
        }
    }
}

/// Messages sent from the TUI to the coordinator thread.
pub enum ToCoordinator {
    Prompt(String),
    Shutdown,
}

/// Activity update from a running worker agent.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum WorkerActivity {
    /// Agent started a tool call (e.g., "Bash", "Read", "Edit").
    ToolStarted(String),
    /// Agent finished a tool call.
    ToolDone,
    /// Agent is producing text (thinking/writing). Sent once per text
    /// burst, not per token.
    Thinking,
}

/// Messages sent from the coordinator thread back to the TUI.
#[derive(Debug)]
pub enum FromCoordinator {
    /// Connection established, system prompt being sent.
    Connected,
    /// System prompt processed, coordinator is ready for user input.
    Ready,
    /// Agent produced text output.
    Text(String),
    /// Agent started a tool call.
    ToolCall(String),
    /// Agent finished a tool call.
    #[allow(dead_code)]
    ToolCallDone(String),
    /// Prompt completed (stop reason).
    Done(String),
    /// A worker was spawned for a task.
    WorkerSpawned { task_id: String, title: String },
    /// A worker completed its task.
    WorkerCompleted { task_id: String, title: String },
    /// A worker failed its task.
    WorkerFailed {
        task_id: String,
        title: String,
        error: String,
    },
    /// A worker's branch had a merge conflict. The worktree is preserved for retry.
    WorkerConflicted {
        task_id: String,
        title: String,
        worktree: String,
        // Branch name is included for logging/display by consumers even if not always rendered.
        #[allow(dead_code)]
        branch: String,
    },
    /// Live activity update from a running worker.
    #[allow(dead_code)]
    WorkerUpdate {
        task_id: String,
        activity: WorkerActivity,
    },
    /// An error occurred.
    Error(String),
}

/// Handle held by the TUI to communicate with the coordinator.
pub struct CoordinatorHandle {
    pub tx: mpsc::UnboundedSender<ToCoordinator>,
    pub rx: mpsc::UnboundedReceiver<FromCoordinator>,
}

/// Spawn the coordinator on a dedicated OS thread with its own tokio runtime + LocalSet.
///
/// Returns a handle with channels for bidirectional communication.
pub fn spawn(cwd: PathBuf, db_path: String, enki_bin: PathBuf) -> CoordinatorHandle {
    let (to_coord_tx, to_coord_rx) = mpsc::unbounded_channel::<ToCoordinator>();
    let (from_coord_tx, from_coord_rx) = mpsc::unbounded_channel::<FromCoordinator>();

    std::thread::Builder::new()
        .name("coordinator-acp".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build coordinator runtime");

            rt.block_on(async {
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(coordinator_loop(cwd, db_path, enki_bin, to_coord_rx, from_coord_tx))
                    .await;
            });
        })
        .expect("failed to spawn coordinator thread");

    CoordinatorHandle {
        tx: to_coord_tx,
        rx: from_coord_rx,
    }
}

/// Build the system prompt that teaches the coordinator its role and tools.
fn build_system_prompt(cwd: &std::path::Path) -> String {
    let cwd_display = cwd.display();

    format!(
        r#"You are the **coordinator** for enki, a multi-agent coding orchestrator.

## Your Role

You plan work, decompose user requests into tasks, assign complexity tiers, and track progress. You are the user's primary interface for managing a codebase with multiple AI worker agents.

## Current Workspace

- Working directory: `{cwd_display}`
- Database: `~/.enki/db.sqlite`

## Available CLI Tools

You have access to the `enki` CLI via the `$ENKI_BIN` environment variable. Always invoke it as `$ENKI_BIN` (never bare `enki`) to ensure it works regardless of PATH configuration.

### Project Management
- `$ENKI_BIN project list` — List all registered projects (shows ID, name, path)
- `$ENKI_BIN project add <path> [--name <name>]` — Register a git repo as a project

### Task Management
- `$ENKI_BIN task list <project-id>` — List tasks for a project (shows ID, status, tier, title)
- `$ENKI_BIN task create <project-id> "<title>" [--description "<desc>"] [--tier light|standard|heavy]` — Create a single task
- `$ENKI_BIN task update-status <task-id> <status>` — Update task status (open, ready, running, done, failed, blocked)
- `$ENKI_BIN task retry <task-id>` — Retry a blocked task after a merge conflict (rebases branch onto main, re-merges)

### Template Execution (preferred for multi-step work)
- `$ENKI_BIN exec <project-id> <template-path> [--var key=value ...]` — Run a TOML workflow template

  Templates define named steps with dependencies, tiers, and variable substitution. Enki creates all tasks in the database, resolves the dependency graph, and automatically promotes tasks to ready as their dependencies complete.

  Example template (`feature.toml`):
  ```toml
  name = "feature"
  description = "Implement a feature"

  [vars.feature]
  description = "Feature name"
  required = true

  [[steps]]
  id = "design"
  title = "Design {{feature}}"
  description = "Produce a design doc for {{feature}}"
  tier = "heavy"

  [[steps]]
  id = "implement"
  title = "Implement {{feature}}"
  description = "Implement {{feature}} based on the design"
  needs = ["design"]
  tier = "standard"

  [[steps]]
  id = "test"
  title = "Test {{feature}}"
  description = "Write tests for {{feature}}"
  needs = ["design"]
  tier = "light"
  ```

  Run with: `$ENKI_BIN exec proj-xxx feature.toml --var feature="auth middleware"`

  Use `$ENKI_BIN exec` whenever the work has multiple steps or dependencies. For single one-off tasks, `$ENKI_BIN task create` + `$ENKI_BIN task update-status ready` is fine.

### Status
- `$ENKI_BIN status` — Show workspace overview (projects, task counts by status)

## Automatic Worker Spawning

When a task has status **ready**, enki will **automatically** spawn a worker agent to execute it. Workers run in isolated git worktrees, complete their task, and the branch is merged back to main. Dependent tasks are promoted to ready automatically when their dependencies complete — you do not need to set them ready manually.

**Workflow for a single task:**
1. `$ENKI_BIN task create` to register the task
2. `$ENKI_BIN task update-status <id> ready` to trigger the worker

**Workflow for multi-step work (preferred):**
1. Write a template TOML file describing the steps and dependencies
2. `$ENKI_BIN exec <project-id> template.toml --var ...` to launch the whole workflow

## Complexity Tiers

Assign a tier based on difficulty:
- **light** — Mechanical tasks: rename, format, simple boilerplate, docs
- **standard** (default) — Feature implementation, bug fixes, test writing
- **heavy** — Architectural decisions, ambiguous requirements, complex debugging

## Planning Guidelines

When the user asks you to implement something:

1. **Understand** — Read the request carefully. Ask clarifying questions if genuinely ambiguous.
2. **Explore** — Look at the relevant codebase files to understand the current state.
3. **Decompose** — Break the work into small, independently testable tasks. Each task should:
   - Have a clear title and description with acceptance criteria
   - Be completable by a single worker agent in one session
   - Include which files to look at and what conventions to follow
4. **Choose the right tool** — Use `enki exec` with a template for multi-step work with dependencies. Use `enki task create` for single isolated tasks.
5. **Report** — Summarize what you've planned.

Prefer more small tasks over fewer large tasks. Each task should change no more than a few files.

## Responding to the User

- Be concise and direct
- When you create tasks, show the user what you created
- When asked about status, run `$ENKI_BIN status` or `$ENKI_BIN task list` and report
- You can also read files, explore the codebase, and answer questions directly

Wait for the user's first message before taking any action."#
    )
}

/// Prompt for worker agents.
fn build_worker_prompt(title: &str, description: &str) -> String {
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

/// Completion signal from a worker's spawn_local task.
struct WorkerDone {
    task_id: Id,
    agent_id: Id,
    title: String,
    branch: String,
    bare_repo: PathBuf,
    worktree_path: PathBuf,
    result: Result<String, String>, // Ok(stop_reason) or Err(error)
}

async fn coordinator_loop(
    cwd: PathBuf,
    db_path: String,
    enki_bin: PathBuf,
    mut rx: mpsc::UnboundedReceiver<ToCoordinator>,
    tx: mpsc::UnboundedSender<FromCoordinator>,
) {
    tracing::info!(cwd = %cwd.display(), enki_bin = %enki_bin.display(), "coordinator loop started");

    let db = match Db::open(&db_path) {
        Ok(db) => {
            tracing::debug!(path = %db_path, "database opened");
            db
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to open database");
            let _ = tx.send(FromCoordinator::Error(format!("failed to open db: {e}")));
            return;
        }
    };

    let (worker_done_tx, mut worker_done_rx) = mpsc::unbounded_channel::<WorkerDone>();

    // Set ENKI_BIN so spawned agents can call `enki` regardless of PATH.
    let enki_env = {
        let mut env = std::collections::HashMap::new();
        env.insert("ENKI_BIN".to_string(), enki_bin.display().to_string());
        env
    };

    // Shared flag to suppress update forwarding during system prompt init.
    let forward_updates = std::rc::Rc::new(std::cell::Cell::new(false));
    let forward_flag = forward_updates.clone();

    // Coordinator agent manager — streams updates to TUI.
    let tx_updates = tx.clone();
    let mut coord_mgr = AgentManager::new();
    coord_mgr.set_env(enki_env.clone());
    coord_mgr.on_update(move |_session_id, update| {
        if !forward_flag.get() {
            return;
        }
        let msg = match update {
            SessionUpdate::Text(text) => FromCoordinator::Text(text),
            SessionUpdate::ToolCallStarted { title, .. } => FromCoordinator::ToolCall(title),
            SessionUpdate::ToolCallDone { id } => FromCoordinator::ToolCallDone(id),
            SessionUpdate::Plan(_) => return,
        };
        let _ = tx_updates.send(msg);
    });

    // Worker agent manager — streams activity updates (tool calls, thinking)
    // to the TUI for per-worker status display.
    let mut worker_mgr = AgentManager::new();
    worker_mgr.set_env(enki_env);

    // Maps ACP session_id → task_id so the worker callback can route updates.
    let session_task_map: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));

    // Track whether each worker is currently in a "thinking" state to avoid
    // spamming Thinking updates on every text token.
    let thinking_state: std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashSet::new()));

    {
        let map = session_task_map.clone();
        let thinking = thinking_state.clone();
        let tx_worker = tx.clone();
        worker_mgr.on_update(move |session_id, update| {
            let map = map.borrow();
            let Some(task_id) = map.get(session_id) else { return };
            let task_id = task_id.clone();

            let activity = match update {
                SessionUpdate::ToolCallStarted { title, .. } => {
                    // Worker started a tool — no longer "thinking"
                    thinking.borrow_mut().remove(session_id);
                    WorkerActivity::ToolStarted(title)
                }
                SessionUpdate::ToolCallDone { .. } => {
                    WorkerActivity::ToolDone
                }
                SessionUpdate::Text(_) => {
                    // Only send Thinking once per text burst
                    if !thinking.borrow_mut().insert(session_id.to_string()) {
                        return;
                    }
                    WorkerActivity::Thinking
                }
                SessionUpdate::Plan(_) => return,
            };

            let _ = tx_worker.send(FromCoordinator::WorkerUpdate { task_id, activity });
        });
    }

    // Resolve the ACP agent binary (installs on first use)
    let agent_cmd = match enki_core::agent_runtime::resolve() {
        Ok(cmd) => {
            tracing::info!(program = %cmd.program.display(), "agent binary resolved");
            cmd
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to resolve agent binary");
            let _ = tx.send(FromCoordinator::Error(format!(
                "failed to resolve agent binary: {e}"
            )));
            return;
        }
    };

    // Start the coordinator ACP session
    let args_ref: Vec<&str> = agent_cmd.args.iter().map(|s| s.as_str()).collect();
    let session_id = match coord_mgr
        .start_session(
            agent_cmd.program.to_str().unwrap(),
            &args_ref,
            cwd.clone(),
        )
        .await
    {
        Ok(id) => {
            tracing::info!(session_id = %id, "coordinator ACP session started");
            let _ = tx.send(FromCoordinator::Connected);
            id
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to start coordinator ACP session");
            let _ = tx.send(FromCoordinator::Error(format!(
                "failed to start coordinator: {e}"
            )));
            return;
        }
    };

    // Send the system prompt (updates suppressed during this phase).
    let system_prompt = build_system_prompt(&cwd);
    tracing::debug!(session_id, "sending system prompt");
    match coord_mgr.prompt(&session_id, &system_prompt).await {
        Ok(_) => {
            tracing::info!(session_id, "coordinator ready");
            forward_updates.set(true);
            let _ = tx.send(FromCoordinator::Ready);
        }
        Err(e) => {
            tracing::error!(session_id, error = %e, "system prompt failed");
            let _ = tx.send(FromCoordinator::Error(format!(
                "system prompt failed: {e}"
            )));
            return;
        }
    }

    let limits = TierLimits::default();
    let mut active_task_ids: Vec<String> = Vec::new();
    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(3));
    poll_interval.tick().await; // skip the immediate first tick

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    ToCoordinator::Prompt(text) => {
                        tracing::debug!(session_id, chars = text.len(), "user prompt received");
                        match coord_mgr.prompt(&session_id, &text).await {
                            Ok(stop_reason) => {
                                tracing::debug!(session_id, stop_reason, "coordinator prompt finished");
                                let _ = tx.send(FromCoordinator::Done(stop_reason));
                            }
                            Err(e) => {
                                tracing::error!(session_id, error = %e, "coordinator prompt error");
                                let _ = tx.send(FromCoordinator::Error(format!("prompt error: {e}")));
                            }
                        }
                    }
                    ToCoordinator::Shutdown => {
                        tracing::info!(session_id, "shutdown requested");
                        coord_mgr.kill_session(&session_id);
                        break;
                    }
                }
            }

            done = worker_done_rx.recv() => {
                let Some(done) = done else { continue };
                active_task_ids.retain(|id| id != &done.task_id.0);
                let _ = db.update_agent_status(&done.agent_id, AgentStatus::Dead);

                match done.result {
                    Ok(_) => {
                        tracing::info!(
                            task_id = %done.task_id.0, title = %done.title,
                            branch = %done.branch, "worker finished, merging branch"
                        );
                        let merge_result = try_merge(&done.bare_repo, &done.branch);

                        match merge_result {
                            Ok(()) => {
                                tracing::info!(task_id = %done.task_id.0, "branch merged successfully");
                                cleanup_worktree(&done.bare_repo, &done.worktree_path);
                                let _ = db.update_task_status(&done.task_id, TaskStatus::Done);
                                promote_unblocked_tasks(&db, &done.task_id);
                                let _ = tx.send(FromCoordinator::WorkerCompleted {
                                    task_id: done.task_id.0,
                                    title: done.title,
                                });
                            }
                            Err(WorktreeError::Conflict(_)) => {
                                // Worktree is intact — leave it for the user to retry.
                                tracing::warn!(
                                    task_id = %done.task_id.0, branch = %done.branch,
                                    worktree = %done.worktree_path.display(),
                                    "merge conflict: marking task blocked"
                                );
                                let _ = db.update_task_status(&done.task_id, TaskStatus::Blocked);
                                let _ = tx.send(FromCoordinator::WorkerConflicted {
                                    task_id: done.task_id.0,
                                    title: done.title,
                                    worktree: done.worktree_path.display().to_string(),
                                    branch: done.branch,
                                });
                            }
                            Err(e) => {
                                tracing::error!(
                                    task_id = %done.task_id.0, branch = %done.branch,
                                    error = %e, "merge failed"
                                );
                                cleanup_worktree(&done.bare_repo, &done.worktree_path);
                                let _ = db.update_task_status(&done.task_id, TaskStatus::Failed);
                                let _ = tx.send(FromCoordinator::WorkerFailed {
                                    task_id: done.task_id.0,
                                    title: done.title,
                                    error: format!("merge failed: {e}"),
                                });
                            }
                        }
                    }
                    Err(error) => {
                        tracing::error!(
                            task_id = %done.task_id.0, title = %done.title,
                            error, "worker failed"
                        );
                        cleanup_worktree(&done.bare_repo, &done.worktree_path);
                        let _ = db.update_task_status(&done.task_id, TaskStatus::Failed);
                        let _ = tx.send(FromCoordinator::WorkerFailed {
                            task_id: done.task_id.0,
                            title: done.title,
                            error,
                        });
                    }
                }
            }

            _ = poll_interval.tick() => {
                poll_ready_tasks(
                    &worker_mgr, &db, &limits, &mut active_task_ids,
                    &worker_done_tx, &tx, &session_task_map,
                ).await;
            }
        }
    }
}

/// Look for tasks with status "ready" and spawn workers, respecting tier concurrency limits.
async fn poll_ready_tasks(
    mgr: &AgentManager,
    db: &Db,
    limits: &TierLimits,
    active_task_ids: &mut Vec<String>,
    worker_done_tx: &mpsc::UnboundedSender<WorkerDone>,
    tx: &mpsc::UnboundedSender<FromCoordinator>,
    session_task_map: &std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, String>>>,
) {
    let projects = match db.list_projects() {
        Ok(p) => p,
        Err(_) => return,
    };

    // Count currently running tasks to enforce limits.
    // We track in-flight counts locally within this tick so multiple
    // tasks in the same poll don't all bypass the same limit.
    let (mut total, mut heavy, mut standard, mut light) = count_running_by_tier(db);

    for project in &projects {
        let tasks = match db.list_tasks(&project.id) {
            Ok(t) => t,
            Err(_) => continue,
        };

        for task in &tasks {
            if task.status != TaskStatus::Ready {
                continue;
            }
            if active_task_ids.contains(&task.id.0) {
                continue;
            }

            let tier = task.tier.unwrap_or(Tier::Standard);
            if total >= limits.max_workers {
                tracing::debug!("worker limit reached ({}), deferring task {}", limits.max_workers, task.id);
                continue;
            }
            let tier_ok = match tier {
                Tier::Heavy => heavy < limits.max_heavy,
                Tier::Standard => standard < limits.max_standard,
                Tier::Light => light < limits.max_light,
            };
            if !tier_ok {
                tracing::debug!(task_id = %task.id, "tier limit reached, deferring task");
                continue;
            }

            tracing::info!(
                task_id = %task.id.0, title = %task.title,
                project_id = %project.id.0, "spawning worker for ready task"
            );

            match spawn_worker(
                mgr, db, &project.id, &task.id, &task.title,
                task.description.as_deref().unwrap_or(""),
                worker_done_tx,
                session_task_map,
            )
            .await
            {
                Ok(()) => {
                    active_task_ids.push(task.id.0.clone());
                    // Update local counts so remaining tasks in this tick see current limits.
                    total += 1;
                    match tier {
                        Tier::Heavy => heavy += 1,
                        Tier::Standard => standard += 1,
                        Tier::Light => light += 1,
                    }
                    let _ = tx.send(FromCoordinator::WorkerSpawned {
                        task_id: task.id.0.clone(),
                        title: task.title.clone(),
                    });
                }
                Err(e) => {
                    tracing::error!(task_id = %task.id.0, error = %e, "failed to spawn worker");
                    let _ = db.update_task_status(&task.id, TaskStatus::Failed);
                    let _ = tx.send(FromCoordinator::WorkerFailed {
                        task_id: task.id.0.clone(),
                        title: task.title.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }
    }
}

/// Count tasks in `running` status across all projects, broken down by tier.
/// Returns (total, heavy, standard, light).
fn count_running_by_tier(db: &Db) -> (usize, usize, usize, usize) {
    let projects = match db.list_projects() {
        Ok(p) => p,
        Err(_) => return (0, 0, 0, 0),
    };

    let mut total = 0usize;
    let mut heavy = 0usize;
    let mut standard = 0usize;
    let mut light = 0usize;

    for project in &projects {
        if let Ok(tasks) = db.list_tasks(&project.id) {
            for task in &tasks {
                if task.status == TaskStatus::Running {
                    total += 1;
                    match task.tier.unwrap_or(Tier::Standard) {
                        Tier::Heavy => heavy += 1,
                        Tier::Standard => standard += 1,
                        Tier::Light => light += 1,
                    }
                }
            }
        }
    }

    (total, heavy, standard, light)
}

/// After a task completes, promote any tasks that were waiting on it to `ready`
/// if all their other dependencies are also done.
fn promote_unblocked_tasks(db: &Db, completed_task_id: &Id) {
    let dependents = match db.get_dependents(completed_task_id) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(task_id = %completed_task_id, error = %e, "failed to get dependents");
            return;
        }
    };

    for dep_id in &dependents {
        let dep_task = match db.get_task(dep_id) {
            Ok(t) => t,
            Err(_) => continue,
        };

        if dep_task.status != TaskStatus::Open {
            continue;
        }

        let all_deps = match db.get_dependencies(dep_id) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let all_done = all_deps.iter().all(|dep| {
            db.get_task(dep)
                .map(|t| t.status == TaskStatus::Done)
                .unwrap_or(false)
        });

        if all_done {
            tracing::info!(
                task_id = %dep_id, unblocked_by = %completed_task_id,
                "promoting task to ready"
            );
            let _ = db.update_task_status(dep_id, TaskStatus::Ready);
        }
    }
}

/// Spawn a worker ACP session for a task.
/// Syncs the bare repo, creates a worktree, starts a session, fires off the prompt.
async fn spawn_worker(
    mgr: &AgentManager,
    db: &Db,
    project_id: &Id,
    task_id: &Id,
    title: &str,
    description: &str,
    worker_done_tx: &mpsc::UnboundedSender<WorkerDone>,
    session_task_map: &std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, String>>>,
) -> anyhow::Result<()> {
    let project = db.get_project(project_id)?;

    let branch = format!("task/{}", task_id);
    let bare_path = PathBuf::from(&project.bare_repo);
    let worktree_dir = bare_path.parent().unwrap().join(".enki-worktrees");
    std::fs::create_dir_all(&worktree_dir)?;

    let wt_mgr = WorktreeManager::new(&bare_path)?;

    // Warn if source repo has uncommitted work (workers won't see it).
    match wt_mgr.check_source_status() {
        Ok(status) if !status.is_clean() => {
            tracing::warn!(
                task_id = %task_id.0,
                status = %status.summary(),
                "source repo has uncommitted work — workers won't see these changes"
            );
        }
        Err(e) => {
            tracing::debug!(task_id = %task_id.0, error = %e, "could not check source status");
        }
        _ => {}
    }

    // Sync bare repo so workers branch from current code.
    if let Err(e) = wt_mgr.sync() {
        tracing::warn!(task_id = %task_id.0, error = %e, "bare repo sync failed, proceeding with current state");
    } else {
        tracing::debug!(task_id = %task_id.0, "bare repo synced");
    }

    let worktree_path = wt_mgr.create(&branch, "origin/main", &worktree_dir)?;
    tracing::debug!(
        task_id = %task_id.0, branch,
        worktree = %worktree_path.display(), "worktree created"
    );

    db.update_task_status(task_id, TaskStatus::Running)?;
    let agent_id = Id::new("agent");
    db.assign_task(
        task_id,
        &agent_id,
        worktree_path.to_str().unwrap(),
        &branch,
    )?;

    let agent = Agent {
        id: agent_id.clone(),
        role: AgentRole::Worker,
        project_id: Some(project_id.clone()),
        acp_session: None,
        pid: None,
        status: AgentStatus::Busy,
        current_task: Some(task_id.clone()),
        started_at: chrono::Utc::now(),
        last_seen: None,
    };
    let _ = db.insert_agent(&agent);

    let agent_cmd = enki_core::agent_runtime::resolve()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let args_ref: Vec<&str> = agent_cmd.args.iter().map(|s| s.as_str()).collect();
    let session_id = mgr
        .start_session(
            agent_cmd.program.to_str().unwrap(),
            &args_ref,
            worktree_path.clone(),
        )
        .await?;
    tracing::debug!(session_id, task_id = %task_id.0, "worker ACP session started");

    // Register session→task mapping so the worker callback can route updates.
    session_task_map.borrow_mut().insert(session_id.clone(), task_id.0.clone());

    let prompt = build_worker_prompt(title, description);
    let mgr_clone = mgr.clone();
    let session_task_map = session_task_map.clone();
    let task_id = task_id.clone();
    let title = title.to_string();
    let branch_owned = branch.to_string();
    let bare_repo_owned = bare_path.to_path_buf();
    let worktree_owned = worktree_path;
    let done_tx = worker_done_tx.clone();
    tokio::task::spawn_local(async move {
        tracing::debug!(session_id, task_id = %task_id.0, "worker prompt started");
        let result = mgr_clone.prompt(&session_id, &prompt).await;
        tracing::debug!(session_id, task_id = %task_id.0, ok = result.is_ok(), "worker prompt returned");
        session_task_map.borrow_mut().remove(&session_id);
        mgr_clone.kill_session(&session_id);
        let _ = done_tx.send(WorkerDone {
            task_id,
            agent_id,
            title,
            branch: branch_owned,
            bare_repo: bare_repo_owned,
            worktree_path: worktree_owned,
            result: result.map_err(|e| e.to_string()),
        });
    });

    Ok(())
}

/// Merge a worker's branch into main within the bare repo.
/// Returns the `WorktreeError` directly so callers can distinguish `Conflict` from other errors.
fn try_merge(bare_repo: &Path, branch: &str) -> enki_core::worktree::Result<()> {
    let wt_mgr = WorktreeManager::new(bare_repo)?;
    wt_mgr.merge_branch(branch, "main")
}

/// Best-effort worktree cleanup (used on failure paths).
fn cleanup_worktree(bare_repo: &Path, worktree_path: &Path) {
    if let Ok(wt_mgr) = WorktreeManager::new(bare_repo) {
        let _ = wt_mgr.remove(worktree_path, true);
    }
}
