mod prompts;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use enki_acp::{AgentManager, SessionUpdate};
use enki_core::monitor;
use enki_core::orchestrator::{
    Command, Event, MergeResult, Orchestrator, WorkerOutcome, WorkerResult,
};
use enki_core::scheduler::Limits;
use enki_core::types::{Id, MergeStatus, Tier, short_id};
use enki_core::copy::{CopyManager, GitIdentity};
use tokio::sync::mpsc;

use prompts::{build_system_prompt, build_worker_prompt, extract_output};

/// Messages sent from the TUI to the coordinator thread.
#[allow(dead_code)]
pub enum ToCoordinator {
    Prompt(String),
    Interrupt,
    Shutdown,
    /// Stop all running workers immediately.
    StopAll,
}

/// Activity update from a running worker agent.
#[derive(Debug, Clone)]
pub enum WorkerActivity {
    ToolStarted(String),
    ToolDone,
    Thinking,
}

/// Messages sent from the coordinator thread back to the TUI.
#[derive(Debug)]
#[allow(dead_code)]
pub enum FromCoordinator {
    Connected,
    Ready,
    Text(String),
    ToolCall(String),
    ToolCallDone(String),
    Done(String),
    WorkerSpawned { task_id: String, title: String, tier: String },
    WorkerCompleted { task_id: String, title: String },
    WorkerFailed {
        task_id: String,
        title: String,
        error: String,
    },
    WorkerUpdate {
        task_id: String,
        activity: WorkerActivity,
    },
    MergeQueued {
        mr_id: String,
        task_id: String,
        branch: String,
    },
    MergeLanded {
        mr_id: String,
        task_id: String,
        branch: String,
    },
    MergeConflicted {
        mr_id: String,
        task_id: String,
        branch: String,
    },
    MergeFailed {
        mr_id: String,
        task_id: String,
        branch: String,
        reason: String,
    },
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
// Shared coordinator context (immutable for the session lifetime)
// ---------------------------------------------------------------------------

struct Runtime {
    mgr: AgentManager,
    tracker: std::rc::Rc<std::cell::RefCell<WorkerTracker>>,
    worker_done_tx: mpsc::UnboundedSender<WorkerDone>,
    tx: mpsc::UnboundedSender<FromCoordinator>,
    enki_bin: PathBuf,
    copy_mgr: CopyManager,
    orch: Orchestrator,
    infra_broken: bool,
    db_path: String,
}

impl Runtime {
    fn kill_all_workers(&self) {
        let t = self.tracker.borrow();
        for sid in t.session_to_task.keys() {
            self.mgr.kill_session(sid);
        }
        drop(t);
        let mut t = self.tracker.borrow_mut();
        t.session_to_task.clear();
        t.last_activity.clear();
        t.current_tool.clear();
    }

    async fn handle_command(&mut self, cmd: Command, coord: &mut CoordinatorSession) {
        let events = self.orch.handle(cmd);
        self.process_events(events, coord).await;
    }

    async fn poll_tick(
        &mut self,
        coord: &mut CoordinatorSession,
        enki_dir: &std::path::Path,
        merger_done_tx: &mpsc::UnboundedSender<MergerDone>,
        merge_in_progress: &mut bool,
        last_merge_statuses: &mut HashMap<String, MergeStatus>,
    ) {
        // Check for external stop signal.
        let stop_file = enki_dir.join("stop");
        if stop_file.exists() {
            let _ = std::fs::remove_file(&stop_file);
            self.kill_all_workers();
            self.handle_command(Command::StopAll, coord).await;
        }

        // Discover new work from DB (external MCP calls) + check signal files.
        if !self.infra_broken {
            self.handle_command(Command::CheckSignals, coord).await;
            self.handle_command(Command::DiscoverFromDb, coord).await;
        }

        // Snapshot worker activity to DB.
        {
            let t = self.tracker.borrow();
            for (session_id, tool_name) in &t.current_tool {
                if let Some(task_id_str) = t.session_to_task.get(session_id) {
                    let task_id = Id(task_id_str.clone());
                    let _ = self.orch.db().update_task_activity(&task_id, Some(tool_name));
                }
            }
        }

        // Worker count sync.
        let _ = self.tx.send(FromCoordinator::WorkerCount(self.tracker.borrow().worker_count()));

        // Monitor patrol.
        let workers = self.tracker.borrow().worker_list();
        let events = self.orch.handle(Command::MonitorTick { workers });
        for event in &events {
            if let Event::MonitorCancel { session_id, task_id, stale_secs } = event {
                tracing::warn!(session_id, task_id, stale_secs, "monitor: worker stale, cancelling");
                let _ = self.mgr.cancel(session_id).await;
                coord.queue_event(format!(
                    "- Task ({}) worker stuck (no activity for {stale_secs}s) — cancel sent", short_id(task_id)
                ));
            }
            if let Event::MonitorEscalation(msg) = event {
                coord.queue_event(msg.clone());
            }
        }

        // Dispatch queued merge requests.
        if !*merge_in_progress {
            try_dispatch_merge(
                self.orch.db(), &self.db_path, &self.copy_mgr,
                merger_done_tx, merge_in_progress,
            );
        }

        // Merge progress polling.
        if let Ok(active_mrs) = self.orch.db().get_active_merge_requests() {
            let mut current_ids: HashSet<String> = HashSet::new();
            for mr in &active_mrs {
                current_ids.insert(mr.id.0.clone());
                let changed = match last_merge_statuses.get(&mr.id.0) {
                    Some(prev) => *prev != mr.status,
                    None => mr.status != MergeStatus::Queued,
                };
                if changed {
                    let _ = self.tx.send(FromCoordinator::MergeProgress {
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
        if !self.infra_broken {
            let events = self.orch.reconcile_merges();
            self.process_events(events, coord).await;
        }
    }

    /// Process orchestrator events: spawn workers, forward TUI messages, queue status updates.
    async fn process_events(&mut self, initial_events: Vec<Event>, coord: &mut CoordinatorSession) {
        let mut events = initial_events;

        while !events.is_empty() {
            let batch = std::mem::take(&mut events);
            for event in batch {
                match event {
                    Event::SpawnWorker {
                        task_id, title, description, tier,
                        execution_id, step_id, upstream_outputs,
                    } => {
                        if self.infra_broken {
                            let more = self.orch.handle(Command::WorkerDone(WorkerResult {
                                task_id: task_id.clone(),
                                execution_id: Some(execution_id),
                                step_id: Some(step_id),
                                title, branch: String::new(),
                                outcome: WorkerOutcome::Failed {
                                    error: "infrastructure broken".into(),
                                },
                            }));
                            events.extend(more);
                            continue;
                        }

                        match self.spawn_worker(
                            &task_id, &title, &description, tier,
                            &execution_id, &step_id, &upstream_outputs,
                        ).await {
                            Ok(()) => {
                                let _ = self.tx.send(FromCoordinator::WorkerSpawned {
                                    task_id: task_id.0.clone(),
                                    title: title.clone(),
                                    tier: tier.as_str().to_string(),
                                });
                                coord.queue_event(format!(
                                    "- Worker spawned for \"{}\" ({})", title, task_id
                                ));
                            }
                            Err(e) => {
                                let error = e.to_string();
                                tracing::error!(task_id = %task_id, error = %error, "failed to spawn worker");

                                if error.contains("cp failed") || error.contains("not found") {
                                    self.infra_broken = true;
                                }

                                let _ = self.tx.send(FromCoordinator::WorkerFailed {
                                    task_id: task_id.0.clone(),
                                    title: title.clone(),
                                    error: error.clone(),
                                });
                                coord.queue_event(format!(
                                    "- Task \"{}\" ({}) failed to spawn: {}",
                                    title, task_id, error
                                ));

                                let more = self.orch.handle(Command::WorkerDone(WorkerResult {
                                    task_id, execution_id: Some(execution_id),
                                    step_id: Some(step_id), title, branch: String::new(),
                                    outcome: WorkerOutcome::Failed { error },
                                }));
                                events.extend(more);
                            }
                        }
                    }
                    Event::KillSession { session_id } => {
                        self.mgr.kill_session(&session_id);
                        self.tracker.borrow_mut().remove(&session_id);
                        self.orch.session_ended(&session_id);
                    }
                    Event::QueueMerge(mr) => {
                        let _ = self.tx.send(FromCoordinator::MergeQueued {
                            mr_id: mr.id.0.clone(),
                            task_id: mr.task_id.0.clone(),
                            branch: mr.branch.clone(),
                        });
                        coord.queue_event(format!(
                            "- Task \"{}\" completed, merge {} queued", mr.task_id, mr.id
                        ));
                    }
                    Event::WorkerCompleted { task_id, title } => {
                        let _ = self.tx.send(FromCoordinator::WorkerCompleted {
                            task_id: task_id.clone(), title: title.clone(),
                        });
                    }
                    Event::WorkerFailed { task_id, title, error } => {
                        let _ = self.tx.send(FromCoordinator::WorkerFailed {
                            task_id: task_id.clone(), title: title.clone(), error: error.clone(),
                        });
                        coord.queue_event(format!("- Task \"{title}\" ({}) failed: {error}", short_id(&task_id)));
                    }
                    Event::MergeLanded { mr_id, task_id, branch } => {
                        let _ = self.tx.send(FromCoordinator::MergeLanded {
                            mr_id: mr_id.clone(), task_id: task_id.clone(), branch: branch.clone(),
                        });
                        coord.queue_event(format!(
                            "- Merge {} landed: task {} merged to main", short_id(&mr_id), short_id(&task_id)
                        ));
                    }
                    Event::MergeConflicted { mr_id, task_id, branch } => {
                        let _ = self.tx.send(FromCoordinator::MergeConflicted {
                            mr_id: mr_id.clone(), task_id: task_id.clone(), branch: branch.clone(),
                        });
                        coord.queue_event(format!(
                            "- Merge {} conflicted — task {} needs resolution", short_id(&mr_id), short_id(&task_id)
                        ));
                    }
                    Event::MergeFailed { mr_id, task_id, branch, reason } => {
                        let _ = self.tx.send(FromCoordinator::MergeFailed {
                            mr_id: mr_id.clone(), task_id: task_id.clone(),
                            branch: branch.clone(), reason: reason.clone(),
                        });
                        coord.queue_event(format!("- Merge {mr_id} failed: {reason}"));
                    }
                    Event::ExecutionComplete { execution_id } => {
                        coord.queue_event(format!("- Execution {execution_id} completed successfully"));
                    }
                    Event::ExecutionFailed { execution_id } => {
                        coord.queue_event(format!("- Execution {execution_id} failed"));
                    }
                    Event::AllStopped { count } => {
                        let _ = self.tx.send(FromCoordinator::AllStopped { count });
                    }
                    Event::MonitorCancel { .. } | Event::MonitorEscalation(_) => {
                        // Handled directly in poll_tick.
                    }
                    Event::TaskRetrying { task_id, title, attempt, max } => {
                        coord.queue_event(format!(
                            "- Task \"{title}\" ({}) timed out — retrying ({attempt}/{max})", short_id(&task_id)
                        ));
                    }
                    Event::StatusMessage(msg) => {
                        coord.queue_event(msg);
                    }
                    Event::WorkerReport { task_id, status } => {
                        let _ = self.tx.send(FromCoordinator::WorkerReport { task_id, status });
                    }
                    Event::CheckpointReached { execution_id, step_id, title: _, output } => {
                        let output_str = output.as_deref().unwrap_or("(no output)");
                        coord.queue_event(format!(
                            "- CHECKPOINT reached: step \"{step_id}\" in execution {execution_id} completed. \
                             Output: {output_str}\n  \
                             The execution is now paused. Review the output, then either:\n  \
                             - Call enki_execution_add_steps to add follow-up steps, then enki_resume to continue\n  \
                             - Call enki_resume directly to continue with remaining steps"
                        ));
                    }
                    Event::Mail { from, to, subject, priority, .. } => {
                        let _ = self.tx.send(FromCoordinator::Mail {
                            from: from.clone(), to: to.clone(),
                            subject: subject.clone(), priority: priority.clone(),
                        });
                        if to == "coordinator" {
                            coord.queue_event(format!(
                                "- Mail from {from}: \"{subject}\" [priority: {priority}]"
                            ));
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_worker(
        &mut self,
        task_id: &Id,
        title: &str,
        description: &str,
        tier: Tier,
        execution_id: &Id,
        step_id: &str,
        upstream_outputs: &[(String, String)],
    ) -> anyhow::Result<()> {
        let branch = format!("task/{}", task_id);
        let (copy_path, base_commit, base_branch) = self.copy_mgr.create_copy(&task_id.0)?;

        let agent_id = Id::new("agent");
        self.orch.db().assign_task(
            task_id, &agent_id, copy_path.to_str().unwrap(), &branch, &base_branch,
        )?;

        let worker_mcp = vec![enki_acp::acp_schema::McpServer::Stdio(
            enki_acp::acp_schema::McpServerStdio::new("enki", &self.enki_bin)
                .args(vec![
                    "mcp".into(), "--role".into(), "worker".into(),
                    "--task-id".into(), task_id.0.clone(),
                ]),
        )];

        let agent_cmd =
            enki_core::agent_runtime::resolve().map_err(|e| anyhow::anyhow!("{e}"))?;
        let args_ref: Vec<&str> = agent_cmd.args.iter().map(|s| s.as_str()).collect();
        let session_id = self.mgr
            .start_session_with_mcp(
                agent_cmd.program.to_str().unwrap(), &args_ref,
                copy_path.clone(), worker_mcp, &task_id.0,
            )
            .await?;

        self.tracker.borrow_mut().register(session_id.clone(), task_id.0.clone());
        self.orch.set_step_session(&execution_id.0, step_id, session_id.clone());

        let timeout = monitor::tier_timeout(tier);
        let prompt = build_worker_prompt(title, description, upstream_outputs);
        let mgr_clone = self.mgr.clone();
        let tracker_clone = self.tracker.clone();
        let task_id = task_id.clone();
        let title = title.to_string();
        let branch_owned = branch;
        let copy_path_owned = copy_path;
        let done_tx = self.worker_done_tx.clone();
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
                            format!("worker timed out after {} minutes", timeout.as_secs() / 60),
                        )))
                    }
                };
            tracker_clone.borrow_mut().remove(&session_id);
            mgr_clone.kill_session(&session_id);
            let _ = done_tx.send(WorkerDone {
                task_id,
                session_id: Some(sid_for_done),
                title,
                branch: branch_owned,
                copy_path: copy_path_owned,
                base_commit,
                result: result.map_err(|e| e.to_string()),
                execution_id: exec_id_owned,
                step_id: step_id_owned,
            });
        });

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal channel types
// ---------------------------------------------------------------------------

struct WorkerDone {
    task_id: Id,
    session_id: Option<String>,
    title: String,
    branch: String,
    copy_path: PathBuf,
    base_commit: Option<String>,
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
// CoordinatorSession — the coordinator agent's session state
// ---------------------------------------------------------------------------

struct CoordinatorSession {
    session_id: String,
    pending_events: Vec<String>,
    prompt_generation: u64,
    active_prompt: Option<tokio::task::JoinHandle<()>>,
    prompt_done_tx: mpsc::UnboundedSender<(u64, Result<String, String>)>,
    forward_updates: std::rc::Rc<std::cell::Cell<bool>>,
}

impl CoordinatorSession {
    fn new(session_id: String) -> (Self, mpsc::UnboundedReceiver<(u64, Result<String, String>)>) {
        let (prompt_done_tx, prompt_done_rx) = mpsc::unbounded_channel();
        let session = Self {
            session_id,
            pending_events: Vec::new(),
            prompt_generation: 0,
            active_prompt: None,
            prompt_done_tx,
            forward_updates: std::rc::Rc::new(std::cell::Cell::new(false)),
        };
        (session, prompt_done_rx)
    }

    fn queue_event(&mut self, msg: String) {
        self.pending_events.push(msg);
    }

    async fn deliver_prompt(
        &mut self,
        mgr: &AgentManager,
        tx: &mpsc::UnboundedSender<FromCoordinator>,
        text: String,
    ) {
        if let Some(handle) = self.active_prompt.take() {
            let _ = mgr.cancel(&self.session_id).await;
            handle.abort();
            let _ = tx.send(FromCoordinator::Interrupted);
        }

        let full_text = if self.pending_events.is_empty() {
            text
        } else {
            let events_text = std::mem::take(&mut self.pending_events).join("\n");
            format!("[worker status updates]\n{events_text}\n\n[user message]\n{text}")
        };

        self.spawn_prompt(mgr, full_text);
    }

    fn handle_prompt_done(
        &mut self,
        generation: u64,
        result: Result<String, String>,
    ) -> Option<FromCoordinator> {
        if generation != self.prompt_generation {
            return None;
        }
        self.active_prompt = None;
        Some(match result {
            Ok(stop_reason) => FromCoordinator::Done(stop_reason),
            Err(e) => FromCoordinator::Error(format!("prompt error: {e}")),
        })
    }

    async fn interrupt(
        &mut self,
        mgr: &AgentManager,
        tx: &mpsc::UnboundedSender<FromCoordinator>,
    ) {
        if let Some(handle) = self.active_prompt.take() {
            let _ = mgr.cancel(&self.session_id).await;
            handle.abort();
            let _ = tx.send(FromCoordinator::Interrupted);
        }
    }

    fn shutdown(&mut self, mgr: &AgentManager) {
        if let Some(handle) = self.active_prompt.take() {
            handle.abort();
        }
        mgr.kill_session(&self.session_id);
    }

    fn flush_if_idle(&mut self, mgr: &AgentManager) {
        if self.active_prompt.is_some() || self.pending_events.is_empty() {
            return;
        }
        let events_text = std::mem::take(&mut self.pending_events).join("\n");
        let msg = format!("[worker status updates]\n{events_text}");
        self.spawn_prompt(mgr, msg);
    }

    fn spawn_prompt(&mut self, mgr: &AgentManager, text: String) {
        self.prompt_generation += 1;
        let generation = self.prompt_generation;
        let mgr = mgr.clone();
        let sid = self.session_id.clone();
        let done_tx = self.prompt_done_tx.clone();
        self.active_prompt = Some(tokio::task::spawn_local(async move {
            let result = mgr.prompt(&sid, &text).await;
            let _ = done_tx.send((generation, result.map_err(|e| e.to_string())));
        }));
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

    // Create a new session for this process lifetime.
    let session_id_obj = enki_core::types::Id::new("sess");
    let session = enki_core::types::Session {
        id: session_id_obj.clone(),
        started_at: chrono::Utc::now(),
        ended_at: None,
    };
    if let Err(e) = db.insert_session(&session) {
        let _ = tx.send(FromCoordinator::Error(format!("failed to create session: {e}")));
        return;
    }
    let enki_session_id = session_id_obj.0.clone();
    tracing::info!(session_id = %enki_session_id, "new session created");

    let mut orch = Orchestrator::new(db, Limits::default(), enki_session_id.clone());

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
        env.insert("ENKI_SESSION_ID".to_string(), enki_session_id.clone());
        env
    };

    // Set up events directory for signal files.
    let events_dir = enki_dir.join("events");
    orch.set_events_dir(events_dir);

    // Single agent manager for all sessions.
    let mut mgr = AgentManager::new();
    mgr.set_env(enki_env);

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

    // Start coordinator ACP session.
    let planner_mcp = vec![enki_acp::acp_schema::McpServer::Stdio(
        enki_acp::acp_schema::McpServerStdio::new("enki", &enki_bin)
            .args(vec!["mcp".into(), "--role".into(), "planner".into()]),
    )];
    let args_ref: Vec<&str> = agent_cmd.args.iter().map(|s| s.as_str()).collect();
    let coord_session_id = match mgr
        .start_session_with_mcp(
            agent_cmd.program.to_str().unwrap(),
            &args_ref,
            cwd.clone(),
            planner_mcp,
            "coordinator",
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

    let (mut coord, mut prompt_done_rx) = CoordinatorSession::new(coord_session_id);

    // Unified on_update callback routing by session_id.
    let tracker = std::rc::Rc::new(std::cell::RefCell::new(WorkerTracker::new()));
    {
        let coord_sid = coord.session_id.clone();
        let forward_flag = coord.forward_updates.clone();
        let tx_updates = tx.clone();
        let tracker_cb = tracker.clone();
        mgr.on_update(move |session_id, update| {
            if session_id == coord_sid {
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
            } else {
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

                let _ = tx_updates.send(FromCoordinator::WorkerUpdate { task_id, activity });
            }
        });
    }

    // Send system prompt (updates suppressed during this phase).
    let system_prompt = build_system_prompt(&cwd);
    match mgr.prompt(&coord.session_id, &system_prompt).await {
        Ok(_) => {
            coord.forward_updates.set(true);
            let _ = tx.send(FromCoordinator::Ready);
        }
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "system prompt failed: {e}"
            )));
            return;
        }
    }

    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(3));
    poll_interval.tick().await;

    let project_root = crate::commands::project_root().unwrap_or_default();
    let copies_dir = crate::commands::copies_dir().unwrap_or_default();
    let git_identity = match GitIdentity::from_git_config(&project_root) {
        Ok(id) => id,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!("git identity: {e}")));
            return;
        }
    };

    let mut rt = Runtime {
        mgr,
        tracker,
        worker_done_tx,
        tx,
        enki_bin,
        copy_mgr: CopyManager::new(project_root, copies_dir, git_identity),
        orch,
        infra_broken: false,
        db_path,
    };

    // Stateless session: no crash recovery. Fresh start every time.

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
                        coord.deliver_prompt(&rt.mgr, &rt.tx, text).await;
                    }
                    ToCoordinator::Interrupt => {
                        coord.interrupt(&rt.mgr, &rt.tx).await;
                    }
                    ToCoordinator::Shutdown => {
                        coord.shutdown(&rt.mgr);
                        break;
                    }
                    ToCoordinator::StopAll => {
                        rt.kill_all_workers();
                        rt.handle_command(Command::StopAll, &mut coord).await;
                    }
                }
            }

            result = prompt_done_rx.recv() => {
                if let Some((generation, result)) = result {
                    if let Some(msg) = coord.handle_prompt_done(generation, result) {
                        let _ = rt.tx.send(msg);
                    }
                }
            }

            done = worker_done_rx.recv() => {
                let Some(done) = done else { continue };

                if let Some(sid) = done.session_id.as_ref() {
                    rt.tracker.borrow_mut().remove(sid);
                    rt.orch.session_ended(sid);
                }
                let _ = rt.orch.db().update_task_activity(&done.task_id, None);

                let worker_result = process_worker_done(done, &rt.copy_mgr);
                rt.handle_command(Command::WorkerDone(worker_result), &mut coord).await;
            }

            done = merger_done_rx.recv() => {
                let Some(done) = done else { continue };
                merge_in_progress = false;

                let events = rt.orch.handle(Command::MergeDone(MergeResult {
                    mr_id: done.merge_request_id,
                    outcome: done.outcome,
                }));

                // After merge, clean up worker copies.
                for event in &events {
                    if let Event::MergeLanded { mr_id, .. } = event {
                        let mr_id_obj = Id(mr_id.clone());
                        if let Ok(mr) = rt.orch.db().get_merge_request(&mr_id_obj)
                            && let Some(task_id) = mr.branch.strip_prefix("task/") {
                                let copy_path = rt.copy_mgr.copies_dir().join(task_id);
                                tokio::task::spawn_blocking(move || {
                                    let _ = std::fs::remove_dir_all(&copy_path);
                                });
                            }
                    }
                }

                rt.process_events(events, &mut coord).await;
            }

            _ = poll_interval.tick() => {
                rt.poll_tick(&mut coord, &enki_dir, &merger_done_tx,
                             &mut merge_in_progress, &mut last_merge_statuses).await;
            }
        }

        coord.flush_if_idle(&rt.mgr);
    }

    // Session cleanup: abandon in-flight work and end the session.
    if let Ok(count) = rt.orch.db().abandon_session_tasks(&enki_session_id)
        && count > 0 {
            tracing::info!(count, "session end: abandoned in-flight tasks");
        }
    if let Ok(count) = rt.orch.db().abandon_session_merges(&enki_session_id)
        && count > 0 {
            tracing::info!(count, "session end: abandoned in-flight merges");
        }
    let _ = rt.orch.db().end_session(&enki_session_id);
    tracing::info!(session_id = %enki_session_id, "session ended");
}

// ---------------------------------------------------------------------------
// Worker done processing (infrastructure layer)
// ---------------------------------------------------------------------------

/// Convert a raw WorkerDone (from the channel) into an orchestrator WorkerResult.
/// Handles auto-commit, change detection, and copy cleanup.
fn process_worker_done(done: WorkerDone, copy_mgr: &CopyManager) -> WorkerResult {
    match done.result {
        Ok(ref stop_reason) => {
            // Auto-commit uncommitted changes in the copy.
            let msg = format!("{}\n\ncreated by enki", done.title);
            let committed = copy_mgr.commit_copy(&done.copy_path, &msg);

            // Check for actual changes vs the base commit.
            // Fast path: if commit_copy found nothing dirty, check if the
            // worker committed anything by comparing HEAD to the base commit.
            // This avoids spawning a `git diff --stat` process in the common
            // no-changes case.
            let has_changes = if committed {
                true
            } else {
                match &done.base_commit {
                    Some(base) => {
                        enki_core::copy::head_sha(&done.copy_path)
                            .as_deref() != Some(base.as_str())
                    }
                    None => enki_core::copy::head_sha(&done.copy_path).is_some(),
                }
            };

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
// Refinery dispatch
// ---------------------------------------------------------------------------

fn try_dispatch_merge(
    db: &enki_core::db::Db,
    db_path: &str,
    copy_mgr: &CopyManager,
    merger_done_tx: &mpsc::UnboundedSender<MergerDone>,
    merge_in_progress: &mut bool,
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
    let project_root_owned = copy_mgr.project_root().to_path_buf();
    let copies_dir_owned = copy_mgr.copies_dir().to_path_buf();
    let db_path_clone = db_path.to_string();
    let git_identity_owned = copy_mgr.git_identity().clone();

    // Determine copy path from branch name (task/<task_id> → copies/<task_id>).
    let copy_path = copy_mgr.copies_dir().join(
        branch.strip_prefix("task/").unwrap_or(&branch),
    );

    tokio::task::spawn_blocking(move || {
        let db =
            enki_core::db::Db::open(&db_path_clone).expect("refinery: failed to open db");
        let copy_mgr = CopyManager::new(project_root_owned, copies_dir_owned, git_identity_owned);
        let outcome =
            enki_core::refinery::process_merge(&copy_mgr, &copy_path, &branch, &base_branch, &db, &mr_id);
        let _ = done_tx.send(MergerDone {
            merge_request_id: mr_id,
            outcome,
        });
    });
}
