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
pub fn set_agent(agent: String, state: tauri::State<CoordinatorState>) -> Result<(), String> {
    state.tx.lock().unwrap()
        .send(ToCoordinator::SetAgent(agent))
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
        enki::coordinator::spawn(cwd, db_path.to_str().unwrap().to_string(), enki_bin, None);

    *state.tx.lock().unwrap() = tx;
    *state.join_handle.lock().unwrap() = join_handle;
    *state.cwd.lock().unwrap() = path.clone();
    save_last_project(&path);

    // Start relaying events from the new coordinator.
    tauri::async_runtime::spawn(event_relay(app, rx));

    Ok(())
}

// ---------------------------------------------------------------------------
// Config commands (settings UI)
// ---------------------------------------------------------------------------

/// Flat JSON representation of the config for the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPayload {
    pub commit_suffix: String,
    pub max_workers: usize,
    pub max_heavy: usize,
    pub max_standard: usize,
    pub max_light: usize,
    pub sonnet_only: bool,
    pub agent_command: String,
}

fn global_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("enki.toml")
}

#[tauri::command]
pub fn load_config() -> Result<ConfigPayload, String> {
    let config = enki_core::config::load_config(&PathBuf::from("/dev/null"));
    Ok(ConfigPayload {
        commit_suffix: config.git.commit_suffix,
        max_workers: config.workers.limits.max_workers,
        max_heavy: config.workers.limits.max_heavy,
        max_standard: config.workers.limits.max_standard,
        max_light: config.workers.limits.max_light,
        sonnet_only: config.workers.sonnet_only,
        agent_command: config.agent.command,
    })
}

#[tauri::command]
pub fn save_config(config: ConfigPayload) -> Result<(), String> {
    let path = global_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // Build TOML string manually to keep it clean and only include non-default values.
    let default = enki_core::config::Config::default();
    let mut sections: Vec<String> = Vec::new();

    // [git]
    if config.commit_suffix != default.git.commit_suffix {
        sections.push(format!("[git]\ncommit_suffix = {:?}", config.commit_suffix));
    }

    // [workers]
    {
        let mut fields = Vec::new();
        if config.max_workers != default.workers.limits.max_workers {
            fields.push(format!("max_workers = {}", config.max_workers));
        }
        if config.max_heavy != default.workers.limits.max_heavy {
            fields.push(format!("max_heavy = {}", config.max_heavy));
        }
        if config.max_standard != default.workers.limits.max_standard {
            fields.push(format!("max_standard = {}", config.max_standard));
        }
        if config.max_light != default.workers.limits.max_light {
            fields.push(format!("max_light = {}", config.max_light));
        }
        if config.sonnet_only != default.workers.sonnet_only {
            fields.push(format!("sonnet_only = {}", config.sonnet_only));
        }
        if !fields.is_empty() {
            sections.push(format!("[workers]\n{}", fields.join("\n")));
        }
    }

    // [agent]
    if config.agent_command != default.agent.command {
        sections.push(format!("[agent]\ncommand = {:?}", config.agent_command));
    }

    let content = if sections.is_empty() {
        String::new()
    } else {
        sections.join("\n\n") + "\n"
    };

    std::fs::write(&path, content).map_err(|e| e.to_string())
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
/// Called from `tauri::Builder::setup()`. If `ENKI_PROJECT_DIR` is set, spawns
/// the coordinator immediately. Otherwise starts without a project — the user
/// can open one later via the "Open Project" context menu.
pub fn setup(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    // GUI apps on macOS inherit a minimal PATH (just /usr/bin:/bin:/usr/sbin:/sbin).
    // Augment it with common locations for node, cargo, and other dev tools so that
    // the coordinator can find them when resolving agent binaries.
    augment_path();

    let project_dir = std::env::var("ENKI_PROJECT_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(load_last_project);

    if let Some(cwd) = project_dir {
        let enki_dir = cwd.join(".enki");
        let db_path = enki_dir.join("db.sqlite");

        if !db_path.exists() {
            std::fs::create_dir_all(&enki_dir)?;
            enki_core::db::Db::open(db_path.to_str().unwrap())?;
        }

        let db_path_str = db_path.to_str().unwrap().to_string();
        let cwd_str = cwd.to_string_lossy().to_string();
        save_last_project(&cwd_str);
        let enki_bin = find_enki_bin();

        let CoordinatorHandle { tx, rx, join_handle } =
            enki::coordinator::spawn(cwd, db_path_str, enki_bin, None);

        app.manage(CoordinatorState {
            tx: Mutex::new(tx),
            join_handle: Mutex::new(join_handle),
            cwd: Mutex::new(cwd_str),
        });

        let handle = app.clone();
        tauri::async_runtime::spawn(event_relay(handle, rx));
    } else {
        // No project directory — start with a dummy channel.
        // The user will open a project via the context menu, which calls open_project
        // and replaces these with a real coordinator.
        let (tx, _rx) = mpsc::unbounded_channel();
        app.manage(CoordinatorState {
            tx: Mutex::new(tx),
            join_handle: Mutex::new(None),
            cwd: Mutex::new(String::new()),
        });
    }

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

/// macOS GUI apps inherit a minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin).
/// Prepend well-known dev tool directories so we can find node, cargo, etc.
fn augment_path() {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };

    let extra_dirs: Vec<PathBuf> = vec![
        home.join(".cargo/bin"),
        home.join(".local/bin"),
        // nvm
        home.join(".nvm/current/bin"),
        // volta
        home.join(".volta/bin"),
        // fnm
        home.join(".local/share/fnm/aliases/default/bin"),
        // mise / rtx
        home.join(".local/share/mise/shims"),
        // homebrew
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/opt/homebrew/sbin"),
        PathBuf::from("/usr/local/bin"),
    ];

    let current = std::env::var("PATH").unwrap_or_default();
    let current_dirs: std::collections::HashSet<PathBuf> =
        std::env::split_paths(&current).collect();

    let mut prepend: Vec<PathBuf> = extra_dirs
        .into_iter()
        .filter(|d| d.is_dir() && !current_dirs.contains(d))
        .collect();

    if prepend.is_empty() {
        return;
    }

    prepend.extend(std::env::split_paths(&current));
    let new_path = std::env::join_paths(&prepend).unwrap_or_default();
    // SAFETY: called once at startup before any threads are spawned.
    unsafe { std::env::set_var("PATH", &new_path) };
}

// ---------------------------------------------------------------------------
// Last-project persistence (~/.enki/last-project)
// ---------------------------------------------------------------------------

fn last_project_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".enki").join("last-project"))
}

fn load_last_project() -> Option<PathBuf> {
    let path = PathBuf::from(std::fs::read_to_string(last_project_path()?).ok()?.trim());
    if path.is_dir() { Some(path) } else { None }
}

fn save_last_project(dir: &str) {
    if let Some(p) = last_project_path() {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(p, dir).ok();
    }
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
