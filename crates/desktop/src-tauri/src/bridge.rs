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
pub fn stop_worker(task_id: String, state: tauri::State<CoordinatorState>) -> Result<(), String> {
    state.tx.lock().unwrap()
        .send(ToCoordinator::StopWorker { task_id })
        .map_err(|_| "coordinator channel closed".to_string())
}

#[tauri::command]
pub async fn set_agent(
    agent: String,
    state: tauri::State<'_, CoordinatorState>,
    app: AppHandle,
) -> Result<(), String> {
    let cwd = state.cwd.lock().unwrap().clone();
    if cwd.is_empty() {
        return Err("no project open".into());
    }

    let enki_dir = PathBuf::from(&cwd).join(".enki");
    let db_path = enki_dir.join("db.sqlite");

    // Shutdown old coordinator.
    state.tx.lock().unwrap().send(ToCoordinator::Shutdown).ok();
    if let Some(handle) = state.join_handle.lock().unwrap().take() {
        handle.join().ok();
    }

    // Spawn new coordinator with the selected agent.
    let enki_bin = find_enki_bin();
    let CoordinatorHandle { tx, rx, join_handle } =
        enki::coordinator::spawn(
            PathBuf::from(&cwd),
            db_path.to_str().unwrap().to_string(),
            enki_bin,
            Some(agent),
        );

    *state.tx.lock().unwrap() = tx;
    *state.join_handle.lock().unwrap() = join_handle;

    // Start relaying events from the new coordinator.
    tauri::async_runtime::spawn(event_relay(app, rx));

    Ok(())
}

#[tauri::command]
pub fn get_project_dir(state: tauri::State<CoordinatorState>) -> String {
    state.cwd.lock().unwrap().clone()
}

#[tauri::command]
pub fn get_current_branch(state: tauri::State<CoordinatorState>) -> Result<String, String> {
    let cwd = state.cwd.lock().unwrap().clone();
    if cwd.is_empty() {
        return Err("no project open".into());
    }
    let output = std::process::Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(&cwd)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("not on a branch".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
// Backlog commands (direct SQLite CRUD)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct BacklogItemPayload {
    pub id: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

fn open_project_db(state: &CoordinatorState) -> Result<enki_core::db::Db, String> {
    let cwd = state.cwd.lock().unwrap().clone();
    if cwd.is_empty() {
        return Err("no project open".into());
    }
    let db_path = PathBuf::from(&cwd).join(".enki").join("db.sqlite");
    enki_core::db::Db::open(db_path.to_str().unwrap()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn backlog_add(body: String, state: tauri::State<CoordinatorState>) -> Result<String, String> {
    let db = open_project_db(&state)?;
    let session_id = std::env::var("ENKI_SESSION_ID").unwrap_or_else(|_| "desktop".into());
    let now = chrono::Utc::now();
    let item = enki_core::types::BacklogItem {
        id: enki_core::types::Id::new("bl"),
        session_id,
        body,
        created_at: now,
        updated_at: now,
    };
    db.insert_backlog_item(&item).map_err(|e| e.to_string())?;
    Ok(item.id.0)
}

#[tauri::command]
pub fn backlog_list(state: tauri::State<CoordinatorState>) -> Result<Vec<BacklogItemPayload>, String> {
    let db = open_project_db(&state)?;
    let items = db.list_all_backlog_items().map_err(|e| e.to_string())?;
    Ok(items
        .into_iter()
        .map(|item| BacklogItemPayload {
            id: item.id.0,
            body: item.body,
            created_at: item.created_at.to_rfc3339(),
            updated_at: item.updated_at.to_rfc3339(),
        })
        .collect())
}

#[tauri::command]
pub fn backlog_update(id: String, body: String, state: tauri::State<CoordinatorState>) -> Result<(), String> {
    let db = open_project_db(&state)?;
    db.update_backlog_item(&enki_core::types::Id(id), &body)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn backlog_remove(id: String, state: tauri::State<CoordinatorState>) -> Result<(), String> {
    let db = open_project_db(&state)?;
    db.delete_backlog_item(&enki_core::types::Id(id))
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// File explorer commands
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub child_count: Option<u32>,
    pub is_hidden: bool,
    pub is_gitignored: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectoryListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextFileContent {
    pub content: String,
    pub language: String,
    pub is_markdown: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageFileContent {
    pub data: String,
    pub mime_type: String,
}

fn build_gitignore(dir: &std::path::Path) -> Option<ignore::gitignore::Gitignore> {
    // Walk up to find the git repo root (directory containing .git).
    let mut root = dir.to_path_buf();
    loop {
        if root.join(".git").exists() {
            break;
        }
        if !root.pop() {
            return None;
        }
    }
    let gitignore_path = root.join(".gitignore");
    if !gitignore_path.exists() {
        return None;
    }
    let (gi, err) = ignore::gitignore::Gitignore::new(&gitignore_path);
    if let Some(e) = err {
        tracing::debug!("gitignore parse error: {e}");
    }
    Some(gi)
}

#[tauri::command]
pub fn list_directory(path: String) -> Result<DirectoryListing, String> {
    let dir = std::path::Path::new(&path);
    if !dir.is_dir() {
        return Err(format!("{path} is not a directory"));
    }
    let abs_path = dir.canonicalize().map_err(|e| e.to_string())?;
    let gitignore = build_gitignore(&abs_path);

    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&abs_path).map_err(|e| e.to_string())?;

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();
        let size = if is_dir { 0 } else { metadata.len() };
        let is_hidden = name.starts_with('.');

        let child_count = if is_dir {
            match std::fs::read_dir(entry.path()) {
                Ok(rd) => Some(rd.count() as u32),
                Err(_) => None,
            }
        } else {
            None
        };

        let is_gitignored = gitignore.as_ref().is_some_and(|gi| {
            gi.matched_path_or_any_parents(entry.path(), is_dir)
                .is_ignore()
        });

        entries.push(FileEntry { name, is_dir, size, child_count, is_hidden, is_gitignored });
    }

    // Sort: directories first, then alphabetical case-insensitive.
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(DirectoryListing {
        path: abs_path.to_string_lossy().to_string(),
        entries,
    })
}

fn language_from_extension(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "css" => "css",
        "scss" => "scss",
        "html" | "htm" => "html",
        "sql" => "sql",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "md" | "mdx" => "markdown",
        "xml" => "xml",
        "c" => "c",
        "cpp" | "cc" | "cxx" => "cpp",
        "h" | "hpp" => "cpp",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "zig" => "zig",
        "vue" => "vue",
        "svelte" => "svelte",
        "graphql" | "gql" => "graphql",
        "proto" => "protobuf",
        "dockerfile" => "dockerfile",
        "tf" => "hcl",
        "nix" => "nix",
        "el" | "lisp" | "cl" => "lisp",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "csv" => "csv",
        "ini" | "cfg" => "ini",
        "diff" | "patch" => "diff",
        "log" => "log",
        "txt" => "plaintext",
        _ => "plaintext",
    }
}

#[tauri::command]
pub fn read_text_file(path: String) -> Result<TextFileContent, String> {
    let file_path = std::path::Path::new(&path);

    let metadata = std::fs::metadata(file_path).map_err(|e| e.to_string())?;
    if metadata.len() > 1_000_000 {
        return Err("file is too large to display (>1MB)".into());
    }

    let bytes = std::fs::read(file_path).map_err(|e| e.to_string())?;

    // Check for binary content (null bytes in first 8KB).
    let check_len = bytes.len().min(8192);
    if bytes[..check_len].contains(&0) {
        return Err("file appears to be binary".into());
    }

    let content = String::from_utf8(bytes)
        .map_err(|_| "file is not valid UTF-8".to_string())?;

    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Handle extensionless files by name.
    let language = if ext.is_empty() {
        let name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();
        match name.as_str() {
            "dockerfile" => "dockerfile",
            "makefile" | "gnumakefile" => "makefile",
            "justfile" => "just",
            _ => "plaintext",
        }
    } else {
        language_from_extension(&ext)
    };

    let is_markdown = ext == "md" || ext == "mdx";

    Ok(TextFileContent { content, language: language.to_string(), is_markdown })
}

fn mime_from_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "svg" => Some("image/svg+xml"),
        "webp" => Some("image/webp"),
        "ico" => Some("image/x-icon"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

#[tauri::command]
pub fn read_image_file(path: String) -> Result<ImageFileContent, String> {
    let file_path = std::path::Path::new(&path);

    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let mime_type = mime_from_extension(&ext)
        .ok_or_else(|| format!("unsupported image format: .{ext}"))?;

    let metadata = std::fs::metadata(file_path).map_err(|e| e.to_string())?;
    if metadata.len() > 10_000_000 {
        return Err("image is too large to display (>10MB)".into());
    }

    let bytes = std::fs::read(file_path).map_err(|e| e.to_string())?;

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(ImageFileContent { data, mime_type: mime_type.to_string() })
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
    pub local_workers: bool,
    pub ollama_host: String,
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
    let worker_override = config.agent.overrides.get("worker");
    let local_workers = worker_override
        .and_then(|w| w.command.as_deref())
        .map(|cmd| cmd == "opencode")
        .unwrap_or(false);
    let ollama_host = worker_override
        .and_then(|w| w.env.as_ref())
        .and_then(|env| env.get("OLLAMA_HOST"))
        .cloned()
        .unwrap_or_default();
    Ok(ConfigPayload {
        commit_suffix: config.git.commit_suffix,
        max_workers: config.workers.limits.max_workers,
        max_heavy: config.workers.limits.max_heavy,
        max_standard: config.workers.limits.max_standard,
        max_light: config.workers.limits.max_light,
        sonnet_only: config.workers.sonnet_only,
        local_workers,
        ollama_host,
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

    // [agent.worker]
    if config.local_workers {
        let mut worker_fields = vec![format!("command = {:?}", "opencode")];
        if !config.ollama_host.is_empty() {
            worker_fields.push(format!("env = {{ OLLAMA_HOST = {:?} }}", config.ollama_host));
        }
        sections.push(format!("[agent.worker]\n{}", worker_fields.join("\n")));
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
    MergerSpawned { task_id: String, title: String, conflict_files: Vec<String> },
    WorkerCount { count: usize },
    AllStopped { count: usize },
    Mail { from: String, to: String, subject: String, priority: String },
    SidecarStarted { prompt: String },
    SidecarUpdate { activity: WorkerActivityEvent },
    SidecarCompleted,
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
        FromCoordinator::MergerSpawned { task_id, title, conflict_files } =>
            CoordinatorEvent::MergerSpawned { task_id, title, conflict_files },
        FromCoordinator::WorkerCount(count) =>
            CoordinatorEvent::WorkerCount { count },
        FromCoordinator::AllStopped { count } =>
            CoordinatorEvent::AllStopped { count },
        FromCoordinator::Mail { from, to, subject, priority } =>
            CoordinatorEvent::Mail { from, to, subject, priority },
        FromCoordinator::SidecarStarted { prompt } =>
            CoordinatorEvent::SidecarStarted { prompt },
        FromCoordinator::SidecarUpdate { activity } => {
            let activity = match activity {
                WorkerActivity::ToolStarted(name) => WorkerActivityEvent::ToolStarted { name },
                WorkerActivity::ToolDone => WorkerActivityEvent::ToolDone,
                WorkerActivity::Thinking => WorkerActivityEvent::Thinking,
            };
            CoordinatorEvent::SidecarUpdate { activity }
        }
        FromCoordinator::SidecarCompleted => CoordinatorEvent::SidecarCompleted,
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
