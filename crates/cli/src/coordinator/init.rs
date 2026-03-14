use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use enki_acp::{AgentManager, SessionUpdate};
use enki_core::copy::{CopyManager, GitIdentity};
use enki_core::orchestrator::Orchestrator;
use tokio::sync::mpsc;

use super::prompts::build_system_prompt;
use super::session::CoordinatorSession;
use super::sidecar::{self, SidecarSession};
use super::tracker::WorkerTracker;
use super::workers::{MergerAgentDone, WorkerDone};
use super::{FromCoordinator, Runtime, WorkerActivity};

/// All state produced by coordinator initialization, consumed by the main select loop.
pub(super) struct InitState {
    pub rt: Runtime,
    pub coord: CoordinatorSession,
    pub prompt_done_rx: mpsc::UnboundedReceiver<super::session::PromptResult>,
    pub worker_done_rx: mpsc::UnboundedReceiver<WorkerDone>,
    pub merger_agent_done_rx: mpsc::UnboundedReceiver<MergerAgentDone>,
    pub sidecar: SidecarSession,
    pub sidecar_done_rx: mpsc::UnboundedReceiver<sidecar::SidecarResult>,
    pub enki_dir: PathBuf,
    pub enki_session_id: String,
    pub poll_interval: tokio::time::Interval,
}

/// Initialize all coordinator state: DB, session, ACP manager, planner session, etc.
/// Returns `None` if initialization fails (error already sent to `tx`).
pub(super) async fn initialize(
    cwd: PathBuf,
    db_path: String,
    enki_bin: PathBuf,
    agent_override: Option<String>,
    tx: mpsc::UnboundedSender<FromCoordinator>,
) -> Option<InitState> {
    let init_start = Instant::now();
    tracing::info!(cwd = %cwd.display(), enki_bin = %enki_bin.display(), "coordinator loop started");

    let db = match enki_core::db::Db::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!("failed to open db: {e}")));
            return None;
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
        return None;
    }
    let enki_session_id = session_id_obj.0.clone();
    tracing::info!(session_id = %enki_session_id, "new session created");

    // Load config: ~/.config/enki.toml → .enki/enki.toml
    let mut config = enki_core::config::load_config(&cwd);
    if let Some(agent) = agent_override {
        config.agent.command = agent;
    }
    tracing::info!(?config.agent.command, sonnet_only = config.workers.sonnet_only, "loaded config");

    let mut orch = Orchestrator::new(db, config.workers.limits.clone(), enki_session_id.clone());

    let (worker_done_tx, worker_done_rx) = mpsc::unbounded_channel::<WorkerDone>();

    // Derive all paths from the explicit `cwd` parameter, not from env vars or process CWD.
    let enki_dir = cwd.join(".enki");
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

    // Resolve coordinator agent binary from config.
    let coord_agent_cfg = config.agent_for_role("coordinator");
    let coord_cmd = match enki_core::agent_runtime::resolve_from_config(&coord_agent_cfg) {
        Ok(cmd) => cmd,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "failed to resolve coordinator agent: {e}"
            )));
            return None;
        }
    };

    // Start coordinator ACP session.
    let planner_mcp = vec![enki_acp::acp_schema::McpServer::Stdio(
        enki_acp::acp_schema::McpServerStdio::new("enki", &enki_bin)
            .args(vec!["mcp".into(), "--role".into(), "planner".into()]),
    )];
    let args_ref: Vec<&str> = coord_cmd.args.iter().map(|s| s.as_str()).collect();
    let coord_session_id = match mgr
        .start_session_with_mcp(
            coord_cmd.program.to_str().unwrap(),
            &args_ref,
            cwd.clone(),
            planner_mcp,
            "coordinator",
            false, // planner doesn't use sonnet_only
            &coord_cmd.env,
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
            return None;
        }
    };

    let (coord, prompt_done_rx) = CoordinatorSession::new(coord_session_id);

    // Resolve sidecar agent (may differ from coordinator).
    let sidecar_agent_cfg = config.agent_for_role("sidecar");
    let sidecar_cmd = match enki_core::agent_runtime::resolve_from_config(&sidecar_agent_cfg) {
        Ok(cmd) => cmd,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "failed to resolve sidecar agent: {e}"
            )));
            return None;
        }
    };

    // Create sidecar ACP session (works in project root, no worktree).
    let sidecar_mcp = vec![enki_acp::acp_schema::McpServer::Stdio(
        enki_acp::acp_schema::McpServerStdio::new("enki", &enki_bin)
            .args(vec!["mcp".into(), "--role".into(), "sidecar".into()]),
    )];
    let sidecar_args_ref: Vec<&str> = sidecar_cmd.args.iter().map(|s| s.as_str()).collect();
    let sidecar_session_id = match mgr
        .start_session_with_mcp(
            sidecar_cmd.program.to_str().unwrap(),
            &sidecar_args_ref,
            cwd.clone(),
            sidecar_mcp,
            "sidecar",
            true, // sonnet_only — fast and cheap for quick tasks
            &sidecar_cmd.env,
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "failed to start sidecar: {e}"
            )));
            return None;
        }
    };

    let (sidecar, sidecar_done_rx) = SidecarSession::new(sidecar_session_id);

    // Unified on_update callback routing by session_id.
    let tracker = std::rc::Rc::new(std::cell::RefCell::new(WorkerTracker::new()));
    {
        let coord_sid = coord.session_id.clone();
        let sidecar_sid = sidecar.session_id.clone();
        let forward_flag = coord.forward_updates.clone();
        let forward_sidecar = sidecar.forward_updates.clone();
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
            } else if session_id == sidecar_sid {
                if !forward_sidecar.get() {
                    return;
                }
                let activity = match update {
                    SessionUpdate::ToolCallStarted { title, .. } => WorkerActivity::ToolStarted(title),
                    SessionUpdate::ToolCallDone { .. } => WorkerActivity::ToolDone,
                    SessionUpdate::Text(_) => WorkerActivity::Thinking,
                    SessionUpdate::Plan(_) => return,
                };
                let _ = tx_updates.send(FromCoordinator::SidecarUpdate { activity });
            } else {
                let mut t = tracker_cb.borrow_mut();
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

    // Load agent roles.
    let roles = enki_core::roles::load_roles(&cwd);
    tracing::info!(role_count = roles.len(), "loaded agent roles");

    // Send system prompt (updates suppressed during this phase).
    let system_prompt = build_system_prompt(&cwd, &roles);
    let content = vec![enki_acp::acp_schema::ContentBlock::Text(
        enki_acp::acp_schema::TextContent::new(system_prompt),
    )];
    match mgr.prompt(&coord.session_id, content).await {
        Ok(_) => {
            coord.forward_updates.set(true);
            let _ = tx.send(FromCoordinator::Ready);
        }
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "system prompt failed: {e}"
            )));
            return None;
        }
    }

    // Send sidecar system prompt.
    let sidecar_prompt = "You are a quick-task sidecar agent for the Enki coding orchestrator. \
        You work directly on the main branch in the project root directory. \
        Your job is to handle small, focused tasks: fixing typos, making config changes, \
        committing code, running quick checks. \
        After making any file changes, ALWAYS commit them with a clear, concise commit message. \
        Keep your changes small and focused. Do not create branches or worktrees.";
    let sidecar_content = vec![enki_acp::acp_schema::ContentBlock::Text(
        enki_acp::acp_schema::TextContent::new(sidecar_prompt.to_string()),
    )];
    match mgr.prompt(&sidecar.session_id, sidecar_content).await {
        Ok(_) => {
            sidecar.forward_updates.set(true);
        }
        Err(e) => {
            tracing::warn!(error = %e, "sidecar system prompt failed (non-fatal)");
        }
    }

    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(3));
    poll_interval.tick().await;

    let project_root = cwd.clone();
    let copies_dir = enki_dir.join("copies");
    let git_identity = match GitIdentity::from_git_config(&project_root) {
        Ok(id) => id,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!("git identity: {e}")));
            return None;
        }
    };

    let (merger_agent_done_tx, merger_agent_done_rx) = mpsc::unbounded_channel::<MergerAgentDone>();

    let mut rt = Runtime {
        mgr,
        tracker,
        worker_done_tx,
        merger_agent_done_tx,
        tx,
        enki_bin,
        copy_mgr: CopyManager::new(project_root, copies_dir, git_identity),
        orch,
        infra_broken: false,
        db_path,
        roles,
        config,
        session_start: Instant::now(),
        stats: super::SessionStats::default(),
        merge_start_times: std::collections::HashMap::new(),
        merge_conflict_info: std::collections::HashMap::new(),
    };

    // Clean up orphaned merge temp dirs from prior crashed sessions.
    let removed = rt.copy_mgr.cleanup_orphaned_merge_dirs(std::time::Duration::from_secs(3600));
    if !removed.is_empty() {
        tracing::info!(count = removed.len(), "cleaned up orphaned merge temp dirs");
    }

    // Re-queue MRs stuck in transient states from prior sessions.
    rt.orch.reconcile_stuck_merges();

    tracing::info!(
        elapsed_ms = init_start.elapsed().as_millis() as u64,
        "coordinator initialized"
    );

    Some(InitState {
        rt,
        coord,
        prompt_done_rx,
        worker_done_rx,
        merger_agent_done_rx,
        sidecar,
        sidecar_done_rx,
        enki_dir,
        enki_session_id,
        poll_interval,
    })
}
