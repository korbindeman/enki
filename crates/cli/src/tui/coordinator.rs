use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use enki_acp::{AgentManager, SessionUpdate};
use enki_core::monitor;
use enki_core::orchestrator::{
    Command, Event, MergeResult, Orchestrator, WorkerOutcome, WorkerResult,
};
use enki_core::scheduler::Limits;
use enki_core::types::{Agent, AgentStatus, Id, MergeStatus, Tier};
use enki_core::worktree::CopyManager;
use tokio::sync::mpsc;

/// Messages sent from the TUI to the coordinator thread.
pub enum ToCoordinator {
    Prompt(String),
    Interrupt,
    Shutdown,
    /// Stop all running workers immediately.
    #[allow(dead_code)]
    StopAll,
}

/// Activity update from a running worker agent.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum WorkerActivity {
    ToolStarted(String),
    ToolDone,
    Thinking,
}

/// Messages sent from the coordinator thread back to the TUI.
#[derive(Debug)]
pub enum FromCoordinator {
    Connected,
    Ready,
    Text(String),
    ToolCall(String),
    #[allow(dead_code)]
    ToolCallDone(String),
    Done(String),
    WorkerSpawned { task_id: String, title: String, tier: String },
    WorkerCompleted { task_id: String, title: String },
    WorkerFailed {
        task_id: String,
        title: String,
        error: String,
    },
    #[allow(dead_code)]
    WorkerUpdate {
        task_id: String,
        activity: WorkerActivity,
    },
    #[allow(dead_code)]
    MergeQueued {
        mr_id: String,
        task_id: String,
        branch: String,
    },
    #[allow(dead_code)]
    MergeLanded {
        mr_id: String,
        task_id: String,
        branch: String,
    },
    #[allow(dead_code)]
    MergeConflicted {
        mr_id: String,
        task_id: String,
        branch: String,
    },
    #[allow(dead_code)]
    MergeFailed {
        mr_id: String,
        task_id: String,
        branch: String,
        reason: String,
    },
    #[allow(dead_code)]
    MergeProgress {
        mr_id: String,
        task_id: String,
        branch: String,
        status: String,
    },
    WorkerReport { task_id: String, status: String },
    Mail {
        from: String,
        to: String,
        subject: String,
        priority: String,
    },
    AllStopped { count: usize },
    WorkerCount(usize),
    Interrupted,
    Error(String),
}

/// Handle held by the TUI to communicate with the coordinator.
pub struct CoordinatorHandle {
    pub tx: mpsc::UnboundedSender<ToCoordinator>,
    pub rx: mpsc::UnboundedReceiver<FromCoordinator>,
}

/// Spawn the coordinator on a dedicated OS thread with its own tokio runtime + LocalSet.
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

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

fn build_system_prompt(cwd: &std::path::Path) -> String {
    let cwd_display = cwd.display();

    format!(
        r#"You are the **coordinator** for enki, a multi-agent coding orchestrator.

## Your Role

You plan work, decompose user requests into tasks, assign complexity tiers, and track progress. You are the user's primary interface for managing a codebase with multiple AI worker agents.

## Current Workspace

- Working directory: `{cwd_display}`

## Available MCP Tools

You have access to enki tools via the **enki MCP server**. Use these tools directly — do not shell out to the CLI.

### Execution Management
- `enki_execution_create(steps)` — Create a multi-step execution with dependency ordering. Each step has `id`, `title`, `description`, `tier`, and `needs` (list of step IDs it depends on). Steps with no dependencies start immediately; others wait. **Use this for any work involving 2+ related steps.**

### Simple Task Creation
- `enki_task_create(title, description?, tier?)` — Create a single standalone task. Use only for isolated, independent tasks (quick fixes, one-off changes). For multi-step work, use `enki_execution_create` instead.

### Status & Monitoring
- `enki_task_list` — List all tasks (shows ID, status, tier, title)
- `enki_task_update_status(task_id, status)` — Update task status (open, ready, running, done, failed, blocked)
- `enki_status` — Show task counts by status
- `enki_task_retry(task_id)` — Retry a failed task within its execution. Resets it to ready, unblocks sibling tasks, and restores the execution. **Use this instead of recreating an entire execution when only one step failed.**
- `enki_stop_all` — Stop all running workers immediately. Use when the user asks to stop, halt, or cancel all tasks.

## Automatic Worker Spawning

When a task has status **ready**, enki will **automatically** spawn a worker agent to execute it. Workers run in isolated git worktrees, complete their task, and a programmatic refinery rebases and merges the branch back to main. Dependent tasks are promoted to ready automatically when their dependencies complete — you do not need to set them ready manually.

## Complexity Tiers

Assign a tier based on difficulty:
- **light** — Mechanical tasks: rename, format, simple boilerplate, stubs, docs
- **standard** (default) — Feature implementation, bug fixes, test writing
- **heavy** — Architectural decisions, ambiguous requirements, complex debugging

## Planning Guidelines

When the user asks you to implement something:

1. **Understand** — Read the request carefully. Ask clarifying questions if genuinely ambiguous.
2. **Explore** — Look at the relevant codebase files to understand the current state.
3. **Decompose** — Break the work into steps with clear dependencies.
4. **Create execution** — Use `enki_execution_create` with all steps and their dependency relationships.
5. **Report** — Summarize what you've planned: steps, dependencies, and tiers.

### Scaffold-First Pattern

For greenfield projects, major new features, or work that establishes a new directory/module structure, **always include a scaffold step** as the first step:

- The scaffold step creates directory structure, stub files with interfaces/types, config files, and any shared contracts that parallel workers need
- All implementation steps should depend on the scaffold step
- The scaffold step should be **light** tier — it's mechanical work (mkdir, create files, define interfaces)
- Implementation steps then run in parallel after the scaffold completes

Example:
```
scaffold (light, no deps) → dirs, stubs, interfaces
  ├── feature-a (standard, needs: scaffold)
  ├── feature-b (standard, needs: scaffold)
  └── feature-c (standard, needs: scaffold)
```

**Skip the scaffold step** when:
- The project already has established structure and the tasks work within existing modules
- You're making a bug fix or small enhancement
- There's only a single task to do

### Task Design

- Prefer more small tasks over fewer large ones
- Each task should change no more than a few files
- Each task description should include acceptance criteria and which files to look at
- Workers cannot see each other's work — only the output from completed upstream dependencies

## Handling Failures

When a task or merge fails:
- **Use `enki_task_retry`** to retry the failed step. This preserves the execution and its sibling tasks — blocked dependents are automatically unblocked when the retried task succeeds.
- **Do NOT recreate the entire execution.** The existing tasks, dependencies, and any completed work are preserved by retry.
- Only create a new execution if the original plan was fundamentally wrong (e.g., wrong decomposition, missing steps).

## Merging

A programmatic refinery rebases and merges completed task branches. If a merge fails (conflict, verification failure), the task will be marked failed and you'll be notified.

## Responding to the User

- Be concise and direct
- When you create executions, show the step graph: what runs first, what runs in parallel, what depends on what
- When asked about status, use `enki_status` or `enki_task_list` and report
- You can also read files, explore the codebase, and answer questions directly

Wait for the user's first message before taking any action."#
    )
}

fn build_worker_prompt(
    title: &str,
    description: &str,
    upstream_outputs: &[(String, String)],
) -> String {
    let mut prompt = format!(
        r#"You are a focused coding agent working on a single task.

TASK: {title}
{description}"#
    );

    if !upstream_outputs.is_empty() {
        prompt.push_str("\n\n## Context from upstream steps\n");
        for (step_title, output) in upstream_outputs {
            prompt.push_str(&format!("\n### {step_title} (completed)\n{output}\n"));
        }
    }

    prompt.push_str(
        r#"

Make focused changes. Only modify files relevant to your task. Commit when done.

Use the enki_worker_report tool to report what you're doing at each major phase of your work (e.g. "analyzing codebase", "implementing changes", "running tests").

When you finish, output a summary between [OUTPUT] and [/OUTPUT] tags:

[OUTPUT]
Brief summary of changes made, files modified, decisions taken.
[/OUTPUT]"#,
    );

    prompt
}

fn extract_output(result: &str) -> Option<String> {
    if let Some(start) = result.find("[OUTPUT]") {
        let content_start = start + "[OUTPUT]".len();
        if let Some(end) = result[content_start..].find("[/OUTPUT]") {
            let output = result[content_start..content_start + end].trim();
            if !output.is_empty() {
                return Some(output.to_string());
            }
        }
    }
    if result.len() > 10 {
        let start = result.len().saturating_sub(500);
        Some(result[start..].to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Internal channel types
// ---------------------------------------------------------------------------

struct WorkerDone {
    task_id: Id,
    agent_id: Id,
    session_id: Option<String>,
    title: String,
    branch: String,
    copy_path: PathBuf,
    result: Result<String, String>,
    execution_id: Option<Id>,
    step_id: Option<String>,
}

struct MergerDone {
    merge_request_id: Id,
    outcome: enki_core::refinery::MergeOutcome,
}

// ---------------------------------------------------------------------------
// Worker tracker (replaces 4 Rc<RefCell<HashMap>>)
// ---------------------------------------------------------------------------

struct WorkerTracker {
    session_to_task: HashMap<String, String>,
    last_activity: HashMap<String, Instant>,
    current_tool: HashMap<String, String>,
    thinking: HashSet<String>,
}

impl WorkerTracker {
    fn new() -> Self {
        Self {
            session_to_task: HashMap::new(),
            last_activity: HashMap::new(),
            current_tool: HashMap::new(),
            thinking: HashSet::new(),
        }
    }

    fn register(&mut self, session_id: String, task_id: String) {
        self.session_to_task
            .insert(session_id.clone(), task_id);
        self.last_activity.insert(session_id, Instant::now());
    }

    fn remove(&mut self, session_id: &str) {
        self.session_to_task.remove(session_id);
        self.last_activity.remove(session_id);
        self.current_tool.remove(session_id);
        self.thinking.remove(session_id);
    }

    /// Build the worker list for MonitorTick: (session_id, task_id, last_activity).
    fn worker_list(&self) -> Vec<(String, String, Instant)> {
        self.last_activity
            .iter()
            .filter_map(|(sid, last)| {
                let tid = self.session_to_task.get(sid)?.clone();
                Some((sid.clone(), tid, *last))
            })
            .collect()
    }

    fn worker_count(&self) -> usize {
        self.session_to_task.len()
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

async fn coordinator_loop(
    cwd: PathBuf,
    db_path: String,
    enki_bin: PathBuf,
    mut rx: mpsc::UnboundedReceiver<ToCoordinator>,
    tx: mpsc::UnboundedSender<FromCoordinator>,
) {
    tracing::info!(cwd = %cwd.display(), enki_bin = %enki_bin.display(), "coordinator loop started");

    let db = match enki_core::db::Db::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!("failed to open db: {e}")));
            return;
        }
    };

    let mut orch = Orchestrator::new(db, Limits::default());

    let (worker_done_tx, mut worker_done_rx) = mpsc::unbounded_channel::<WorkerDone>();

    // Env vars for spawned agents.
    let enki_dir = match crate::commands::enki_dir() {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!("failed to find .enki dir: {e}")));
            return;
        }
    };
    let enki_env = {
        let mut env = HashMap::new();
        env.insert("ENKI_BIN".to_string(), enki_bin.display().to_string());
        env.insert("ENKI_DIR".to_string(), enki_dir.display().to_string());
        env
    };

    // Set up events directory for signal files.
    let events_dir = enki_dir.join("events");
    orch.set_events_dir(events_dir);

    // Coordinator agent manager.
    let forward_updates = std::rc::Rc::new(std::cell::Cell::new(false));
    let forward_flag = forward_updates.clone();
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

    // Worker agent manager with consolidated tracker.
    let tracker = std::rc::Rc::new(std::cell::RefCell::new(WorkerTracker::new()));
    let mut worker_mgr = AgentManager::new();
    worker_mgr.set_env(enki_env);

    {
        let tracker_cb = tracker.clone();
        let tx_worker = tx.clone();
        worker_mgr.on_update(move |session_id, update| {
            let mut t = tracker_cb.borrow_mut();
            t.last_activity
                .insert(session_id.to_string(), Instant::now());

            let Some(task_id) = t.session_to_task.get(session_id).cloned() else {
                return;
            };

            let activity = match update {
                SessionUpdate::ToolCallStarted { title, .. } => {
                    t.thinking.remove(session_id);
                    t.current_tool
                        .insert(session_id.to_string(), title.clone());
                    WorkerActivity::ToolStarted(title)
                }
                SessionUpdate::ToolCallDone { .. } => {
                    t.current_tool.remove(session_id);
                    WorkerActivity::ToolDone
                }
                SessionUpdate::Text(_) => {
                    if !t.thinking.insert(session_id.to_string()) {
                        return;
                    }
                    WorkerActivity::Thinking
                }
                SessionUpdate::Plan(_) => return,
            };

            let _ = tx_worker.send(FromCoordinator::WorkerUpdate { task_id, activity });
        });
    }

    // Resolve agent binary.
    let agent_cmd = match enki_core::agent_runtime::resolve() {
        Ok(cmd) => cmd,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "failed to resolve agent binary: {e}"
            )));
            return;
        }
    };

    // MCP server configs.
    let planner_mcp = vec![enki_acp::acp_schema::McpServer::Stdio(
        enki_acp::acp_schema::McpServerStdio::new("enki", &enki_bin)
            .args(vec!["mcp".into(), "--role".into(), "planner".into()]),
    )];
    // Start coordinator ACP session.
    let args_ref: Vec<&str> = agent_cmd.args.iter().map(|s| s.as_str()).collect();
    let session_id = match coord_mgr
        .start_session_with_mcp(
            agent_cmd.program.to_str().unwrap(),
            &args_ref,
            cwd.clone(),
            planner_mcp.clone(),
        )
        .await
    {
        Ok(id) => {
            let _ = tx.send(FromCoordinator::Connected);
            id
        }
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "failed to start coordinator: {e}"
            )));
            return;
        }
    };

    // Send system prompt (updates suppressed during this phase).
    let system_prompt = build_system_prompt(&cwd);
    match coord_mgr.prompt(&session_id, &system_prompt).await {
        Ok(_) => {
            forward_updates.set(true);
            let _ = tx.send(FromCoordinator::Ready);
        }
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "system prompt failed: {e}"
            )));
            return;
        }
    }

    let mut infra_broken = false;
    let mut pending_events: Vec<String> = Vec::new();
    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(3));
    poll_interval.tick().await;

    let project_root = crate::commands::project_root().unwrap_or_default();
    let copies_dir = crate::commands::copies_dir().unwrap_or_default();

    // Crash recovery via orchestrator.
    let events = orch.handle(Command::Recover);
    process_events(
        events,
        &mut orch,
        &worker_mgr,
        &tracker,
        &worker_done_tx,
        &tx,
        &mut pending_events,
        &enki_bin,
        &mut infra_broken,
        &project_root,
        &copies_dir,
    )
    .await;

    // Prompt management.
    let (prompt_done_tx, mut prompt_done_rx) =
        mpsc::unbounded_channel::<(u64, Result<String, String>)>();
    let mut prompt_generation: u64 = 0;
    let mut active_prompt: Option<tokio::task::JoinHandle<()>> = None;

    // Refinery state.
    let (merger_done_tx, mut merger_done_rx) = mpsc::unbounded_channel::<MergerDone>();
    let mut merge_in_progress = false;
    let mut last_merge_statuses: HashMap<String, MergeStatus> = HashMap::new();

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    ToCoordinator::Prompt(text) => {
                        if let Some(handle) = active_prompt.take() {
                            let _ = coord_mgr.cancel(&session_id).await;
                            handle.abort();
                            let _ = tx.send(FromCoordinator::Interrupted);
                        }

                        let full_text = if pending_events.is_empty() {
                            text
                        } else {
                            let events_text = pending_events.drain(..).collect::<Vec<_>>().join("\n");
                            format!("[worker status updates]\n{events_text}\n\n[user message]\n{text}")
                        };

                        prompt_generation += 1;
                        let generation = prompt_generation;
                        let mgr = coord_mgr.clone();
                        let sid = session_id.clone();
                        let done_tx = prompt_done_tx.clone();
                        active_prompt = Some(tokio::task::spawn_local(async move {
                            let result = mgr.prompt(&sid, &full_text).await;
                            let _ = done_tx.send((generation, result.map_err(|e| e.to_string())));
                        }));
                    }
                    ToCoordinator::Interrupt => {
                        if let Some(handle) = active_prompt.take() {
                            let _ = coord_mgr.cancel(&session_id).await;
                            handle.abort();
                            let _ = tx.send(FromCoordinator::Interrupted);
                        }
                    }
                    ToCoordinator::Shutdown => {
                        if let Some(handle) = active_prompt.take() {
                            handle.abort();
                        }
                        coord_mgr.kill_session(&session_id);
                        break;
                    }
                    ToCoordinator::StopAll => {
                        // Kill all worker ACP sessions.
                        let t = tracker.borrow();
                        for sid in t.session_to_task.keys() {
                            worker_mgr.kill_session(sid);
                        }
                        drop(t);
                        tracker.borrow_mut().session_to_task.clear();
                        tracker.borrow_mut().last_activity.clear();
                        tracker.borrow_mut().current_tool.clear();

                        let events = orch.handle(Command::StopAll);
                        process_events(
                            events, &mut orch, &worker_mgr, &tracker, &worker_done_tx,
                            &tx, &mut pending_events, &enki_bin, &mut infra_broken, &project_root, &copies_dir,
                        ).await;
                    }
                }
            }

            result = prompt_done_rx.recv() => {
                if let Some((generation, result)) = result {
                    if generation != prompt_generation { continue; }
                    active_prompt = None;
                    match result {
                        Ok(stop_reason) => {
                            let _ = tx.send(FromCoordinator::Done(stop_reason));
                        }
                        Err(e) => {
                            let _ = tx.send(FromCoordinator::Error(format!("prompt error: {e}")));
                        }
                    }
                }
            }

            done = worker_done_rx.recv() => {
                let Some(done) = done else { continue };

                // Clean up tracker.
                if let Some(sid) = done.session_id.as_ref() {
                    tracker.borrow_mut().remove(sid);
                    orch.session_ended(sid);
                }
                let _ = orch.db().update_task_activity(&done.task_id, None);
                let _ = orch.db().update_agent_status(&done.agent_id, AgentStatus::Dead);

                // Process infrastructure and convert to WorkerResult.
                let worker_result = process_worker_done(done, &project_root, &copies_dir);
                let events = orch.handle(Command::WorkerDone(worker_result));
                process_events(
                    events, &mut orch, &worker_mgr, &tracker, &worker_done_tx,
                    &tx, &mut pending_events, &enki_bin, &mut infra_broken, &project_root, &copies_dir,
                ).await;
            }

            done = merger_done_rx.recv() => {
                let Some(done) = done else { continue };
                merge_in_progress = false;

                let events = orch.handle(Command::MergeDone(MergeResult {
                    mr_id: done.merge_request_id,
                    outcome: done.outcome,
                }));

                // After merge, clean up worker copies.
                for event in &events {
                    if let Event::MergeLanded { mr_id, .. } = event {
                        let mr_id_obj = Id(mr_id.clone());
                        if let Ok(mr) = orch.db().get_merge_request(&mr_id_obj) {
                            // Extract task_id from branch name (task/<task_id>).
                            if let Some(task_id) = mr.branch.strip_prefix("task/") {
                                let copy_path = copies_dir.join(task_id);
                                let _ = std::fs::remove_dir_all(&copy_path);
                            }
                        }
                    }
                }

                process_events(
                    events, &mut orch, &worker_mgr, &tracker, &worker_done_tx,
                    &tx, &mut pending_events, &enki_bin, &mut infra_broken, &project_root, &copies_dir,
                ).await;
            }

            _ = poll_interval.tick() => {
                // Check for external stop signal.
                let stop_file = enki_dir.join("stop");
                if stop_file.exists() {
                    let _ = std::fs::remove_file(&stop_file);
                    let t = tracker.borrow();
                    for sid in t.session_to_task.keys() {
                        worker_mgr.kill_session(sid);
                    }
                    drop(t);
                    tracker.borrow_mut().session_to_task.clear();
                    tracker.borrow_mut().last_activity.clear();
                    tracker.borrow_mut().current_tool.clear();

                    let events = orch.handle(Command::StopAll);
                    process_events(
                        events, &mut orch, &worker_mgr, &tracker, &worker_done_tx,
                        &tx, &mut pending_events, &enki_bin, &mut infra_broken, &project_root, &copies_dir,
                    ).await;
                }

                // Discover new work from DB (external MCP calls) + check signal files.
                if !infra_broken {
                    let events = orch.handle(Command::CheckSignals);
                    process_events(
                        events, &mut orch, &worker_mgr, &tracker, &worker_done_tx,
                        &tx, &mut pending_events, &enki_bin, &mut infra_broken, &project_root, &copies_dir,
                    ).await;

                    let events = orch.handle(Command::DiscoverFromDb);
                    process_events(
                        events, &mut orch, &worker_mgr, &tracker, &worker_done_tx,
                        &tx, &mut pending_events, &enki_bin, &mut infra_broken, &project_root, &copies_dir,
                    ).await;
                }

                // Snapshot worker activity to DB.
                {
                    let t = tracker.borrow();
                    for (session_id, tool_name) in &t.current_tool {
                        if let Some(task_id_str) = t.session_to_task.get(session_id) {
                            let task_id = Id(task_id_str.clone());
                            let _ = orch.db().update_task_activity(&task_id, Some(tool_name));
                        }
                    }
                }

                // Worker count sync.
                let _ = tx.send(FromCoordinator::WorkerCount(tracker.borrow().worker_count()));

                // Monitor patrol.
                let workers = tracker.borrow().worker_list();
                let events = orch.handle(Command::MonitorTick { workers });
                for event in &events {
                    if let Event::MonitorCancel { session_id, task_id, stale_secs } = event {
                        tracing::warn!(session_id, task_id, stale_secs, "monitor: worker stale, cancelling");
                        let _ = worker_mgr.cancel(session_id).await;
                        // Update agent status.
                        if let Ok(tasks) = orch.db().list_tasks() {
                            if let Some(task) = tasks.iter().find(|t| t.id.0 == *task_id) {
                                if let Some(agent_id) = &task.assigned_to {
                                    let _ = orch.db().update_agent_status(agent_id, AgentStatus::Stuck);
                                }
                            }
                        }
                        pending_events.push(format!(
                            "- Task ({task_id}) worker stuck (no activity for {stale_secs}s) — cancel sent"
                        ));
                    }
                    if let Event::MonitorEscalation(msg) = event {
                        pending_events.push(msg.clone());
                    }
                }

                // Dispatch queued merge requests.
                if !merge_in_progress {
                    try_dispatch_merge(
                        orch.db(), &db_path, &project_root, &copies_dir,
                        &merger_done_tx, &mut merge_in_progress, &tx,
                    );
                }

                // Merge progress polling.
                if let Ok(active_mrs) = orch.db().get_active_merge_requests() {
                    let mut current_ids: HashSet<String> = HashSet::new();
                    for mr in &active_mrs {
                        current_ids.insert(mr.id.0.clone());
                        let changed = match last_merge_statuses.get(&mr.id.0) {
                            Some(prev) => *prev != mr.status,
                            None => mr.status != MergeStatus::Queued,
                        };
                        if changed {
                            let _ = tx.send(FromCoordinator::MergeProgress {
                                mr_id: mr.id.0.clone(),
                                task_id: mr.task_id.0.clone(),
                                branch: mr.branch.clone(),
                                status: mr.status.as_str().to_string(),
                            });
                        }
                        last_merge_statuses.insert(mr.id.0.clone(), mr.status);
                    }
                    last_merge_statuses.retain(|k, _| current_ids.contains(k));
                }

                // Reconcile: catch missed merge signals.
                if !infra_broken {
                    let events = orch.reconcile_merges();
                    process_events(
                        events, &mut orch, &worker_mgr, &tracker, &worker_done_tx,
                        &tx, &mut pending_events, &enki_bin, &mut infra_broken, &project_root, &copies_dir,
                    ).await;
                }
            }
        }

        // Flush pending events to coordinator agent when idle.
        if active_prompt.is_none() && !pending_events.is_empty() {
            let events_text = pending_events.drain(..).collect::<Vec<_>>().join("\n");
            let msg = format!("[worker status updates]\n{events_text}");
            prompt_generation += 1;
            let generation = prompt_generation;
            let mgr = coord_mgr.clone();
            let sid = session_id.clone();
            let done_tx = prompt_done_tx.clone();
            active_prompt = Some(tokio::task::spawn_local(async move {
                let result = mgr.prompt(&sid, &msg).await;
                let _ = done_tx.send((generation, result.map_err(|e| e.to_string())));
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Event processing
// ---------------------------------------------------------------------------

/// Process orchestrator events: spawn workers, forward TUI messages, queue status updates.
async fn process_events(
    initial_events: Vec<Event>,
    orch: &mut Orchestrator,
    worker_mgr: &AgentManager,
    tracker: &std::rc::Rc<std::cell::RefCell<WorkerTracker>>,
    worker_done_tx: &mpsc::UnboundedSender<WorkerDone>,
    tx: &mpsc::UnboundedSender<FromCoordinator>,
    pending_events: &mut Vec<String>,
    enki_bin: &Path,
    infra_broken: &mut bool,
    project_root: &Path,
    copies_dir: &Path,
) {
    let mut events = initial_events;

    // Process in a loop: spawn failures can produce cascading events.
    while !events.is_empty() {
        let batch = std::mem::take(&mut events);
        for event in batch {
            match event {
                Event::SpawnWorker {
                    task_id,
                    title,
                    description,
                    tier,
                    execution_id,
                    step_id,
                    upstream_outputs,
                } => {
                    if *infra_broken {
                        // Infrastructure failed — tell orchestrator this task failed.
                        let more = orch.handle(Command::WorkerDone(WorkerResult {
                            task_id: task_id.clone(),
                            execution_id: Some(execution_id),
                            step_id: Some(step_id),
                            title,
                            branch: String::new(),
                            outcome: WorkerOutcome::Failed {
                                error: "infrastructure broken".into(),
                            },
                        }));
                        events.extend(more);
                        continue;
                    }

                    match spawn_worker(
                        worker_mgr,
                        orch,
                        &task_id,
                        &title,
                        &description,
                        tier,
                        &execution_id,
                        &step_id,
                        &upstream_outputs,
                        worker_done_tx,
                        tracker,
                        enki_bin,
                        project_root,
                        copies_dir,
                    )
                    .await
                    {
                        Ok(()) => {
                            let _ = tx.send(FromCoordinator::WorkerSpawned {
                                task_id: task_id.0.clone(),
                                title: title.clone(),
                                tier: tier.as_str().to_string(),
                            });
                            pending_events.push(format!(
                                "- Worker spawned for \"{}\" ({})",
                                title, task_id
                            ));
                        }
                        Err(e) => {
                            let error = e.to_string();
                            tracing::error!(task_id = %task_id, error = %error, "failed to spawn worker");

                            // Check if this is an infrastructure failure.
                            if error.contains("cp -Rc failed") || error.contains("not found") {
                                *infra_broken = true;
                            }

                            let _ = tx.send(FromCoordinator::WorkerFailed {
                                task_id: task_id.0.clone(),
                                title: title.clone(),
                                error: error.clone(),
                            });
                            pending_events.push(format!(
                                "- Task \"{}\" ({}) failed to spawn: {}",
                                title, task_id, error
                            ));

                            // Tell orchestrator the worker failed.
                            let more = orch.handle(Command::WorkerDone(WorkerResult {
                                task_id,
                                execution_id: Some(execution_id),
                                step_id: Some(step_id),
                                title,
                                branch: String::new(),
                                outcome: WorkerOutcome::Failed { error },
                            }));
                            events.extend(more);
                        }
                    }
                }
                Event::KillSession { session_id } => {
                    worker_mgr.kill_session(&session_id);
                    tracker.borrow_mut().remove(&session_id);
                    orch.session_ended(&session_id);
                }
                Event::QueueMerge(mr) => {
                    let _ = tx.send(FromCoordinator::MergeQueued {
                        mr_id: mr.id.0.clone(),
                        task_id: mr.task_id.0.clone(),
                        branch: mr.branch.clone(),
                    });
                    pending_events.push(format!(
                        "- Task \"{}\" completed, merge {} queued",
                        mr.task_id, mr.id
                    ));
                }
                Event::WorkerCompleted { task_id, title } => {
                    let _ = tx.send(FromCoordinator::WorkerCompleted {
                        task_id: task_id.clone(),
                        title: title.clone(),
                    });
                }
                Event::WorkerFailed {
                    task_id,
                    title,
                    error,
                } => {
                    let _ = tx.send(FromCoordinator::WorkerFailed {
                        task_id: task_id.clone(),
                        title: title.clone(),
                        error: error.clone(),
                    });
                    pending_events.push(format!("- Task \"{title}\" ({task_id}) failed: {error}"));
                }
                Event::MergeLanded { mr_id, task_id, branch } => {
                    let _ = tx.send(FromCoordinator::MergeLanded {
                        mr_id: mr_id.clone(),
                        task_id: task_id.clone(),
                        branch: branch.clone(),
                    });
                    pending_events
                        .push(format!("- Merge {mr_id} landed: task {task_id} merged to main"));
                }
                Event::MergeConflicted { mr_id, task_id, branch } => {
                    let _ = tx.send(FromCoordinator::MergeConflicted {
                        mr_id: mr_id.clone(),
                        task_id: task_id.clone(),
                        branch: branch.clone(),
                    });
                    pending_events
                        .push(format!("- Merge {mr_id} conflicted — task {task_id} needs resolution"));
                }
                Event::MergeFailed {
                    mr_id,
                    task_id,
                    branch,
                    reason,
                } => {
                    let _ = tx.send(FromCoordinator::MergeFailed {
                        mr_id: mr_id.clone(),
                        task_id: task_id.clone(),
                        branch: branch.clone(),
                        reason: reason.clone(),
                    });
                    pending_events.push(format!("- Merge {mr_id} failed: {reason}"));
                }
                Event::ExecutionComplete { execution_id } => {
                    pending_events
                        .push(format!("- Execution {execution_id} completed successfully"));
                }
                Event::ExecutionFailed { execution_id } => {
                    pending_events.push(format!("- Execution {execution_id} failed"));
                }
                Event::AllStopped { count } => {
                    let _ = tx.send(FromCoordinator::AllStopped { count });
                }
                Event::MonitorCancel { .. } | Event::MonitorEscalation(_) => {
                    // Handled directly in the poll tick branch.
                }
                Event::TaskRetrying {
                    task_id,
                    title,
                    attempt,
                    max,
                } => {
                    pending_events.push(format!(
                        "- Task \"{title}\" ({task_id}) timed out — retrying ({attempt}/{max})"
                    ));
                }
                Event::StatusMessage(msg) => {
                    pending_events.push(msg);
                }
                Event::WorkerReport { task_id, status } => {
                    let _ = tx.send(FromCoordinator::WorkerReport { task_id, status });
                }
                Event::Mail { from, to, subject, priority, .. } => {
                    // Forward to TUI for display; also queue for coordinator if addressed to it.
                    let _ = tx.send(FromCoordinator::Mail {
                        from: from.clone(),
                        to: to.clone(),
                        subject: subject.clone(),
                        priority: priority.clone(),
                    });
                    if to == "coordinator" {
                        pending_events.push(format!(
                            "- Mail from {from}: \"{subject}\" [priority: {priority}]"
                        ));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker done processing (infrastructure layer)
// ---------------------------------------------------------------------------

/// Convert a raw WorkerDone (from the channel) into an orchestrator WorkerResult.
/// Handles auto-commit, change detection, and copy cleanup.
fn process_worker_done(done: WorkerDone, project_root: &Path, _copies_dir: &Path) -> WorkerResult {
    let copy_mgr = CopyManager::new(project_root.to_path_buf(), _copies_dir.to_path_buf());

    match done.result {
        Ok(ref stop_reason) => {
            // Auto-commit uncommitted changes in the copy.
            let msg = format!("enki: {}", done.title);
            copy_mgr.commit_copy(&done.copy_path, &msg);

            // Check for actual changes.
            let has_changes = copy_mgr.has_changes(&done.copy_path, &done.branch);

            if !has_changes {
                tracing::warn!(
                    task_id = %done.task_id, title = %done.title,
                    "worker completed but copy has no changes"
                );
                let _ = copy_mgr.remove_copy(&done.copy_path);
                return WorkerResult {
                    task_id: done.task_id,
                    execution_id: done.execution_id,
                    step_id: done.step_id,
                    title: done.title,
                    branch: done.branch,
                    outcome: WorkerOutcome::NoChanges,
                };
            }

            // Keep copy for refinery to fetch from.
            let output = extract_output(stop_reason);
            WorkerResult {
                task_id: done.task_id,
                execution_id: done.execution_id,
                step_id: done.step_id,
                title: done.title,
                branch: done.branch,
                outcome: WorkerOutcome::Success { output },
            }
        }
        Err(ref error) => {
            tracing::error!(
                task_id = %done.task_id, title = %done.title,
                error, "worker failed"
            );
            let _ = copy_mgr.remove_copy(&done.copy_path);
            WorkerResult {
                task_id: done.task_id,
                execution_id: done.execution_id,
                step_id: done.step_id,
                title: done.title,
                branch: done.branch,
                outcome: WorkerOutcome::Failed {
                    error: error.clone(),
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker spawning
// ---------------------------------------------------------------------------

async fn spawn_worker(
    mgr: &AgentManager,
    orch: &mut Orchestrator,
    task_id: &Id,
    title: &str,
    description: &str,
    tier: Tier,
    execution_id: &Id,
    step_id: &str,
    upstream_outputs: &[(String, String)],
    worker_done_tx: &mpsc::UnboundedSender<WorkerDone>,
    tracker: &std::rc::Rc<std::cell::RefCell<WorkerTracker>>,
    enki_bin: &Path,
    project_root: &Path,
    copies_dir: &Path,
) -> anyhow::Result<()> {
    let branch = format!("task/{}", task_id);
    let copy_mgr = CopyManager::new(project_root.to_path_buf(), copies_dir.to_path_buf());
    let copy_path = copy_mgr.create_copy(&task_id.0)?;

    let agent_id = Id::new("agent");
    orch.db().assign_task(
        task_id,
        &agent_id,
        copy_path.to_str().unwrap(),
        &branch,
    )?;

    let agent = Agent {
        id: agent_id.clone(),
        acp_session: None,
        pid: None,
        status: AgentStatus::Busy,
        current_task: Some(task_id.clone()),
        started_at: chrono::Utc::now(),
        last_seen: None,
    };
    let _ = orch.db().insert_agent(&agent);

    // Build per-worker MCP with task_id so enki_worker_report knows which worker it is.
    let worker_mcp = vec![enki_acp::acp_schema::McpServer::Stdio(
        enki_acp::acp_schema::McpServerStdio::new("enki", enki_bin)
            .args(vec![
                "mcp".into(),
                "--role".into(),
                "worker".into(),
                "--task-id".into(),
                task_id.0.clone(),
            ]),
    )];

    let agent_cmd =
        enki_core::agent_runtime::resolve().map_err(|e| anyhow::anyhow!("{e}"))?;
    let args_ref: Vec<&str> = agent_cmd.args.iter().map(|s| s.as_str()).collect();
    let session_id = mgr
        .start_session_with_mcp(
            agent_cmd.program.to_str().unwrap(),
            &args_ref,
            copy_path.clone(),
            worker_mcp,
        )
        .await?;

    // Register with tracker and orchestrator.
    tracker
        .borrow_mut()
        .register(session_id.clone(), task_id.0.clone());
    orch.set_step_session(&execution_id.0, step_id, session_id.clone());

    let timeout = monitor::tier_timeout(tier);
    let prompt = build_worker_prompt(title, description, upstream_outputs);
    let mgr_clone = mgr.clone();
    let tracker_clone = tracker.clone();
    let task_id = task_id.clone();
    let title = title.to_string();
    let branch_owned = branch;
    let copy_path_owned = copy_path;
    let done_tx = worker_done_tx.clone();
    let sid_for_done = session_id.clone();
    let exec_id_owned = Some(execution_id.clone());
    let step_id_owned = Some(step_id.to_string());

    tokio::task::spawn_local(async move {
        let result =
            match tokio::time::timeout(timeout, mgr_clone.prompt(&session_id, &prompt)).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(session_id, task_id = %task_id, "worker timed out");
                    let _ = mgr_clone.cancel(&session_id).await;
                    Err(enki_acp::AcpError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "worker timed out after {} minutes",
                            timeout.as_secs() / 60
                        ),
                    )))
                }
            };
        tracker_clone.borrow_mut().remove(&session_id);
        mgr_clone.kill_session(&session_id);
        let _ = done_tx.send(WorkerDone {
            task_id,
            agent_id,
            session_id: Some(sid_for_done),
            title,
            branch: branch_owned,
            copy_path: copy_path_owned,
            result: result.map_err(|e| e.to_string()),
            execution_id: exec_id_owned,
            step_id: step_id_owned,
        });
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Refinery dispatch
// ---------------------------------------------------------------------------

fn try_dispatch_merge(
    db: &enki_core::db::Db,
    db_path: &str,
    project_root: &Path,
    copies_dir: &Path,
    merger_done_tx: &mpsc::UnboundedSender<MergerDone>,
    merge_in_progress: &mut bool,
    _tx: &mpsc::UnboundedSender<FromCoordinator>,
) {
    let queued = match db.get_queued_merge_requests() {
        Ok(q) => q,
        Err(_) => return,
    };
    let Some(mr) = queued.first() else { return };

    *merge_in_progress = true;
    let mr_id = mr.id.clone();
    let branch = mr.branch.clone();
    let base_branch = mr.base_branch.clone();
    tracing::info!(mr_id = %mr_id, task_id = %mr.task_id, branch = %mr.branch, "dispatching merge");

    let _ = db.update_merge_status(&mr_id, MergeStatus::Processing);

    let done_tx = merger_done_tx.clone();
    let project_root_owned = project_root.to_path_buf();
    let copies_dir_owned = copies_dir.to_path_buf();
    let db_path_clone = db_path.to_string();

    // Determine copy path from branch name (task/<task_id> → copies/<task_id>).
    let copy_path = if let Some(task_id) = branch.strip_prefix("task/") {
        copies_dir.join(task_id)
    } else {
        copies_dir.join(&branch)
    };

    tokio::task::spawn_blocking(move || {
        let db =
            enki_core::db::Db::open(&db_path_clone).expect("refinery: failed to open db");
        let copy_mgr = CopyManager::new(project_root_owned, copies_dir_owned);
        let outcome =
            enki_core::refinery::process_merge(&copy_mgr, &copy_path, &branch, &base_branch, &db, &mr_id);
        let _ = done_tx.send(MergerDone {
            merge_request_id: mr_id,
            outcome,
        });
    });
}

#[allow(dead_code)]
fn cleanup_copy(copy_path: &Path) {
    let _ = std::fs::remove_dir_all(copy_path);
}
