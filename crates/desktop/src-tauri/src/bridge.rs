use std::path::PathBuf;
use std::sync::Mutex;

use enki::coordinator::{CoordinatorHandle, FromCoordinator, ImageData, ToCoordinator, WorkerActivity};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

/// Tauri-managed state: coordinator channel + project info.
/// All fields are behind Mutex so the coordinator can be replaced on project switch.
pub struct CoordinatorState {
    tx: Mutex<mpsc::UnboundedSender<ToCoordinator>>,
    join_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    cwd: Mutex<String>,
}

// ---------------------------------------------------------------------------
// Tauri commands (frontend → Rust)
// ---------------------------------------------------------------------------

/// Image payload received from the frontend (base64-encoded).
#[derive(Debug, Clone, Deserialize)]
pub struct ImagePayload {
    pub data: String,
    pub mime_type: String,
}

#[tauri::command]
pub fn send_prompt(
    text: String,
    images: Option<Vec<ImagePayload>>,
    state: tauri::State<CoordinatorState>,
) -> Result<(), String> {
    use base64::Engine;
    let images = images
        .unwrap_or_default()
        .into_iter()
        .map(|img| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&img.data)
                .map_err(|e| format!("invalid base64 image data: {e}"))?;
            Ok(ImageData { bytes, mime_type: img.mime_type })
        })
        .collect::<Result<Vec<_>, String>>()?;
    state.tx.lock().unwrap()
        .send(ToCoordinator::Prompt { text, images })
        .map_err(|_| "coordinator channel closed".to_string())
}

#[tauri::command]
pub fn interrupt(state: tauri::State<CoordinatorState>) -> Result<(), String> {
    state.tx.lock().unwrap()
        .send(ToCoordinator::Interrupt)
        .map_err(|_| "coordinator channel closed".to_string())
}

#[tauri::command]
pub fn stop_all(state: tauri::State<CoordinatorState>) -> Result<(), String> {
    state.tx.lock().unwrap()
        .send(ToCoordinator::StopAll)
        .map_err(|_| "coordinator channel closed".to_string())
}

#[tauri::command]
pub fn get_project_dir(state: tauri::State<CoordinatorState>) -> String {
    state.cwd.lock().unwrap().clone()
}

#[tauri::command]
pub async fn open_project(
    path: String,
    state: tauri::State<'_, CoordinatorState>,
    app: AppHandle,
) -> Result<(), String> {
    let cwd = PathBuf::from(&path);
    let enki_dir = cwd.join(".enki");
    let db_path = enki_dir.join("db.sqlite");

    // Auto-initialize project if needed.
    if !db_path.exists() {
        std::fs::create_dir_all(&enki_dir).map_err(|e| e.to_string())?;
        enki_core::db::Db::open(db_path.to_str().unwrap()).map_err(|e| e.to_string())?;
    }

    // Shutdown old coordinator.
    state.tx.lock().unwrap().send(ToCoordinator::Shutdown).ok();
    if let Some(handle) = state.join_handle.lock().unwrap().take() {
        handle.join().ok();
    }

    // Spawn new coordinator for the selected project.
    let enki_bin = find_enki_bin();
    let CoordinatorHandle { tx, rx, join_handle } =
        enki::coordinator::spawn(cwd, db_path.to_str().unwrap().to_string(), enki_bin);

    *state.tx.lock().unwrap() = tx;
    *state.join_handle.lock().unwrap() = join_handle;
    *state.cwd.lock().unwrap() = path;

    // Start relaying events from the new coordinator.
    tauri::async_runtime::spawn(event_relay(app, rx));

    Ok(())
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
    // Use ENKI_PROJECT_DIR if set (e.g. `just desktop`), otherwise fall back to CWD.
    let cwd = match std::env::var("ENKI_PROJECT_DIR") {
        Ok(dir) => std::path::PathBuf::from(dir),
        Err(_) => std::env::current_dir()?,
    };
    let enki_dir = cwd.join(".enki");
    let db_path = enki_dir.join("db.sqlite");

    // Auto-initialize project if needed.
    if !db_path.exists() {
        std::fs::create_dir_all(&enki_dir)?;
        enki_core::db::Db::open(db_path.to_str().unwrap())?;
    }

    let db_path_str = db_path.to_str().unwrap().to_string();
    let cwd_str = cwd.to_string_lossy().to_string();

    // Resolve the enki CLI binary — NOT current_exe(), which is the desktop app
    // and would cause infinite window spawning when the coordinator launches
    // `enki mcp` subprocesses.
    let enki_bin = find_enki_bin();

    let CoordinatorHandle { tx, rx, join_handle } =
        enki::coordinator::spawn(cwd, db_path_str, enki_bin);

    app.manage(CoordinatorState {
        tx: Mutex::new(tx),
        join_handle: Mutex::new(join_handle),
        cwd: Mutex::new(cwd_str),
    });

    // Spawn the event relay task on Tauri's async runtime.
    let handle = app.clone();
    tauri::async_runtime::spawn(event_relay(handle, rx));

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

/// Find the `enki` CLI binary on PATH or at the default cargo install location.
fn find_enki_bin() -> std::path::PathBuf {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("enki");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cargo/bin/enki")
}
