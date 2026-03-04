use std::time::Instant;

use crate::style::{Line, Span, Style, truncate_str};
use crossterm::style::Color;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// What the primary agent is doing right now.
pub enum Activity {
    /// Waiting for a response / producing text.
    Thinking,
    /// Running a named tool (e.g. "Read src/main.rs").
    Tool(String),
}

/// A single-line activity indicator with a braille spinner.
///
/// Renders as a pinned status-bar line showing what the agent is doing:
///
/// ```text
///  ⠋ Thinking… 3s
///  ⠋ Read src/main.rs 1s  │  2 workers
/// ```
///
/// # Usage
///
/// ```ignore
/// let mut indicator = Indicator::new();
///
/// // User submitted a prompt
/// indicator.set_activity(Activity::Thinking);
/// canvas.set_status_bar(&indicator.render(80));
///
/// // Tool call started
/// indicator.set_activity(Activity::Tool("Read src/main.rs".into()));
///
/// // Response streaming — clear the indicator
/// indicator.clear_activity();
/// canvas.clear_status_bar();
///
/// // In the event loop, tick the spinner (~12fps)
/// if indicator.is_active() {
///     indicator.tick();
///     canvas.set_status_bar(&indicator.render(80));
/// }
/// ```
pub struct Indicator {
    activity: Option<Activity>,
    worker_count: usize,
    started_at: Instant,
    frame: usize,
}

impl Default for Indicator {
    fn default() -> Self {
        Self::new()
    }
}

impl Indicator {
    pub fn new() -> Self {
        Self {
            activity: None,
            worker_count: 0,
            started_at: Instant::now(),
            frame: 0,
        }
    }

    /// Set the primary activity (thinking or running a tool).
    /// Resets the elapsed timer.
    pub fn set_activity(&mut self, activity: Activity) {
        self.activity = Some(activity);
        self.started_at = Instant::now();
    }

    /// Clear the primary activity (agent is idle).
    pub fn clear_activity(&mut self) {
        self.activity = None;
    }

    /// Increment the background worker count.
    pub fn add_worker(&mut self) {
        self.worker_count += 1;
    }

    /// Decrement the background worker count.
    pub fn remove_worker(&mut self) {
        self.worker_count = self.worker_count.saturating_sub(1);
    }

    /// Set the worker count to an exact value (for polling-based sync).
    pub fn set_worker_count(&mut self, count: usize) {
        self.worker_count = count;
    }

    /// Reset the worker count to zero.
    pub fn reset_workers(&mut self) {
        self.worker_count = 0;
    }

    /// Advance the spinner one frame. Call at ~80ms intervals.
    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % SPINNER.len();
    }

    /// Whether anything is worth displaying (activity or workers).
    pub fn is_active(&self) -> bool {
        self.activity.is_some() || self.worker_count > 0
    }

    /// Render the indicator as status-bar lines.
    ///
    /// `width` is the available terminal width in columns. The tool name is
    /// truncated so the line never exceeds this width.
    ///
    /// Returns an empty `Vec` when idle (which clears the status bar).
    pub fn render(&self, width: u16) -> Vec<Line> {
        let spinner = SPINNER[self.frame];

        if let Some(activity) = &self.activity {
            let elapsed = self.started_at.elapsed().as_secs();
            let time = if elapsed > 0 {
                format!(" {elapsed}s")
            } else {
                String::new()
            };

            let mut spans = match activity {
                Activity::Thinking => vec![
                    Span::styled(format!(" {spinner} "), Style::new().fg(Color::DarkMagenta)),
                    Span::styled("Thinking…", Style::new().fg(Color::DarkGrey).italic()),
                    Span::styled(time, Style::new().fg(Color::DarkGrey)),
                ],
                Activity::Tool(name) => {
                    // " ⠋ " = 3 cols, time suffix, possible worker suffix.
                    // Truncate the tool name so the line fits in `width`.
                    let prefix_len = 3; // " ⠋ "
                    let time_len = time.len();
                    let worker_suffix_len = if self.worker_count > 0 { 20 } else { 0 };
                    let overhead = prefix_len + time_len + worker_suffix_len;
                    let max_name = (width as usize).saturating_sub(overhead);
                    let truncated = truncate_str(name, max_name);
                    vec![
                        Span::styled(format!(" {spinner} "), Style::new().fg(Color::DarkYellow)),
                        Span::styled(truncated, Style::new().fg(Color::DarkYellow)),
                        Span::styled(time, Style::new().fg(Color::DarkGrey)),
                    ]
                }
            };

            if self.worker_count > 0 {
                let n = self.worker_count;
                spans.push(Span::styled(
                    format!("  │  {n} worker{}", if n == 1 { "" } else { "s" }),
                    Style::new().fg(Color::DarkGrey),
                ));
            }

            return vec![Line::new(spans)];
        }

        if self.worker_count > 0 {
            let n = self.worker_count;
            return vec![Line::new(vec![
                Span::styled(format!(" {spinner} "), Style::new().fg(Color::DarkCyan)),
                Span::styled(
                    format!("{n} worker{} running", if n == 1 { "" } else { "s" }),
                    Style::new().fg(Color::DarkGrey).italic(),
                ),
            ])];
        }

        Vec::new()
    }
}
