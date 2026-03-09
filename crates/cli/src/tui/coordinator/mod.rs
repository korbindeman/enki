mod events;
mod init;
mod merges;
mod prompts;
mod session;
mod spawning;
mod tracker;
mod workers;

use std::collections::HashMap;
use std::path::PathBuf;

use enki_acp::AgentManager;
use enki_core::orchestrator::{
    Command, MergeResult, Orchestrator,
};
use enki_core::types::MergeStatus;
use enki_core::copy::CopyManager;
use tokio::sync::mpsc;

use session::CoordinatorSession;
pub use tracker::WorkerActivity;
use tracker::WorkerTracker;
use workers::{
    WorkerDone, MergerDone, MergerAgentDone,
    process_worker_done,
};

/// Messages sent from the TUI to the coordinator thread.
#[allow(dead_code)]
pub enum ToCoordinator {
    Prompt(String),
    Interrupt,
    Shutdown,
    /// Stop all running workers immediately.
    StopAll,
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
    pub(super) join_handle: Option<std::thread::JoinHandle<()>>,
}

/// Spawn the coordinator on a dedicated OS thread with its own tokio runtime + LocalSet.
pub fn spawn(cwd: PathBuf, db_path: String, enki_bin: PathBuf) -> CoordinatorHandle {
    let (to_coord_tx, to_coord_rx) = mpsc::unbounded_channel::<ToCoordinator>();
    let (from_coord_tx, from_coord_rx) = mpsc::unbounded_channel::<FromCoordinator>();

    // Clone tx so we can report panics after the original is consumed by the coordinator.
    let panic_tx = from_coord_tx.clone();

    let join_handle = std::thread::Builder::new()
        .name("coordinator-acp".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build coordinator runtime");

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rt.block_on(async {
                    let local = tokio::task::LocalSet::new();
                    local
                        .run_until(coordinator_loop(cwd, db_path, enki_bin, to_coord_rx, from_coord_tx))
                        .await;
                });
            }));

            if let Err(payload) = result {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                tracing::error!(error = %msg, "coordinator thread panicked");
                let _ = panic_tx.send(FromCoordinator::Error(
                    format!("coordinator panicked: {msg}"),
                ));
            }
        })
        .expect("failed to spawn coordinator thread");

    CoordinatorHandle {
        tx: to_coord_tx,
        rx: from_coord_rx,
        join_handle: Some(join_handle),
    }
}

// ---------------------------------------------------------------------------
// Shared coordinator context
// ---------------------------------------------------------------------------

struct Runtime {
    mgr: AgentManager,
    tracker: std::rc::Rc<std::cell::RefCell<WorkerTracker>>,
    worker_done_tx: mpsc::UnboundedSender<WorkerDone>,
    merger_agent_done_tx: mpsc::UnboundedSender<MergerAgentDone>,
    tx: mpsc::UnboundedSender<FromCoordinator>,
    enki_bin: PathBuf,
    copy_mgr: CopyManager,
    orch: Orchestrator,
    infra_broken: bool,
    db_path: String,
    roles: std::collections::HashMap<String, enki_core::roles::RoleConfig>,
    config: enki_core::config::Config,
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
    let Some(init::InitState {
        mut rt, mut coord, mut prompt_done_rx,
        mut worker_done_rx, mut merger_agent_done_rx,
        enki_dir, enki_session_id, mut poll_interval,
    }) = init::initialize(cwd, db_path, enki_bin, tx).await
    else {
        return;
    };

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
                        // Kill all workers before shutting down the coordinator session.
                        rt.kill_all_workers();
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
                if let Some((generation, result)) = result
                    && let Some(msg) = coord.handle_prompt_done(generation, result)
                {
                    let _ = rt.tx.send(msg);
                }
            }

            done = worker_done_rx.recv() => {
                let Some(done) = done else { continue };

                if let Some(sid) = done.session_id.as_ref() {
                    rt.tracker.borrow_mut().remove(sid);
                    rt.orch.session_ended(sid);
                }
                let _ = rt.orch.db().update_task_activity(&done.task_id, None);

                let worker_result = process_worker_done(done, &rt.copy_mgr, &enki_dir, &rt.config.git.commit_suffix);
                rt.handle_command(Command::WorkerDone(worker_result), &mut coord).await;
            }

            done = merger_done_rx.recv() => {
                let Some(done) = done else { continue };
                merge_in_progress = false;

                let events = rt.orch.handle(Command::MergeDone(MergeResult {
                    mr_id: done.merge_request_id,
                    outcome: done.outcome,
                }));

                // Worktree + branch cleanup is handled by remove_copy (called from
                // process_worker_done) and finish_merge_inner in the refinery.

                rt.process_events(events, &mut coord).await;
            }

            done = merger_agent_done_rx.recv() => {
                let Some(done) = done else { continue };
                rt.tracker.borrow_mut().remove(&done.session_id);
                rt.orch.session_ended(&done.session_id);

                // Run finish_merge in a blocking thread (it does git operations).
                let mr_id = done.mr_id.clone();
                let temp_dir = done.temp_dir.clone();
                let default_branch = done.default_branch.clone();
                let db_path_clone = rt.db_path.clone();
                let project_root = rt.copy_mgr.project_root().to_path_buf();
                let copies_dir = rt.copy_mgr.copies_dir().to_path_buf();
                let git_identity = rt.copy_mgr.git_identity().clone();
                let merger_done_tx_clone = merger_done_tx.clone();

                // Build commit message from task title + suffix.
                let commit_message = {
                    let mr = rt.orch.db().get_merge_request(&done.mr_id);
                    let title = mr.as_ref().ok()
                        .and_then(|mr| rt.orch.db().get_task(&mr.task_id).ok())
                        .map(|t| t.title)
                        .unwrap_or_else(|| done.mr_id.0.clone());
                    let suffix = &rt.config.git.commit_suffix;
                    if suffix.is_empty() { title } else { format!("{title}\n\n{suffix}") }
                };

                tokio::task::spawn_blocking(move || {
                    let db = enki_core::db::Db::open(&db_path_clone)
                        .expect("finish_merge: failed to open db");
                    let copy_mgr = CopyManager::new(
                        project_root, copies_dir, git_identity,
                    );
                    let outcome = enki_core::refinery::finish_merge(
                        &copy_mgr, &temp_dir, &default_branch, &db, &mr_id, &commit_message,
                    );
                    let _ = merger_done_tx_clone.send(MergerDone {
                        merge_request_id: mr_id,
                        outcome,
                    });
                });
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

    // Clean up all worker worktrees.
    rt.copy_mgr.cleanup_all_worktrees();

    tracing::info!(session_id = %enki_session_id, "session ended");
}
