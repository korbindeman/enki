use std::sync::Mutex;

use enki::coordinator::{CoordinatorHandle, FromCoordinator, ToCoordinator, WorkerActivity};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

/// Tauri-managed state: the sender half of the coordinator channel.
pub struct CoordinatorState {
    tx: mpsc::UnboundedSender<ToCoordinator>,
    /// Keep the join handle so the coordinator thread lives as long as the app.
    _join_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

// ---------------------------------------------------------------------------
// Tauri commands (frontend → Rust)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn send_prompt(text: String, state: tauri::State<CoordinatorState>) -> Result<(), String> {
    state.tx.send(ToCoordinator::Prompt { text, images: vec![] })
        .map_err(|_| "coordinator channel closed".to_string())
}

#[tauri::command]
pub fn interrupt(state: tauri::State<CoordinatorState>) -> Result<(), String> {
    state.tx.send(ToCoordinator::Interrupt)
        .map_err(|_| "coordinator channel closed".to_string())
}

#[tauri::command]
pub fn stop_all(state: tauri::State<CoordinatorState>) -> Result<(), String> {
    state.tx.send(ToCoordinator::StopAll)
        .map_err(|_| "coordinator channel closed".to_string())
}

// ---------------------------------------------------------------------------
// Event types (Rust → frontend via Tauri events)
// ---------------------------------------------------------------------------

/// JSON event emitted to the frontend. Each variant maps to a `type` discriminator.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordinatorEvent {
    Text { content: String },
    Done { content: String },
    ToolCall { name: String },
    ToolCallDone { name: String },
    WorkerSpawned { task_id: String, title: String, tier: String },
    WorkerCompleted { task_id: String, title: String },
    WorkerFailed { task_id: String, title: String, error: String },
    WorkerUpdate { task_id: String, activity: WorkerActivityEvent },
    WorkerReport { task_id: String, status: String },
    MergeQueued { task_id: String, branch: String },
    MergeLanded { task_id: String, branch: String },
    MergeFailed { task_id: String, branch: String, reason: String },
    MergeConflicted { task_id: String, branch: String },
    MergeProgress { task_id: String, branch: String, status: String },
    WorkerCount { count: usize },
    AllStopped { count: usize },
    Mail { from: String, to: String, subject: String, priority: String },
    Connected,
    Ready,
    Interrupted,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerActivityEvent {
    ToolStarted { name: String },
    ToolDone,
    Thinking,
}

const EVENT_NAME: &str = "coordinator";

/// Convert a `FromCoordinator` channel message into a Tauri event.
fn to_event(msg: FromCoordinator) -> CoordinatorEvent {
    match msg {
        FromCoordinator::Connected => CoordinatorEvent::Connected,
        FromCoordinator::Ready => CoordinatorEvent::Ready,
        FromCoordinator::Text(content) => CoordinatorEvent::Text { content },
        FromCoordinator::Done(content) => CoordinatorEvent::Done { content },
        FromCoordinator::ToolCall(name) => CoordinatorEvent::ToolCall { name },
        FromCoordinator::ToolCallDone(name) => CoordinatorEvent::ToolCallDone { name },
        FromCoordinator::WorkerSpawned { task_id, title, tier } =>
            CoordinatorEvent::WorkerSpawned { task_id, title, tier },
        FromCoordinator::WorkerCompleted { task_id, title } =>
            CoordinatorEvent::WorkerCompleted { task_id, title },
        FromCoordinator::WorkerFailed { task_id, title, error } =>
            CoordinatorEvent::WorkerFailed { task_id, title, error },
        FromCoordinator::WorkerUpdate { task_id, activity } => {
            let activity = match activity {
                WorkerActivity::ToolStarted(name) => WorkerActivityEvent::ToolStarted { name },
                WorkerActivity::ToolDone => WorkerActivityEvent::ToolDone,
                WorkerActivity::Thinking => WorkerActivityEvent::Thinking,
            };
            CoordinatorEvent::WorkerUpdate { task_id, activity }
        }
        FromCoordinator::WorkerReport { task_id, status } =>
            CoordinatorEvent::WorkerReport { task_id, status },
        FromCoordinator::MergeQueued { mr_id: _, task_id, branch } =>
            CoordinatorEvent::MergeQueued { task_id, branch },
        FromCoordinator::MergeLanded { mr_id: _, task_id, branch } =>
            CoordinatorEvent::MergeLanded { task_id, branch },
        FromCoordinator::MergeConflicted { mr_id: _, task_id, branch } =>
            CoordinatorEvent::MergeConflicted { task_id, branch },
        FromCoordinator::MergeFailed { mr_id: _, task_id, branch, reason } =>
            CoordinatorEvent::MergeFailed { task_id, branch, reason },
        FromCoordinator::MergeProgress { mr_id: _, task_id, branch, status } =>
            CoordinatorEvent::MergeProgress { task_id, branch, status },
        FromCoordinator::WorkerCount(count) =>
            CoordinatorEvent::WorkerCount { count },
        FromCoordinator::AllStopped { count } =>
            CoordinatorEvent::AllStopped { count },
        FromCoordinator::Mail { from, to, subject, priority } =>
            CoordinatorEvent::Mail { from, to, subject, priority },
        FromCoordinator::Interrupted => CoordinatorEvent::Interrupted,
        FromCoordinator::Error(message) => CoordinatorEvent::Error { message },
    }
}

// ---------------------------------------------------------------------------
// Setup: spawn coordinator + event relay
// ---------------------------------------------------------------------------

/// Initialize the coordinator and wire it to Tauri.
///
/// Called from `tauri::Builder::setup()`. Spawns the coordinator on a dedicated
/// OS thread and starts a tokio task that relays `FromCoordinator` messages as
/// Tauri events to the frontend.
pub fn setup(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = enki::commands::project_root()?;
    let db_path = enki::commands::db_path()?;
    let db_path_str = db_path.to_str().unwrap().to_string();

    let enki_bin = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| std::path::PathBuf::from("enki"));

    // Auto-initialize project if needed.
    let enki_dir = cwd.join(".enki");
    if !enki_dir.join("db.sqlite").exists() {
        std::fs::create_dir_all(&enki_dir)?;
        let _ = enki_core::db::Db::open(enki_dir.join("db.sqlite").to_str().unwrap())?;
    }

    let CoordinatorHandle { tx, rx, join_handle } =
        enki::coordinator::spawn(cwd, db_path_str, enki_bin);

    app.manage(CoordinatorState {
        tx,
        _join_handle: Mutex::new(join_handle),
    });

    // Spawn the event relay task.
    let handle = app.clone();
    tokio::spawn(event_relay(handle, rx));

    Ok(())
}

/// Drains the `FromCoordinator` channel and emits Tauri events to the frontend.
async fn event_relay(
    app: AppHandle,
    mut rx: mpsc::UnboundedReceiver<FromCoordinator>,
) {
    while let Some(msg) = rx.recv().await {
        let event = to_event(msg);
        if let Err(e) = app.emit(EVENT_NAME, &event) {
            tracing::warn!(error = %e, "failed to emit coordinator event");
        }
    }
    tracing::info!("coordinator event relay ended");
}
