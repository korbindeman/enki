use std::path::{Path, PathBuf};
use std::time::Duration;

use enki::coordinator::{self, FromCoordinator, ToCoordinator, WorkerActivity};
use enki_tui::chat::{Chat, ChatContext, Handler, UserInput};
use enki_tui::lines;
use enki_tui::{Color, KeyCode, KeyModifiers};
use tokio::sync::mpsc::error::TryRecvError;

const PROMPT: &str = "› ";

/// Run the chat interface. This takes over terminal input (raw mode)
/// with a pinned input bubble at the bottom.
pub async fn run(_db: enki_core::db::Db, db_path: String, enki_bin: PathBuf, agent_override: Option<String>) -> anyhow::Result<()> {
    let project_cwd = std::env::current_dir()?;
    let project_name = project_cwd
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let config = enki_core::config::load_config(&project_cwd);
    let agent_name = agent_override.as_deref().unwrap_or(&config.agent.command);
    let status_msg = if config.workers.sonnet_only {
        format!("{project_name} ({agent_name}, sonnet mode)")
    } else {
        format!("{project_name} ({agent_name})")
    };

    // Spawn coordinator
    let mut coord_handle = coordinator::spawn(project_cwd.clone(), db_path, enki_bin, agent_override);

    let app = CoordinatorHandler {
        tx: &coord_handle.tx,
        project_cwd,
    };

    // Track whether the coordinator channel has disconnected (panic or exit).
    let mut disconnected = false;

    Chat::new(PROMPT)
        .title("enki", &status_msg)
        .autocomplete_trigger('@')
        .exit_confirm_timeout(Duration::from_secs(5))
        .run(app, || {
            if disconnected {
                return None;
            }
            match coord_handle.rx.try_recv() {
                Ok(msg) => Some(msg),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    // Channel closed — coordinator exited or panicked.
                    // Any panic error was already sent on the channel before disconnect.
                    None
                }
            }
        })?;

    // Wait for coordinator thread to finish cleanup.
    if let Some(handle) = coord_handle.join_handle.take() {
        let _ = handle.join();
    }

    Ok(())
}

struct CoordinatorHandler<'a> {
    tx: &'a tokio::sync::mpsc::UnboundedSender<ToCoordinator>,
    project_cwd: PathBuf,
}

impl Handler<FromCoordinator> for CoordinatorHandler<'_> {
    fn on_message(&mut self, msg: FromCoordinator, cx: &mut ChatContext) {
        match msg {
            FromCoordinator::Connected => {
                cx.print_or_update("coordinator:init", &lines::system("Coordinator initializing..."));
            }
            FromCoordinator::Ready => {
                cx.print_or_update("coordinator:init", &lines::system("Coordinator ready."));
            }
            FromCoordinator::Text(text) => {
                cx.stream(&text);
            }
            FromCoordinator::ToolCall(name) => {
                cx.tool(name);
            }
            FromCoordinator::ToolCallDone(_) => {
                cx.think();
            }
            FromCoordinator::Done(_reason) => {
                cx.finish_markdown();
                cx.clear_activity();
                cx.notify("Enki is waiting for input");
            }
            FromCoordinator::WorkerSpawned { task_id, title, tier, .. } => {
                cx.print(&lines::event(
                    "▶",
                    &format!("Worker spawned: {title} ({})", enki_core::types::short_id(&task_id)),
                    Color::DarkCyan,
                ));
                cx.add_worker();
                cx.panel_add(&task_id, &title, &tier);
            }
            FromCoordinator::WorkerCompleted { task_id, title } => {
                cx.print(&lines::event(
                    "✓",
                    &format!("Worker completed: {title} ({})", enki_core::types::short_id(&task_id)),
                    Color::Green,
                ));
                cx.remove_worker();
                cx.panel_remove(&task_id);
            }
            FromCoordinator::WorkerFailed {
                task_id,
                title,
                error,
            } => {
                cx.print(&lines::event(
                    "✗",
                    &format!("Worker failed: {title} ({}): {error}", enki_core::types::short_id(&task_id)),
                    Color::Red,
                ));
                cx.remove_worker();
                cx.panel_remove(&task_id);
            }
            FromCoordinator::MergerSpawned { task_id, title, conflict_files } => {
                cx.print(&lines::event(
                    "⚙",
                    &format!("Merger spawned: {title} ({}) — {} file(s)",
                        enki_core::types::short_id(&task_id), conflict_files.len()),
                    Color::Yellow,
                ));
                cx.panel_add(&task_id, &title, "light");
                cx.panel_set_activity(&task_id,
                    &format!("Resolving {} conflict(s)", conflict_files.len()));
            }
            FromCoordinator::MergeQueued { mr_id: _, task_id: _, branch } => {
                let tag = format!("merge:{branch}");
                cx.print_or_update(&tag, &lines::event(
                    "⊕",
                    &format!("Merge queued: {branch}"),
                    Color::DarkCyan,
                ));
            }
            FromCoordinator::MergeLanded { mr_id: _, task_id, branch } => {
                let tag = format!("merge:{branch}");
                cx.print_or_update(&tag, &lines::event(
                    "✓",
                    &format!("Merge landed: {branch}"),
                    Color::Green,
                ));
                cx.panel_remove(&task_id);
            }
            FromCoordinator::MergeConflicted { mr_id: _, task_id: _, branch } => {
                let tag = format!("merge:{branch}");
                cx.print_or_update(&tag, &lines::event_bold(
                    "⚠",
                    &format!("Merge conflict: {branch}"),
                    Color::Yellow,
                ));
                cx.notify(&format!("Merge conflict: {branch}"));
            }
            FromCoordinator::MergeFailed { mr_id: _, task_id, branch, reason } => {
                let tag = format!("merge:{branch}");
                cx.print_or_update(&tag, &lines::event(
                    "✗",
                    &format!("Merge failed: {branch}: {reason}"),
                    Color::Red,
                ));
                cx.panel_remove(&task_id);
            }
            FromCoordinator::MergeProgress { mr_id: _, task_id: _, branch, status } => {
                let tag = format!("merge:{branch}");
                cx.print_or_update(&tag, &lines::event(
                    "⊕",
                    &format!("Merge {status}: {branch}"),
                    Color::DarkCyan,
                ));
            }
            FromCoordinator::AllStopped { count } => {
                let msg = if count == 0 {
                    "No workers were running.".to_string()
                } else {
                    format!("Stopped {} worker{}.", count, if count == 1 { "" } else { "s" })
                };
                cx.print(&lines::event("■", &msg, Color::Yellow));
                cx.reset_workers();
                cx.notify(&msg);
            }
            FromCoordinator::WorkerCount(count) => {
                cx.set_worker_count(count);
            }
            FromCoordinator::WorkerUpdate { task_id, activity } => {
                let text = match activity {
                    WorkerActivity::ToolStarted(name) => name,
                    WorkerActivity::ToolDone => "Thinking".to_string(),
                    WorkerActivity::Thinking => "Thinking".to_string(),
                };
                cx.panel_set_activity(&task_id, &text);
            }
            FromCoordinator::WorkerReport { task_id, status } => {
                cx.panel_set_activity(&task_id, &status);
            }
            FromCoordinator::Mail { from, to, subject, priority } => {
                let prio_tag = if priority == "urgent" || priority == "high" {
                    format!(" [{}]", priority)
                } else {
                    String::new()
                };
                cx.print(&lines::event(
                    "✉",
                    &format!("{from} → {to}: {subject}{prio_tag}"),
                    Color::Cyan,
                ));
            }
            FromCoordinator::SidecarStarted { prompt } => {
                let short = if prompt.len() > 60 { &prompt[..60] } else { &prompt };
                cx.print(&lines::event(
                    "⚡",
                    &format!("Sidecar: {short}"),
                    Color::DarkCyan,
                ));
            }
            FromCoordinator::SidecarUpdate { .. } => {
                // Sidecar activity updates are silent in TUI for now.
            }
            FromCoordinator::SidecarCompleted => {
                cx.print(&lines::event(
                    "✓",
                    "Sidecar task completed",
                    Color::Green,
                ));
            }
            FromCoordinator::Interrupted => {
                cx.finish();
                cx.clear_activity();
            }
            FromCoordinator::Error(e) => {
                cx.finish();
                cx.print(&lines::error(&format!("error: {e}")));
                cx.clear_activity();
                cx.notify(&format!("Enki error: {e}"));
            }
        }
    }

    fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers, cx: &mut ChatContext) -> bool {
        if code == KeyCode::Char('w') && modifiers.contains(KeyModifiers::CONTROL) {
            cx.panel_toggle();
            return true;
        }
        false
    }

    fn on_submit(&mut self, input: UserInput, _cx: &mut ChatContext) {
        let images = input.images.into_iter().map(|img| coordinator::ImageData {
            bytes: img.bytes,
            mime_type: img.mime_type,
        }).collect();
        let _ = self.tx.send(ToCoordinator::Prompt { text: input.text, images });
    }

    fn on_interrupt(&mut self) {
        let _ = self.tx.send(ToCoordinator::Interrupt);
    }

    fn on_quit(&mut self) {
        let _ = self.tx.send(ToCoordinator::Shutdown);
    }

    fn autocomplete(&self, query: &str) -> Vec<String> {
        complete_files(&self.project_cwd, query)
    }
}

/// Fuzzy-find files and directories in the project tree.
///
/// Uses `ignore` for recursive walking (respects .gitignore) and `nucleo-matcher`
/// for fzf-style fuzzy scoring. Returns top 10 matches sorted by score.
fn complete_files(cwd: &Path, query: &str) -> Vec<String> {
    use ignore::WalkBuilder;
    use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    // Collect all paths relative to cwd
    let mut candidates: Vec<String> = Vec::new();
    let walker = WalkBuilder::new(cwd)
        .hidden(true) // skip hidden files
        .max_depth(Some(12))
        .build();

    for entry in walker.flatten() {
        // Skip the root directory itself
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let mut display = rel.to_string_lossy().to_string();
        if path.is_dir() {
            display.push('/');
        }
        candidates.push(display);
    }

    if query.is_empty() {
        // No query yet — show top-level entries only (like before)
        candidates.retain(|p| !p.trim_end_matches('/').contains('/'));
        candidates.sort();
        candidates.truncate(10);
        return candidates;
    }

    // Fuzzy match
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );

    let mut scored: Vec<(u32, String)> = candidates
        .into_iter()
        .filter_map(|path| {
            let mut buf = Vec::new();
            let haystack = Utf32Str::new(&path, &mut buf);
            let score = pattern.score(haystack, &mut matcher)?;
            Some((score, path))
        })
        .collect();

    // Sort by score descending, then alphabetically for ties
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(10);
    scored.into_iter().map(|(_, path)| path).collect()
}
