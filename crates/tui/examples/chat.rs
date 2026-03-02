use std::time::Duration;

use enki_tui::canvas::{Canvas, StreamBuffer};
use enki_tui::input::{InputAction, InputLine};
use enki_tui::style::{Line, Span, Style};
use enki_tui::{poll_event, Color, KeyCode, TermEvent};
use tokio::sync::mpsc;

// ─── Backend simulation ──────────────────────────────────────

enum BackendMsg {
    Chunk(String),
    ToolCall(String),
    ToolDone(String),
    Thinking(String),
    Done,
}

/// Canned responses that cycle through different capabilities.
const RESPONSES: &[&str] = &[
    // 0: Simple markdown
    "Sure, I can help with that.\n\n\
Here's a quick overview:\n\n\
- **First**, we'll read the relevant files\n\
- **Then**, make the necessary changes\n\
- **Finally**, verify everything compiles\n\n\
Let me take a look at your codebase.",

    // 1: Code block
    "Here's the implementation:\n\n\
```rust\n\
pub struct Config {\n\
    pub name: String,\n\
    pub verbose: bool,\n\
}\n\
\n\
impl Config {\n\
    pub fn load() -> Result<Self> {\n\
        let raw = std::fs::read_to_string(\"config.toml\")?;\n\
        toml::from_str(&raw).map_err(Into::into)\n\
    }\n\
}\n\
```\n\n\
This reads the config file and deserializes it. The `Result` propagates any IO or parse errors.",

    // 2: Multi-tool workflow
    "I found the issue. The `process` function isn't handling the edge case \
where the input is empty. Let me fix that.\n\n\
The fix is straightforward — add an early return at the top of the function. \
I've also added a test to cover this case.",

    // 3: Short answer
    "Done. The changes compile and all tests pass.",
];

/// Tools to simulate for each response index.
const TOOL_SEQUENCES: &[&[(&str, u64)]] = &[
    // 0: read files
    &[("Read src/main.rs", 400), ("Read src/lib.rs", 300)],
    // 1: read + write
    &[("Read src/config.rs", 350), ("Write src/config.rs", 200)],
    // 2: multi-step
    &[
        ("Read src/process.rs", 300),
        ("Write src/process.rs", 200),
        ("Read tests/process_test.rs", 250),
        ("Write tests/process_test.rs", 200),
        ("Run cargo test", 800),
    ],
    // 3: quick
    &[("Run cargo check", 500)],
];

async fn fake_respond(tx: mpsc::UnboundedSender<BackendMsg>, index: usize) {
    let resp = RESPONSES[index % RESPONSES.len()];
    let tools = TOOL_SEQUENCES[index % TOOL_SEQUENCES.len()];

    // Thinking
    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(BackendMsg::Thinking("Analyzing request...".into()))
        .ok();
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Tool calls
    for (name, duration_ms) in tools {
        tx.send(BackendMsg::ToolCall(name.to_string())).ok();
        tokio::time::sleep(Duration::from_millis(*duration_ms)).await;
        tx.send(BackendMsg::ToolDone(name.to_string())).ok();
    }

    // Stream the response in small chunks
    let bytes = resp.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let chunk_size = (7 + pos % 5).min(bytes.len() - pos);
        let end = (pos + chunk_size).min(bytes.len());
        let end = if end < bytes.len() {
            let mut e = end;
            while e > pos && !resp.is_char_boundary(e) {
                e -= 1;
            }
            if e == pos { end } else { e }
        } else {
            end
        };
        let chunk = &resp[pos..end];
        tx.send(BackendMsg::Chunk(chunk.to_string())).ok();
        pos = end;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tx.send(BackendMsg::Done).ok();
}

// ─── UI helpers ──────────────────────────────────────────────

fn user_msg_lines(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let first_prefix = Span::styled("❯ ", Style::new().fg(Color::Cyan).bold());
    let cont_prefix = Span::styled("  ", Style::new());

    for (i, line) in text.lines().enumerate() {
        let prefix = if i == 0 {
            first_prefix.clone()
        } else {
            cont_prefix.clone()
        };
        lines.push(Line::new(vec![
            prefix,
            Span::styled(line, Style::new().fg(Color::White)),
        ]));
    }
    lines
}

fn thinking_line(text: &str) -> Line {
    Line::new(vec![
        Span::styled("  ◐ ", Style::new().fg(Color::Magenta).dim()),
        Span::styled(text, Style::new().fg(Color::DarkGrey).italic()),
    ])
}

fn tool_call_line(name: &str) -> Line {
    Line::new(vec![
        Span::styled("  ⏵ ", Style::new().fg(Color::DarkYellow)),
        Span::styled(name, Style::new().fg(Color::DarkYellow).dim()),
    ])
}

fn tool_done_line(name: &str) -> Line {
    Line::new(vec![
        Span::styled("  ✓ ", Style::new().fg(Color::DarkGreen)),
        Span::styled(name, Style::new().fg(Color::DarkGrey).dim()),
    ])
}

fn separator(width: u16) -> Line {
    let rule = "─".repeat(width as usize);
    Line::new(vec![Span::styled(rule, Style::new().fg(Color::DarkGrey))])
}

// ─── Main ────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Pre-raw-mode: clear screen and print header
    print!("\x1b[2J\x1b[H");
    let pre_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let rule = "─".repeat(pre_width as usize);
    println!(
        "  \x1b[1;35menki\x1b[0m \x1b[2;37mchat\x1b[0m\n\x1b[90m{rule}\x1b[0m"
    );

    let mut canvas = Canvas::enter("❯ ")?;
    let mut input = InputLine::new();
    let mut stream = StreamBuffer::new();
    let mut running = true;
    let mut msg_count: usize = 0;
    let mut streaming_active = false;

    let (tx, mut rx) = mpsc::unbounded_channel::<BackendMsg>();

    canvas.update_bubble(&input);

    while running {
        // 1. Drain backend messages
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BackendMsg::Thinking(text) => {
                    stream.finish(&mut canvas);
                    canvas.print_line(&thinking_line(&text));
                }
                BackendMsg::ToolCall(name) => {
                    stream.finish(&mut canvas);
                    canvas.print_line(&tool_call_line(&name));
                }
                BackendMsg::ToolDone(name) => {
                    canvas.print_line(&tool_done_line(&name));
                }
                BackendMsg::Chunk(text) => {
                    if !streaming_active {
                        stream.finish(&mut canvas);
                        canvas.blank_line();
                        canvas.begin_streaming();
                        streaming_active = true;
                    }
                    stream.push(&text);
                    stream.flush(&mut canvas);
                }
                BackendMsg::Done => {
                    stream.finish_markdown(&mut canvas);
                    streaming_active = false;

                    let w = canvas.content_width();
                    canvas.blank_line();
                    canvas.print_line(&separator(w));
                }
            }
        }

        // 2. Poll events
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
                    // Handle scroll keys before input
                    match key.code {
                        KeyCode::PageUp => {
                            canvas.scroll_up(canvas.viewport_height());
                            continue;
                        }
                        KeyCode::PageDown => {
                            canvas.scroll_down(canvas.viewport_height());
                            continue;
                        }
                        _ => {}
                    }

                    let old_ac_count = input
                        .autocomplete
                        .as_ref()
                        .map(|ac| ac.matches.len())
                        .unwrap_or(0);

                    let action = input.handle_key(key.code, key.modifiers, None);

                    match action {
                        InputAction::Quit => running = false,
                        InputAction::ConfirmExit => {
                            canvas.scroll_to_bottom();
                            canvas.print_line(&Line::new(vec![Span::styled(
                                "  Press Ctrl+C again to exit.",
                                Style::new().fg(Color::DarkGrey).italic(),
                            )]));
                            canvas.update_bubble(&input);
                        }
                        InputAction::Submit(text) => {
                            if old_ac_count > 0 {
                                canvas.clear_autocomplete(old_ac_count);
                            }

                            canvas.scroll_to_bottom();

                            if streaming_active {
                                stream.finish(&mut canvas);
                                streaming_active = false;
                                let w = canvas.content_width();
                                canvas.print_line(&separator(w));
                            }

                            canvas.print_lines(&user_msg_lines(&text));
                            canvas.update_bubble(&input);

                            let tx = tx.clone();
                            let idx = msg_count;
                            msg_count += 1;
                            tokio::spawn(async move {
                                fake_respond(tx, idx).await;
                            });
                        }
                        InputAction::Changed => {
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
                        InputAction::None => {}
                    }
                }
            }
        }
    }

    Ok(())
}
