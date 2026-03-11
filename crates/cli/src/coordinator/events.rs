use std::collections::{HashMap, HashSet};
use std::time::Instant;

use enki_core::orchestrator::{Command, Event, MergeResult, WorkerOutcome, WorkerResult};
use enki_core::types::{Id, MergeStatus, short_id};
use tokio::sync::mpsc;

use super::session::CoordinatorSession;
use super::sidecar::SidecarSession;
use super::workers::{self, MergerDone, try_dispatch_merge};
use super::spawning::WorkerPrep;
use super::{FromCoordinator, Runtime};

impl Runtime {
    /// Process orchestrator events: spawn workers, forward TUI messages, queue status updates.
    pub(super) async fn process_events(
        &mut self,
        initial_events: Vec<Event>,
        coord: &mut CoordinatorSession,
    ) {
        let mut events = initial_events;

        while !events.is_empty() {
            let batch = std::mem::take(&mut events);

            // Partition: collect SpawnWorker events for parallel processing.
            let mut spawn_events = Vec::new();

            for event in batch {
                match event {
                    Event::SpawnWorker {
                        task_id, title, description, tier,
                        execution_id, step_id, upstream_outputs, role,
                    } => {
                        spawn_events.push((
                            task_id, title, description, tier,
                            execution_id, step_id, upstream_outputs, role,
                        ));
                    }
                    Event::KillSession { session_id } => {
                        self.mgr.kill_session(&session_id);
                        self.tracker.borrow_mut().remove(&session_id);
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
                        let mut msg = format!("- Task \"{title}\" ({}) failed: {error}", short_id(&task_id));
                        if let Some((log_path, excerpt)) = workers::read_session_log_excerpt(&task_id) {
                            msg.push_str(&format!(
                                "\n  Session log tail (full log: {log_path}):\n{excerpt}"
                            ));
                        }
                        coord.queue_event(msg);
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
                    Event::MergeNeedsResolution {
                        mr_id, task_id, temp_dir, default_branch,
                        conflict_files, conflict_diff,
                    } => {
                        coord.queue_event(format!(
                            "- Merge conflict on task {} — spawning merger agent to resolve ({} file(s))",
                            short_id(&task_id.0), conflict_files.len()
                        ));

                        // Get the task description for context.
                        let task_desc = self.orch.db().get_task(&task_id)
                            .map(|t| {
                                let desc = t.description.unwrap_or_default();
                                format!("Task: {}\n{}", t.title, desc)
                            })
                            .unwrap_or_else(|_| format!("Task: {}", task_id));

                        match self.spawn_merger_agent(
                            &mr_id, &task_id, &temp_dir, &default_branch,
                            &conflict_files, &conflict_diff, &task_desc,
                        ).await {
                            Ok(()) => {
                                tracing::info!(
                                    mr_id, task_id = %task_id,
                                    "merger agent spawned for conflict resolution"
                                );
                            }
                            Err(e) => {
                                tracing::error!(mr_id, error = %e, "failed to spawn merger agent");
                                // Clean up temp dir and report failure.
                                let _ = std::fs::remove_dir_all(&temp_dir);
                                let more = self.orch.handle(Command::MergeDone(MergeResult {
                                    mr_id: Id(mr_id),
                                    outcome: enki_core::refinery::MergeOutcome::Failed(
                                        format!("failed to spawn merger agent: {e}")
                                    ),
                                }));
                                events.extend(more);
                            }
                        }
                    }
                    Event::MergeFailed { mr_id, task_id, branch, reason } => {
                        let _ = self.tx.send(FromCoordinator::MergeFailed {
                            mr_id: mr_id.clone(), task_id: task_id.clone(),
                            branch: branch.clone(), reason: reason.clone(),
                        });
                        coord.queue_event(format!("- Merge {mr_id} failed: {reason}"));
                    }
                    Event::ExecutionComplete { execution_id } => {
                        tracing::info!(execution_id = %execution_id, "execution completed");
                        coord.queue_event(format!("- Execution {execution_id} completed successfully"));
                    }
                    Event::ExecutionFailed { execution_id } => {
                        tracing::warn!(execution_id = %execution_id, "execution failed");
                        coord.queue_event(format!("- Execution {execution_id} failed"));
                    }
                    Event::AllStopped { count } => {
                        let _ = self.tx.send(FromCoordinator::AllStopped { count });
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

            // --- Parallel SpawnWorker processing ---
            // Phase 1: Sequential sync prep (worktree creation, DB writes).
            // Phase 2: Parallel ACP session creation (spawn_local runs concurrently).
            // Phase 3: Sequential finalization (tracker, prompt dispatch).
            let spawn_batch_start = Instant::now();
            let spawn_batch_count = spawn_events.len();
            let mut launches: Vec<(WorkerPrep, tokio::task::JoinHandle<enki_acp::Result<String>>)> = Vec::new();

            for (task_id, title, description, tier, execution_id, step_id, upstream_outputs, role) in spawn_events {
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

                match self.prepare_worker(&task_id, role.as_deref()) {
                    Ok(prep_result) => {
                        // Launch ACP session creation immediately (runs concurrently on LocalSet).
                        let mgr = self.mgr.clone();
                        let enki_bin = self.enki_bin.clone();
                        let copy_path = prep_result.copy_path.clone();
                        let mcp_args = prep_result.mcp_args.clone();
                        let task_label = task_id.0.clone();
                        let sonnet_only = self.config.workers.sonnet_only;
                        let agent_program = prep_result.agent_program.clone();
                        let agent_args = prep_result.agent_args.clone();
                        let agent_env = prep_result.agent_env.clone();

                        let handle = tokio::task::spawn_local(async move {
                            let worker_mcp = vec![enki_acp::acp_schema::McpServer::Stdio(
                                enki_acp::acp_schema::McpServerStdio::new("enki", &enki_bin)
                                    .args(mcp_args),
                            )];
                            let args_ref: Vec<&str> = agent_args.iter().map(|s| s.as_str()).collect();
                            mgr.start_session_with_mcp(
                                &agent_program, &args_ref,
                                copy_path, worker_mcp, &task_label, sonnet_only,
                                &agent_env,
                            ).await
                        });

                        launches.push((
                            WorkerPrep {
                                task_id, title, description, tier,
                                execution_id, step_id, upstream_outputs, role,
                                branch: prep_result.branch,
                                copy_path: prep_result.copy_path,
                                base_commit: prep_result.base_commit,
                                artifact: prep_result.artifact,
                            },
                            handle,
                        ));
                    }
                    Err(e) => {
                        let error = e.to_string();
                        tracing::error!(task_id = %task_id, error = %error, "failed to prepare worker");
                        if error.contains("worktree") || error.contains("not found") {
                            self.infra_broken = true;
                        }
                        let _ = self.tx.send(FromCoordinator::WorkerFailed {
                            task_id: task_id.0.clone(),
                            title: title.clone(),
                            error: error.clone(),
                        });
                        coord.queue_event(format!(
                            "- Task \"{}\" ({}) failed to spawn: {}", title, task_id, error
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

            // Phase 3: Await all ACP sessions and finalize.
            for (prep, handle) in launches {
                let result = handle.await.expect("spawn_local panicked");
                match result {
                    Ok(session_id) => {
                        self.stats.workers_spawned += 1;
                        self.finalize_worker_spawn(prep, session_id, coord);
                    }
                    Err(e) => {
                        let error = e.to_string();
                        tracing::error!(task_id = %prep.task_id, error = %error, "failed to spawn worker");
                        if error.contains("worktree") || error.contains("not found") {
                            self.infra_broken = true;
                        }
                        let _ = self.tx.send(FromCoordinator::WorkerFailed {
                            task_id: prep.task_id.0.clone(),
                            title: prep.title.clone(),
                            error: error.clone(),
                        });
                        coord.queue_event(format!(
                            "- Task \"{}\" ({}) failed to spawn: {}", prep.title, prep.task_id, error
                        ));
                        let more = self.orch.handle(Command::WorkerDone(WorkerResult {
                            task_id: prep.task_id, execution_id: Some(prep.execution_id),
                            step_id: Some(prep.step_id), title: prep.title, branch: String::new(),
                            outcome: WorkerOutcome::Failed { error },
                        }));
                        events.extend(more);
                    }
                }
            }

            if spawn_batch_count > 0 {
                tracing::info!(
                    count = spawn_batch_count,
                    elapsed_ms = spawn_batch_start.elapsed().as_millis() as u64,
                    "worker spawn batch completed"
                );
            }
        }
    }

    pub(super) async fn poll_tick(
        &mut self,
        coord: &mut CoordinatorSession,
        sidecar: &mut SidecarSession,
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

        // Intercept quick_task signal files BEFORE CheckSignals (which deletes all signal files).
        let events_dir = enki_dir.join("events");
        if let Ok(entries) = std::fs::read_dir(&events_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                // Peek at the content to check if it's a quick_task signal.
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(signal) = serde_json::from_str::<serde_json::Value>(&content) {
                        if signal.get("type").and_then(|v| v.as_str()) == Some("quick_task") {
                            // Consume the signal file before CheckSignals sees it.
                            std::fs::remove_file(&path).ok();
                            if let Some(prompt) = signal["prompt"].as_str() {
                                tracing::info!("sidecar quick task dispatched");
                                let _ = self.tx.send(FromCoordinator::SidecarStarted {
                                    prompt: prompt.to_string(),
                                });
                                sidecar.dispatch(&self.mgr, prompt.to_string());
                            }
                        }
                    }
                }
            }
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


        // Dispatch queued merge requests.
        if !*merge_in_progress {
            try_dispatch_merge(
                self.orch.db(), &self.db_path, &self.copy_mgr,
                merger_done_tx, merge_in_progress,
                &self.config.git.commit_suffix,
                &mut self.merge_start_times,
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

        // Expire old messages.
        let _ = self.orch.db().delete_expired_messages();
    }
}
