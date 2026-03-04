//! Worker activity panel.
//!
//! A togglable banner panel showing per-worker activity: tier badge,
//! task title, elapsed time, and current activity.
//!
//! Follows the same pattern as [`Indicator`](crate::indicator::Indicator):
//! owned mutable state with mutation methods and `render(width) -> Vec<Line>`.
//!
//! # Usage
//!
//! ```ignore
//! let mut panel = WorkerPanel::new();
//!
//! panel.add("task-01", "Implement auth", "standard");
//! panel.set_activity("task-01", "Reading src/auth.rs");
//!
//! if panel.is_visible() {
//!     canvas.set_banner(&panel.render(80));
//! }
//!
//! panel.toggle();
//! panel.remove("task-01");
//! ```

use std::collections::HashMap;
use std::time::Instant;

use crossterm::style::Color;

use crate::style::{Line, Span, Style, truncate_str};

/// Per-worker state tracked by the panel.
struct WorkerEntry {
    title: String,
    tier: String,
    spawned_at: Instant,
    activity: String,
}

/// A togglable panel showing per-worker activity.
///
/// Each worker row shows: tier badge, task title, elapsed time, current activity.
/// Elapsed time is color-coded: green (<2m), yellow (2-5m), red (>5m).
///
/// ```text
/// ─── Workers (Ctrl+W to close) ─────────────────
///  ▶ [standard] Implement auth module      2m 14s  Reading src/auth.rs
///  ▶ [light]    Write unit tests           0m 45s  Thinking
///  ▶ [heavy]    Design API schema          5m 02s  analyzing codebase
/// ────────────────────────────────────────────────
/// ```
pub struct WorkerPanel {
    workers: HashMap<String, WorkerEntry>,
    /// Insertion order for stable rendering.
    order: Vec<String>,
    visible: bool,
}

impl WorkerPanel {
    pub fn new() -> Self {
        Self {
            workers: HashMap::new(),
            order: Vec::new(),
            visible: false,
        }
    }

    /// Add a worker to the panel.
    pub fn add(&mut self, task_id: &str, title: &str, tier: &str) {
        let id = task_id.to_string();
        self.workers.insert(
            id.clone(),
            WorkerEntry {
                title: title.to_string(),
                tier: tier.to_string(),
                spawned_at: Instant::now(),
                activity: "Starting…".to_string(),
            },
        );
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    /// Remove a worker from the panel (completed or failed).
    pub fn remove(&mut self, task_id: &str) {
        self.workers.remove(task_id);
        self.order.retain(|id| id != task_id);
    }

    /// Update a worker's current activity text.
    pub fn set_activity(&mut self, task_id: &str, activity: &str) {
        if let Some(entry) = self.workers.get_mut(task_id) {
            entry.activity = activity.to_string();
        }
    }

    /// Toggle panel visibility. Returns new visibility state.
    pub fn toggle(&mut self) -> bool {
        self.visible = !self.visible;
        self.visible
    }

    /// Whether the panel should be rendered.
    pub fn is_visible(&self) -> bool {
        self.visible && !self.workers.is_empty()
    }

    /// Number of tracked workers.
    pub fn count(&self) -> usize {
        self.workers.len()
    }

    /// Render the panel as banner lines.
    ///
    /// Returns empty `Vec` when not visible or no workers — which clears the
    /// panel portion of the banner.
    pub fn render(&self, width: u16) -> Vec<Line> {
        if !self.is_visible() {
            return Vec::new();
        }

        let w = width as usize;
        let mut lines = Vec::with_capacity(self.workers.len() + 2);

        // Header
        let header_text = " Workers (Ctrl+W to close) ";
        let rule_len = w.saturating_sub(header_text.len() + 3);
        let header = format!("─── {header_text}{}", "─".repeat(rule_len));
        lines.push(Line::new(vec![Span::styled(
            header,
            Style::new().fg(Color::DarkGrey),
        )]));

        // Worker rows in insertion order
        for task_id in &self.order {
            let Some(entry) = self.workers.get(task_id) else {
                continue;
            };

            let elapsed = entry.spawned_at.elapsed().as_secs();
            let mins = elapsed / 60;
            let secs = elapsed % 60;
            let time_str = format!("{mins}m {secs:02}s");

            let tier_color = match entry.tier.as_str() {
                "light" => Color::DarkCyan,
                "heavy" => Color::DarkMagenta,
                _ => Color::DarkYellow, // standard
            };

            let elapsed_color = if elapsed < 120 {
                Color::Green
            } else if elapsed < 300 {
                Color::Yellow
            } else {
                Color::Red
            };

            // Pad tier to 8 chars for alignment
            let tier_padded = format!("{:<8}", entry.tier);

            // Truncate title so the row fits
            let fixed_overhead = 4 + 10 + 2 + 8 + 2; // " ▶ " + "[tier    ]" + "  " + "Xm XXs" + "  "
            let max_title = (w / 2).saturating_sub(fixed_overhead);
            let title = truncate_str(&entry.title, max_title);
            let title_padded = format!("{title:<width$}", width = max_title);

            let max_activity = w.saturating_sub(fixed_overhead + max_title + 2);
            let activity = truncate_str(&entry.activity, max_activity);

            lines.push(Line::new(vec![
                Span::styled(" ▶ ", Style::new().fg(tier_color)),
                Span::styled(format!("[{tier_padded}]"), Style::new().fg(tier_color)),
                Span::styled(format!(" {title_padded}"), Style::new().fg(Color::White)),
                Span::styled(format!("  {time_str}"), Style::new().fg(elapsed_color)),
                Span::styled(
                    format!("  {activity}"),
                    Style::new().fg(Color::DarkGrey).italic(),
                ),
            ]));
        }

        // Footer
        lines.push(Line::new(vec![Span::styled(
            "─".repeat(w),
            Style::new().fg(Color::DarkGrey),
        )]));

        lines
    }
}

