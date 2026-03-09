use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use enki_acp::{AgentManager, SessionUpdate};
use enki_core::copy::{CopyManager, GitIdentity};
use enki_core::orchestrator::Orchestrator;
use tokio::sync::mpsc;

use super::prompts::build_system_prompt;
use super::session::CoordinatorSession;
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
    let config = enki_core::config::load_config(&cwd);
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
        // Merge agent-specific env from config (enki vars take precedence).
        for (k, v) in &config.agent.env {
            env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        env
    };

    // Set up events directory for signal files.
    let events_dir = enki_dir.join("events");
    orch.set_events_dir(events_dir);

    // Single agent manager for all sessions.
    let mut mgr = AgentManager::new();
    mgr.set_env(enki_env);

    // Resolve agent binary from config.
    let agent_cmd = match enki_core::agent_runtime::resolve_from_config(&config.agent) {
        Ok(cmd) => cmd,
        Err(e) => {
            let _ = tx.send(FromCoordinator::Error(format!(
                "failed to resolve agent binary: {e}"
            )));
            return None;
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
            false, // planner doesn't use sonnet_only
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

    let rt = Runtime {
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
    };

    // Clean up orphaned merge temp dirs from prior crashed sessions.
    let removed = rt.copy_mgr.cleanup_orphaned_merge_dirs(std::time::Duration::from_secs(3600));
    if !removed.is_empty() {
        tracing::info!(count = removed.len(), "cleaned up orphaned merge temp dirs");
    }

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
        enki_dir,
        enki_session_id,
        poll_interval,
    })
}
