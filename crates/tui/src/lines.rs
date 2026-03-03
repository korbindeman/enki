//! Reusable line builders for chat-style UIs.
//!
//! Each function returns a [`Line`] (or `Vec<Line>`) ready to pass to
//! [`Canvas::print_line`] / [`Canvas::print_lines`].

use crossterm::style::Color;

use crate::style::{Line, Span, Style};

/// User message with a prefix on the first line and indentation on continuations.
///
/// ```text
/// > hello world
///   this is a second line
/// ```
pub fn user_message(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let first_prefix = Span::styled("> ", Style::new().fg(Color::Cyan).bold());
    let cont_prefix = Span::styled("  ", Style::new());

    for (i, line) in text.lines().enumerate() {
        let prefix = if i == 0 {
            first_prefix.clone()
        } else {
            cont_prefix.clone()
        };
        lines.push(Line::new(vec![
            prefix,
            Span::styled(line, Style::new().fg(Color::Cyan)),
        ]));
    }
    lines
}

/// Tool call started.
///
/// ```text
///   ⏵ Read src/main.rs
/// ```
pub fn tool_call(name: &str) -> Line {
    Line::new(vec![
        Span::styled("  ⏵ ", Style::new().fg(Color::DarkYellow)),
        Span::styled(name, Style::new().fg(Color::DarkYellow).dim()),
    ])
}

/// Tool call completed.
///
/// ```text
///   ✓ Read src/main.rs
/// ```
pub fn tool_done(name: &str) -> Line {
    Line::new(vec![
        Span::styled("  ✓ ", Style::new().fg(Color::DarkGreen)),
        Span::styled(name, Style::new().fg(Color::DarkGrey).dim()),
    ])
}

/// Horizontal separator rule.
///
/// ```text
/// ────────────────────────
/// ```
pub fn separator(width: u16) -> Line {
    let rule = "─".repeat(width as usize);
    Line::new(vec![Span::styled(rule, Style::new().fg(Color::DarkGrey))])
}

/// System notification (grey text, no icon).
///
/// ```text
/// Coordinator ready.
/// ```
pub fn system(text: &str) -> Line {
    Line::new(vec![Span::styled(
        text,
        Style::new().fg(Color::DarkGrey),
    )])
}

/// Error message (red, bold).
///
/// ```text
/// error: connection refused
/// ```
pub fn error(text: &str) -> Line {
    Line::new(vec![Span::styled(
        text,
        Style::new().fg(Color::Red).bold(),
    )])
}

/// Event with an icon prefix — for worker lifecycle, custom events, etc.
///
/// ```text
///   ▶ Worker spawned: implement auth (task-abc)
///   ✓ Worker completed: implement auth (task-abc)
///   ✗ Worker failed: implement auth (task-abc): timeout
/// ```
pub fn event(icon: &str, text: &str, color: Color) -> Line {
    Line::new(vec![Span::styled(
        format!("  {icon} {text}"),
        Style::new().fg(color),
    )])
}

/// Event with bold styling (for warnings/conflicts).
pub fn event_bold(icon: &str, text: &str, color: Color) -> Line {
    Line::new(vec![Span::styled(
        format!("  {icon} {text}"),
        Style::new().fg(color).bold(),
    )])
}

/// Indented detail line (for follow-up info under an event).
///
/// ```text
///     Worktree preserved at: /path/to/worktree
/// ```
pub fn detail(text: &str, color: Color) -> Line {
    Line::new(vec![Span::styled(
        format!("    {text}"),
        Style::new().fg(color),
    )])
}
