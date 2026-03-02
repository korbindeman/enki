mod coordinator;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use enki_tui::canvas::{Canvas, StreamBuffer};
use enki_tui::input::{InputAction, InputLine};
use enki_tui::style::{Line, Span, Style};
use enki_tui::{poll_event, Color, KeyCode, TermEvent};

use coordinator::{FromCoordinator, ToCoordinator, WorkerActivity};

const PROMPT: &str = "› ";

// ─── Worker status tracking ──────────────────────────────────

struct WorkerState {
    task_id: String,
    title: String,
    current_tool: Option<String>,
    started_at: Instant,
}

struct WorkerTracker {
    workers: Vec<WorkerState>,
    expanded: bool,
}

impl WorkerTracker {
    fn new() -> Self {
        Self { workers: Vec::new(), expanded: false }
    }

    fn add(&mut self, task_id: String, title: String) {
        self.workers.push(WorkerState {
            task_id,
            title,
            current_tool: None,
            started_at: Instant::now(),
        });
    }

    fn remove(&mut self, task_id: &str) {
        self.workers.retain(|w| w.task_id != task_id);
    }

    fn update(&mut self, task_id: &str, activity: WorkerActivity) {
        let Some(worker) = self.workers.iter_mut().find(|w| w.task_id == task_id) else {
            return;
        };
        match activity {
            WorkerActivity::ToolStarted(title) => worker.current_tool = Some(title),
            WorkerActivity::ToolDone => worker.current_tool = None,
            WorkerActivity::Thinking => worker.current_tool = None,
        }
    }

    fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
    }

    fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Render status bar lines based on current state.
    fn render(&self, width: u16) -> Vec<Line> {
        if self.workers.is_empty() {
            return Vec::new();
        }

        let mut lines = Vec::new();
        lines.push(self.render_summary(width));

        if self.expanded {
            for worker in &self.workers {
                lines.push(self.render_worker_line(worker, width));
            }
        }

        lines
    }

    fn render_summary(&self, _width: u16) -> Line {
        let count = self.workers.len();
        let with_tool: usize = self.workers.iter().filter(|w| w.current_tool.is_some()).count();
        let thinking = count - with_tool;

        let mut spans = vec![
            Span::styled(
                format!(" ⚙ {count} worker{}", if count == 1 { "" } else { "s" }),
                Style::new().fg(Color::DarkCyan).bold(),
            ),
        ];

        if with_tool > 0 {
            spans.push(Span::styled(
                format!("  │  {with_tool} running"),
                Style::new().fg(Color::DarkGrey),
            ));
        }
        if thinking > 0 {
            spans.push(Span::styled(
                format!("  {thinking} thinking"),
                Style::new().fg(Color::DarkGrey),
            ));
        }

        if self.expanded {
            spans.push(Span::styled(
                "  [Tab to collapse]",
                Style::new().fg(Color::DarkGrey),
            ));
        } else {
            spans.push(Span::styled(
                "  [Tab to expand]",
                Style::new().fg(Color::DarkGrey),
            ));
        }

        Line::new(spans)
    }

    fn render_worker_line(&self, worker: &WorkerState, width: u16) -> Line {
        let elapsed = worker.started_at.elapsed();
        let mins = elapsed.as_secs() / 60;
        let secs = elapsed.as_secs() % 60;
        let time_str = format!("{mins}:{secs:02}");

        // Truncate title to fit
        let max_title = (width as usize).saturating_sub(30).min(40);
        let title: String = if worker.title.len() > max_title {
            format!("{}…", &worker.title[..max_title - 1])
        } else {
            worker.title.clone()
        };

        let activity = match &worker.current_tool {
            Some(tool) => format!("⚙ {tool}"),
            None => "● thinking".to_string(),
        };

        Line::new(vec![
            Span::styled(
                format!("   ▶ {title}"),
                Style::new().fg(Color::White),
            ),
            Span::styled(
                format!("  {activity}"),
                Style::new().fg(Color::DarkYellow),
            ),
            Span::styled(
                format!("  {time_str}"),
                Style::new().fg(Color::DarkGrey),
            ),
        ])
    }
}

/// Run the chat interface. This takes over terminal input (raw mode)
/// with a pinned input bubble at the bottom.
pub async fn run(db: enki_core::db::Db, db_path: String) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Resolve project CWD before entering raw mode so we can print/prompt normally.
    let project_cwd = select_project_cwd(&db, &cwd);

    // Pre-raw-mode banner (clears the screen, erasing any selection output above)
    print_banner();

    // Spawn coordinator
    let mut coord_handle = coordinator::spawn(project_cwd.clone(), db_path);

    // Enter raw mode (Canvas::drop restores it)
    let mut canvas = Canvas::enter(PROMPT)?;
    let mut input = InputLine::new();
    input.set_autocomplete_trigger(Some('@'));
    let mut stream = StreamBuffer::new();
    let mut tracker = WorkerTracker::new();
    let mut running = true;
    let mut streaming = false;

    canvas.update_bubble(&input);

    while running {
        // Poll coordinator messages (non-blocking)
        while let Ok(msg) = coord_handle.rx.try_recv() {
            match msg {
                FromCoordinator::Connected => {
                    canvas.print_line(&system_line("Coordinator connected. Initializing..."));
                }
                FromCoordinator::Ready => {
                    canvas.print_line(&system_line("Coordinator ready."));
                }
                FromCoordinator::Text(text) => {
                    if !streaming {
                        canvas.begin_streaming();
                        streaming = true;
                    }
                    stream.push(&text);
                    stream.flush(&mut canvas);
                }
                FromCoordinator::ToolCall(_) => {
                    stream.finish(&mut canvas);
                    streaming = false;
                }
                FromCoordinator::ToolCallDone(_) => {}
                FromCoordinator::Done(_reason) => {
                    stream.finish_markdown(&mut canvas);
                    streaming = false;
                }
                FromCoordinator::WorkerSpawned { task_id, title } => {
                    stream.finish(&mut canvas);
                    canvas.print_line(&Line::new(vec![Span::styled(
                        format!("  ▶ Worker spawned: {title} ({task_id})"),
                        Style::new().fg(Color::DarkCyan),
                    )]));
                    tracker.add(task_id, title);
                    canvas.set_status_bar(&tracker.render(canvas.content_width()));
                }
                FromCoordinator::WorkerCompleted { task_id, title } => {
                    stream.finish(&mut canvas);
                    canvas.print_line(&Line::new(vec![Span::styled(
                        format!("  ✓ Worker completed: {title} ({task_id})"),
                        Style::new().fg(Color::Green),
                    )]));
                    tracker.remove(&task_id);
                    canvas.set_status_bar(&tracker.render(canvas.content_width()));
                }
                FromCoordinator::WorkerFailed {
                    task_id,
                    title,
                    error,
                } => {
                    stream.finish(&mut canvas);
                    canvas.print_line(&Line::new(vec![Span::styled(
                        format!("  ✗ Worker failed: {title} ({task_id}): {error}"),
                        Style::new().fg(Color::Red),
                    )]));
                    tracker.remove(&task_id);
                    canvas.set_status_bar(&tracker.render(canvas.content_width()));
                }
                FromCoordinator::WorkerConflicted {
                    task_id,
                    title,
                    worktree,
                    branch: _,
                } => {
                    stream.finish(&mut canvas);
                    canvas.print_line(&Line::new(vec![Span::styled(
                        format!("  ⚠ Merge conflict: {title} ({task_id})"),
                        Style::new().fg(Color::Yellow).bold(),
                    )]));
                    canvas.print_line(&Line::new(vec![Span::styled(
                        format!("    Worktree preserved at: {worktree}"),
                        Style::new().fg(Color::Yellow),
                    )]));
                    canvas.print_line(&Line::new(vec![Span::styled(
                        format!("    Run: enki task retry {task_id}"),
                        Style::new().fg(Color::Yellow),
                    )]));
                    tracker.remove(&task_id);
                    canvas.set_status_bar(&tracker.render(canvas.content_width()));
                }
                FromCoordinator::WorkerUpdate { task_id, activity } => {
                    tracker.update(&task_id, activity);
                    canvas.set_status_bar(&tracker.render(canvas.content_width()));
                }
                FromCoordinator::Error(e) => {
                    stream.finish(&mut canvas);
                    streaming = false;
                    canvas.print_line(&Line::new(vec![Span::styled(
                        format!("error: {e}"),
                        Style::new().fg(Color::Red).bold(),
                    )]));
                }
            }
        }

        // Poll events
        if let Some(event) = poll_event(Duration::from_millis(30))? {
            match event {
                TermEvent::Resize(w, h) => {
                    canvas.handle_resize(w, h, &input);
                }
                TermEvent::ScrollUp(n) => {
                    canvas.scroll_up(n);
                }
                TermEvent::ScrollDown(n) => {
                    canvas.scroll_down(n);
                }
                TermEvent::Key(key) => {
                    // Handle scroll keys and status bar toggle before input
                    match key.code {
                        KeyCode::PageUp => {
                            canvas.scroll_up(canvas.viewport_height());
                            continue;
                        }
                        KeyCode::PageDown => {
                            canvas.scroll_down(canvas.viewport_height());
                            continue;
                        }
                        KeyCode::Tab if !tracker.is_empty() => {
                            tracker.toggle_expanded();
                            canvas.set_status_bar(&tracker.render(canvas.content_width()));
                            continue;
                        }
                        _ => {}
                    }

                    let old_ac_count = input
                        .autocomplete
                        .as_ref()
                        .map(|ac| ac.matches.len())
                        .unwrap_or(0);

                    let cwd_ref = &project_cwd;
                    let action = input.handle_key(
                        key.code,
                        key.modifiers,
                        Some(&|query| complete_files(cwd_ref, query)),
                    );

                    match action {
                        InputAction::None => {}
                        InputAction::Quit => {
                            let _ = coord_handle.tx.send(ToCoordinator::Shutdown);
                            running = false;
                        }
                        InputAction::ConfirmExit => {
                            canvas.set_hint(Some("Press Ctrl+C again to exit.".into()));
                            canvas.update_bubble(&input);
                        }
                        InputAction::Submit(text) => {
                            canvas.set_hint(None);
                            if old_ac_count > 0 {
                                canvas.clear_autocomplete(old_ac_count);
                            }
                            canvas.scroll_to_bottom();
                            stream.finish(&mut canvas);
                            canvas.print_line(&Line::new(vec![
                                Span::styled("> ", Style::new().fg(Color::Cyan).bold()),
                                Span::styled(&text, Style::new().fg(Color::Cyan)),
                            ]));
                            canvas.update_bubble(&input);
                            let _ = coord_handle.tx.send(ToCoordinator::Prompt(text));
                        }
                        InputAction::Changed => {
                            canvas.set_hint(None);
                            if old_ac_count > 0 {
                                canvas.clear_autocomplete(old_ac_count);
                            }
                            canvas.update_bubble(&input);
                            if let Some(ac) = &input.autocomplete
                                && !ac.matches.is_empty()
                            {
                                canvas.draw_autocomplete(&ac.matches, ac.selected);
                            }
                        }
                    }
                }
            }
        }
    }

    // Canvas dropped here -> raw mode restored
    Ok(())
}

/// Determine the working directory to use for the coordinator.
///
/// Always uses CWD. If CWD is inside a registered project, uses that project's
/// root. Otherwise uses CWD directly and suggests registering it.
fn select_project_cwd(db: &enki_core::db::Db, cwd: &Path) -> PathBuf {
    if let Ok(projects) = db.list_projects() {
        // If CWD is inside a registered project, use that project's root.
        if let Some(project) = projects
            .iter()
            .find(|p| cwd.starts_with(&p.local_path) || Path::new(&p.local_path) == cwd)
        {
            return PathBuf::from(&project.local_path);
        }
    }

    // CWD is not a registered project — use it directly.
    if cwd.join(".git").exists() {
        println!("Tip: Run `enki project add {}` to register this repo.", cwd.display());
    }
    cwd.to_path_buf()
}

/// Print the welcome banner. Called BEFORE entering raw mode.
fn print_banner() {
    print!("\x1b[2J\x1b[H"); // clear screen
    println!("\x1b[1;4menki — multi-agent orchestrator\x1b[0m");
    println!("\x1b[90mType a message to chat with the coordinator. Ctrl+C to quit.\x1b[0m");
    println!();
}

/// System notification line (grey).
fn system_line(text: &str) -> Line {
    Line::new(vec![Span::styled(text, Style::new().fg(Color::DarkGrey))])
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
    let pattern = Pattern::new(query, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy);

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
