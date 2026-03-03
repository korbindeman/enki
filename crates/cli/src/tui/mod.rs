mod coordinator;

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use enki_core::types::{Id, Project};
use enki_core::worktree::WorktreeManager;
use enki_tui::chat::{Chat, ChatContext, Handler};
use enki_tui::lines;
use enki_tui::Color;

use coordinator::{FromCoordinator, ToCoordinator};

const PROMPT: &str = "› ";

/// Run the chat interface. This takes over terminal input (raw mode)
/// with a pinned input bubble at the bottom.
pub async fn run(db: enki_core::db::Db, db_path: String, enki_bin: PathBuf) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let (project_cwd, status_msg) = match resolve_project(&db, &cwd) {
        ProjectResolution::Existing { path, name } => {
            (path, format!("Project: {name}"))
        }
        ProjectResolution::Registered { path, name } => {
            (path, format!("Registered new project: {name}"))
        }
        ProjectResolution::NotAGitRepo => {
            anyhow::bail!(
                "Not a git repository. Run `git init` first, or cd into an existing repo."
            );
        }
    };

    // Spawn coordinator
    let mut coord_handle = coordinator::spawn(project_cwd.clone(), db_path, enki_bin);

    let app = CoordinatorHandler {
        tx: &coord_handle.tx,
        project_cwd,
    };

    Chat::new(PROMPT)
        .title("enki", &status_msg)
        .autocomplete_trigger('@')
        .exit_confirm_timeout(Duration::from_secs(5))
        .run(app, || coord_handle.rx.try_recv().ok())?;

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
                cx.print(&lines::system("Coordinator connected. Initializing..."));
            }
            FromCoordinator::Ready => {
                cx.print(&lines::system("Coordinator ready."));
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
            }
            FromCoordinator::WorkerSpawned { task_id, title } => {
                cx.print(&lines::event(
                    "▶",
                    &format!("Worker spawned: {title} ({task_id})"),
                    Color::DarkCyan,
                ));
                cx.add_worker();
            }
            FromCoordinator::WorkerCompleted { task_id, title } => {
                cx.print(&lines::event(
                    "✓",
                    &format!("Worker completed: {title} ({task_id})"),
                    Color::Green,
                ));
                cx.remove_worker();
            }
            FromCoordinator::WorkerFailed {
                task_id,
                title,
                error,
            } => {
                cx.print(&lines::event(
                    "✗",
                    &format!("Worker failed: {title} ({task_id}): {error}"),
                    Color::Red,
                ));
                cx.remove_worker();
            }
            FromCoordinator::WorkerConflicted {
                task_id,
                title,
                worktree,
                branch: _,
            } => {
                cx.print(&lines::event_bold(
                    "⚠",
                    &format!("Merge conflict: {title} ({task_id})"),
                    Color::Yellow,
                ));
                cx.print(&lines::detail(
                    &format!("Worktree preserved at: {worktree}"),
                    Color::Yellow,
                ));
                cx.print(&lines::detail(
                    &format!("Run: enki task retry {task_id}"),
                    Color::Yellow,
                ));
                cx.remove_worker();
            }
            FromCoordinator::WorkerUpdate { .. } => {
                // Activity updates are subsumed by the worker count indicator.
            }
            FromCoordinator::Error(e) => {
                cx.finish();
                cx.print(&lines::error(&format!("error: {e}")));
                cx.clear_activity();
            }
        }
    }

    fn on_submit(&mut self, text: String, _cx: &mut ChatContext) {
        let _ = self.tx.send(ToCoordinator::Prompt(text));
    }

    fn on_quit(&mut self) {
        let _ = self.tx.send(ToCoordinator::Shutdown);
    }

    fn autocomplete(&self, query: &str) -> Vec<String> {
        complete_files(&self.project_cwd, query)
    }
}

enum ProjectResolution {
    Existing { path: PathBuf, name: String },
    Registered { path: PathBuf, name: String },
    NotAGitRepo,
}

fn resolve_project(db: &enki_core::db::Db, cwd: &Path) -> ProjectResolution {
    // Check if CWD is inside an already-registered project.
    if let Ok(projects) = db.list_projects() {
        if let Some(project) = projects
            .iter()
            .find(|p| cwd.starts_with(&p.local_path) || Path::new(&p.local_path) == cwd)
        {
            return ProjectResolution::Existing {
                path: PathBuf::from(&project.local_path),
                name: project.name.clone(),
            };
        }
    }

    // Not registered — auto-register if this is a git repo.
    if !cwd.join(".git").exists() {
        return ProjectResolution::NotAGitRepo;
    }

    let canonical = match std::fs::canonicalize(cwd) {
        Ok(p) => p,
        Err(_) => return ProjectResolution::NotAGitRepo,
    };

    let name = canonical
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let bare_path = canonical.join(".enki.git");
    if !bare_path.exists() {
        if let Err(e) = WorktreeManager::init_bare(&canonical, &bare_path) {
            eprintln!("warning: failed to init bare repo: {e}");
            return ProjectResolution::NotAGitRepo;
        }
    }

    let project = Project {
        id: Id::new("proj"),
        name: name.clone(),
        repo_url: None,
        local_path: canonical.to_string_lossy().to_string(),
        bare_repo: bare_path.to_string_lossy().to_string(),
        created_at: Utc::now(),
    };

    if let Err(e) = db.insert_project(&project) {
        eprintln!("warning: failed to register project: {e}");
        return ProjectResolution::NotAGitRepo;
    }

    ProjectResolution::Registered {
        path: canonical,
        name,
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
