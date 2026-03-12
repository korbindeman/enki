mod events;
mod init;
mod merges;
mod prompts;
mod session;
mod sidecar;
mod spawning;
mod tracker;
mod workers;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use enki_acp::AgentManager;
use enki_core::orchestrator::{
    Command, MergeResult, Orchestrator,
};
use enki_core::types::MergeStatus;
use enki_core::copy::CopyManager;
use tokio::sync::mpsc;
use tracing::Instrument;

use session::CoordinatorSession;
pub use tracker::WorkerActivity;
use tracker::WorkerTracker;
use workers::{
    WorkerDone, MergerDone, MergerAgentDone,
    process_worker_done,
};

/// Conflict info stored between MergeNeedsResolution and MergeLanded/MergeFailed.
struct MergeConflictInfo {
    task_title: String,
    conflict_files: Vec<String>,
}

/// Counters for session-end summary.
#[derive(Default)]
struct SessionStats {
    workers_spawned: u32,
    workers_completed: u32,
    workers_failed: u32,
    merges_landed: u32,
    merges_failed: u32,
    prompts_delivered: u32,
}

/// Image attachment for prompts.
pub struct ImageData {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// Messages sent to the coordinator thread.
#[allow(dead_code)]
pub enum ToCoordinator {
    Prompt { text: String, images: Vec<ImageData> },
    Interrupt,
    Shutdown,
    /// Stop all running workers immediately.
    StopAll,
    /// Stop a single worker by task_id.
    StopWorker { task_id: String },
}

/// Messages sent from the coordinator thread back to the UI.
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
    SidecarStarted { prompt: String },
    SidecarUpdate { activity: WorkerActivity },
    SidecarCompleted,
    Interrupted,
    Error(String),
}

/// Handle held by the caller to communicate with the coordinator.
pub struct CoordinatorHandle {
    pub tx: mpsc::UnboundedSender<ToCoordinator>,
    pub rx: mpsc::UnboundedReceiver<FromCoordinator>,
    pub join_handle: Option<std::thread::JoinHandle<()>>,
}

/// Spawn the coordinator on a dedicated OS thread with its own tokio runtime + LocalSet.
pub fn spawn(cwd: PathBuf, db_path: String, enki_bin: PathBuf, agent_override: Option<String>) -> CoordinatorHandle {
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
                        .run_until(coordinator_loop(cwd, db_path, enki_bin, agent_override, to_coord_rx, from_coord_tx))
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
    session_start: Instant,
    stats: SessionStats,
    /// Tracks when each merge started for duration logging.
    merge_start_times: HashMap<String, Instant>,
    /// Conflict info stored between MergeNeedsResolution and MergeLanded/MergeFailed
    /// for coalescing coordinator announcements.
    merge_conflict_info: HashMap<String, MergeConflictInfo>,
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
        t.current_tool.clear();
    }

    fn kill_worker(&self, task_id: &str) {
        let session_id = self.tracker.borrow().task_to_session(task_id);
        if let Some(sid) = session_id {
            tracing::info!(task_id, session_id = %sid, "stopping worker");
            self.mgr.kill_session(&sid);
        } else {
            tracing::warn!(task_id, "stop_worker: no active session for task");
        }
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
    agent_override: Option<String>,
    mut rx: mpsc::UnboundedReceiver<ToCoordinator>,
    tx: mpsc::UnboundedSender<FromCoordinator>,
) {
    let Some(init::InitState {
        mut rt, mut coord, mut prompt_done_rx,
        mut worker_done_rx, mut merger_agent_done_rx,
        mut sidecar, mut sidecar_done_rx,
        enki_dir, enki_session_id, mut poll_interval,
    }) = init::initialize(cwd, db_path, enki_bin, agent_override, tx).await
    else {
        return;
    };

    // Wrap the main loop in a session span so every log line carries the session_id.
    let session_span = tracing::info_span!("session", session_id = %enki_session_id);

    async {
        // Refinery state.
        let (merger_done_tx, mut merger_done_rx) = mpsc::unbounded_channel::<MergerDone>();
        let mut merge_in_progress = false;
        let mut last_merge_statuses: HashMap<String, MergeStatus> = HashMap::new();

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    match msg {
                        ToCoordinator::Prompt { text, images } => {
                            rt.stats.prompts_delivered += 1;
                            let prompt_start = Instant::now();
                            coord.deliver_prompt(&rt.mgr, &rt.tx, text, images).await;
                            tracing::debug!(
                                elapsed_ms = prompt_start.elapsed().as_millis() as u64,
                                "prompt delivered to coordinator agent"
                            );
                        }
                        ToCoordinator::Interrupt => {
                            coord.interrupt(&rt.mgr, &rt.tx).await;
                        }
                        ToCoordinator::Shutdown => {
                            // Kill all workers and sidecar before shutting down.
                            rt.kill_all_workers();
                            sidecar.shutdown(&rt.mgr);
                            coord.shutdown(&rt.mgr);
                            break;
                        }
                        ToCoordinator::StopAll => {
                            rt.kill_all_workers();
                            rt.handle_command(Command::StopAll, &mut coord).await;
                        }
                        ToCoordinator::StopWorker { task_id } => {
                            rt.kill_worker(&task_id);
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

                    let worker_duration_ms = done.session_id.as_ref().and_then(|sid| {
                        let spawn_time = rt.tracker.borrow_mut().remove(sid);
                        spawn_time.map(|t| t.elapsed().as_millis() as u64)
                    });
                    let _ = rt.orch.db().update_task_activity(&done.task_id, None);

                    let task_id_str = done.task_id.0.clone();
                    let title_str = done.title.clone();
                    let is_ok = done.result.is_ok();

                    let worker_result = process_worker_done(done, &rt.copy_mgr, &enki_dir, &rt.config.git.commit_suffix);

                    if is_ok {
                        rt.stats.workers_completed += 1;
                        tracing::info!(
                            task_id = %task_id_str, title = %title_str,
                            duration_ms = worker_duration_ms.unwrap_or(0),
                            "worker completed"
                        );
                    } else {
                        rt.stats.workers_failed += 1;
                        tracing::warn!(
                            task_id = %task_id_str, title = %title_str,
                            duration_ms = worker_duration_ms.unwrap_or(0),
                            "worker failed"
                        );
                    }

                    rt.handle_command(Command::WorkerDone(worker_result), &mut coord).await;
                }

                done = merger_done_rx.recv() => {
                    let Some(done) = done else { continue };
                    merge_in_progress = false;

                    let mr_id_str = done.merge_request_id.0.clone();
                    let merge_duration_ms = rt.merge_start_times.remove(&mr_id_str)
                        .map(|t| t.elapsed().as_millis() as u64);

                    let landed = matches!(done.outcome, enki_core::refinery::MergeOutcome::Merged);

                    let events = rt.orch.handle(Command::MergeDone(MergeResult {
                        mr_id: done.merge_request_id,
                        outcome: done.outcome,
                    }));

                    if landed {
                        rt.stats.merges_landed += 1;
                    } else {
                        rt.stats.merges_failed += 1;
                    }
                    tracing::info!(
                        mr_id = %mr_id_str,
                        landed,
                        duration_ms = merge_duration_ms.unwrap_or(0),
                        "merge completed"
                    );

                    rt.process_events(events, &mut coord).await;
                }

                done = merger_agent_done_rx.recv() => {
                    let Some(done) = done else { continue };
                    rt.tracker.borrow_mut().remove(&done.session_id);

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

                result = sidecar_done_rx.recv() => {
                    if let Some((generation, result)) = result {
                        if let Some(_prompt) = sidecar.handle_done(&rt.mgr, generation, result) {
                            let _ = rt.tx.send(FromCoordinator::SidecarCompleted);
                            coord.queue_event("- Sidecar quick task completed".to_string());
                        }
                    }
                }

                _ = poll_interval.tick() => {
                    rt.poll_tick(&mut coord, &mut sidecar, &enki_dir, &merger_done_tx,
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

        let duration_secs = rt.session_start.elapsed().as_secs();
        tracing::info!(
            duration_secs,
            workers_spawned = rt.stats.workers_spawned,
            workers_completed = rt.stats.workers_completed,
            workers_failed = rt.stats.workers_failed,
            merges_landed = rt.stats.merges_landed,
            merges_failed = rt.stats.merges_failed,
            prompts_delivered = rt.stats.prompts_delivered,
            "session summary"
        );
        tracing::info!("══════════════════ SESSION END ══════════════════");
    }
    .instrument(session_span)
    .await;
}
