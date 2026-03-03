//! High-level chat UI framework.
//!
//! Provides a trait-based API that eliminates the boilerplate event loop,
//! spinner ticking, streaming state management, and canvas synchronization
//! that every chat-style consumer must otherwise implement manually.
//!
//! # Example
//!
//! ```ignore
//! use enki_tui::chat::{Chat, ChatContext, Handler};
//! use enki_tui::lines;
//!
//! struct MyApp { tx: mpsc::UnboundedSender<String> }
//!
//! impl Handler<BackendMsg> for MyApp {
//!     fn on_message(&mut self, msg: BackendMsg, cx: &mut ChatContext) {
//!         match msg {
//!             BackendMsg::Chunk(t) => cx.stream(&t),
//!             BackendMsg::Done => cx.finish_markdown(),
//!             BackendMsg::Thinking => cx.think(),
//!         }
//!     }
//!
//!     fn on_submit(&mut self, text: String, cx: &mut ChatContext) {
//!         self.tx.send(text).ok();
//!     }
//! }
//!
//! Chat::new("❯ ").run(MyApp { tx }, || rx.try_recv().ok())?;
//! ```

use std::io;
use std::time::{Duration, Instant};

use crossterm::style::Color;

use crate::canvas::{Canvas, StreamBuffer};
use crate::indicator::{Activity, Indicator};
use crate::input::{InputAction, InputLine};
use crate::lines;
use crate::notify;
use crate::style::{Line, Span, Style};
use crate::{poll_event, KeyCode, KeyModifiers, TermEvent};

/// Trait implemented by chat consumers to handle messages and user input.
///
/// The framework handles all event-loop boilerplate (resize, scroll, input
/// editing, autocomplete, spinner ticking). Implementors only define what
/// happens when a backend message arrives or the user submits text.
pub trait Handler<M> {
    /// Called for each message received from the backend channel.
    fn on_message(&mut self, msg: M, cx: &mut ChatContext);

    /// Called when the user submits text (presses Enter).
    ///
    /// The user message is already printed and the indicator is set to
    /// "Thinking…" before this is called. Use `cx` to send text to your
    /// backend.
    fn on_submit(&mut self, text: String, cx: &mut ChatContext);

    /// Called when the user requests an interrupt (Escape or new message
    /// while one is in-flight).
    ///
    /// Use this to cancel any active backend operation. Safe to call when
    /// nothing is running — implementors should no-op in that case.
    fn on_interrupt(&mut self) {}

    /// Called when the user confirms quit (double Ctrl+C).
    ///
    /// Use this to send shutdown signals to your backend. The event loop
    /// exits after this returns.
    fn on_quit(&mut self) {}

    /// Provide autocomplete matches for a query string.
    ///
    /// Called when the user types the autocomplete trigger character (if set).
    /// Return an empty vec for no matches. Default: no autocomplete.
    fn autocomplete(&self, _query: &str) -> Vec<String> {
        Vec::new()
    }
}

/// High-level context passed to [`Handler`] callbacks.
///
/// Wraps Canvas, StreamBuffer, and Indicator behind intent-driven methods
/// so consumers don't need to manage streaming state, indicator syncing,
/// or canvas drawing manually.
pub struct ChatContext {
    canvas: Canvas,
    stream: StreamBuffer,
    indicator: Indicator,
    streaming: bool,
}

impl ChatContext {
    fn new(canvas: Canvas) -> Self {
        Self {
            canvas,
            stream: StreamBuffer::new(),
            indicator: Indicator::new(),
            streaming: false,
        }
    }


    // ─── Activity ────────────────────────────────────────────

    /// Set the indicator to "Thinking…" and update the status bar.
    pub fn think(&mut self) {
        self.finish_stream_if_active();
        self.indicator.set_activity(Activity::Thinking);
        self.sync_status_bar();
    }

    /// Set the indicator to a named tool and update the status bar.
    pub fn tool(&mut self, name: String) {
        self.finish_stream_if_active();
        self.indicator.set_activity(Activity::Tool(name));
        self.sync_status_bar();
    }

    /// Clear the activity indicator.
    pub fn clear_activity(&mut self) {
        self.indicator.clear_activity();
        self.sync_status_bar();
    }

    // ─── Streaming ───────────────────────────────────────────

    /// Push streaming text. Automatically begins a stream block if needed.
    pub fn stream(&mut self, text: &str) {
        if !self.streaming {
            self.indicator.clear_activity();
            self.sync_status_bar();
            self.canvas.begin_streaming();
            self.streaming = true;
        }
        self.stream.push(text);
        self.stream.flush(&mut self.canvas);
    }

    /// Finish the current stream block (plain text).
    pub fn finish(&mut self) {
        self.stream.finish(&mut self.canvas);
        self.streaming = false;
    }

    /// Finish the current stream block with markdown rendering.
    #[cfg(feature = "markdown")]
    pub fn finish_markdown(&mut self) {
        self.stream.finish_markdown(&mut self.canvas);
        self.streaming = false;
    }

    /// Finish any in-progress stream block (plain). No-op if not streaming.
    fn finish_stream_if_active(&mut self) {
        if self.streaming {
            self.stream.finish(&mut self.canvas);
            self.streaming = false;
        }
    }

    // ─── Content output ──────────────────────────────────────

    /// Print a single styled line.
    pub fn print(&mut self, line: &Line) {
        self.canvas.print_line(line);
    }

    /// Print multiple styled lines.
    pub fn print_lines(&mut self, lines: &[Line]) {
        self.canvas.print_lines(lines);
    }

    /// Print an empty line for spacing.
    pub fn blank_line(&mut self) {
        self.canvas.blank_line();
    }

    /// Print a full-width horizontal separator.
    pub fn separator(&mut self) {
        let w = self.canvas.content_width();
        self.canvas.print_line(&lines::separator(w));
    }

    // ─── Workers ─────────────────────────────────────────────

    /// Increment the background worker count.
    pub fn add_worker(&mut self) {
        self.indicator.add_worker();
        self.sync_status_bar();
    }

    /// Decrement the background worker count.
    pub fn remove_worker(&mut self) {
        self.indicator.remove_worker();
        self.sync_status_bar();
    }

    /// Set the worker count to an exact value (polling-based sync).
    pub fn set_worker_count(&mut self, count: usize) {
        self.indicator.set_worker_count(count);
        self.sync_status_bar();
    }

    /// Reset the worker count to zero (used when all workers are stopped).
    pub fn reset_workers(&mut self) {
        self.indicator.reset_workers();
        self.sync_status_bar();
    }

    // ─── Queries ─────────────────────────────────────────────

    /// Usable content width (terminal width minus scrollbar).
    pub fn content_width(&self) -> u16 {
        self.canvas.content_width()
    }

    // ─── Notifications ──────────────────────────────────────

    /// Send a desktop notification (auto-detects terminal support).
    pub fn notify(&self, message: &str) {
        notify::notify(message);
    }

    // ─── Internal ────────────────────────────────────────────

    fn sync_status_bar(&mut self) {
        let rendered = self.indicator.render(self.canvas.width());
        self.canvas.set_status_bar(&rendered);
    }
}

/// Builder and runner for a chat UI session.
///
/// ```ignore
/// Chat::new("❯ ")
///     .title("myapp", "chat assistant")
///     .autocomplete_trigger('@')
///     .run(handler, || rx.try_recv().ok())?;
/// ```
pub struct Chat {
    prompt: String,
    title: Option<String>,
    subtitle: Option<String>,
    autocomplete_trigger: Option<char>,
    exit_confirm_timeout: Duration,
}

impl Chat {
    /// Create a new chat session with the given input prompt.
    pub fn new(prompt: &str) -> Self {
        Self {
            prompt: prompt.to_string(),
            title: None,
            subtitle: None,
            autocomplete_trigger: None,
            exit_confirm_timeout: Duration::from_secs(5),
        }
    }

    /// Set the pre-raw-mode banner title and optional subtitle.
    ///
    /// Clears the screen and prints a styled header before entering raw mode.
    /// The title is bold, the subtitle (if any) is dimmed.
    pub fn title(mut self, title: &str, subtitle: &str) -> Self {
        self.title = Some(title.to_string());
        self.subtitle = Some(subtitle.to_string());
        self
    }

    /// Set the character that triggers autocomplete (e.g. `'@'`).
    pub fn autocomplete_trigger(mut self, trigger: char) -> Self {
        self.autocomplete_trigger = Some(trigger);
        self
    }

    /// Set how long the "Press Ctrl+C again to exit" hint stays active.
    ///
    /// After this duration, the hint is cleared and the user must start
    /// the double-tap sequence again. Default: 5 seconds.
    pub fn exit_confirm_timeout(mut self, timeout: Duration) -> Self {
        self.exit_confirm_timeout = timeout;
        self
    }

    /// Run the chat event loop.
    ///
    /// Enters raw mode, optionally prints a banner, then loops until the
    /// user quits. The `recv` closure is called repeatedly to drain backend
    /// messages (should be non-blocking, like `rx.try_recv().ok()`).
    ///
    /// Raw mode is restored when this returns (via Canvas drop).
    pub fn run<M>(
        self,
        mut handler: impl Handler<M>,
        mut recv: impl FnMut() -> Option<M>,
    ) -> io::Result<()> {
        let canvas = Canvas::enter(&self.prompt)?;
        let mut cx = ChatContext::new(canvas);
        let mut input = InputLine::new();
        input.set_exit_confirm_timeout(self.exit_confirm_timeout);
        if let Some(trigger) = self.autocomplete_trigger {
            input.set_autocomplete_trigger(Some(trigger));
        }
        let mut last_spinner_tick = Instant::now();
        let mut drag_anchor: Option<usize> = None;

        // Banner
        if let Some(title) = &self.title {
            let mut spans = vec![
                Span::styled(format!("  {title}"), Style::new().bold()),
            ];
            if let Some(subtitle) = &self.subtitle {
                spans.push(Span::styled(
                    format!(" {subtitle}"),
                    Style::new().fg(Color::DarkGrey),
                ));
            }
            let w = cx.canvas.content_width();
            cx.canvas.set_banner(&[
                Line::new(spans),
                lines::separator(w),
            ]);
        }

        cx.canvas.update_bubble(&input);

        loop {
            // 1. Drain backend messages
            while let Some(msg) = recv() {
                handler.on_message(msg, &mut cx);
            }

            // 2. Poll terminal events
            if let Some(event) = poll_event(Duration::from_millis(30))? {
                match event {
                    TermEvent::Resize(w, h) => {
                        cx.canvas.handle_resize(w, h, &input);
                    }
                    TermEvent::ScrollUp(n) => {
                        cx.canvas.scroll_up(n);
                    }
                    TermEvent::ScrollDown(n) => {
                        cx.canvas.scroll_down(n);
                    }
                    TermEvent::MouseDown { row, modifiers, .. } => {
                        let cmd = modifiers.contains(KeyModifiers::SUPER);
                        if let Some(msg_id) = cx.canvas.message_at_viewport_row(row) {
                            if cmd {
                                cx.canvas.toggle_selection(msg_id);
                            } else {
                                let already = cx.canvas.is_message_selected(msg_id);
                                let multi = cx.canvas.selection_count() > 1;
                                if already && !multi {
                                    cx.canvas.clear_selection();
                                } else {
                                    cx.canvas.set_selection_single(msg_id);
                                }
                            }
                            drag_anchor = Some(msg_id);
                        } else {
                            if !cmd {
                                cx.canvas.clear_selection();
                            }
                            drag_anchor = None;
                        }
                    }
                    TermEvent::MouseDrag { row, .. } => {
                        if let Some(anchor) = drag_anchor {
                            if let Some(msg_id) = cx.canvas.message_at_viewport_row(row) {
                                let (from, to) = if anchor <= msg_id {
                                    (anchor, msg_id)
                                } else {
                                    (msg_id, anchor)
                                };
                                cx.canvas.select_range(from, to);
                            }
                        }
                    }
                    TermEvent::MouseUp { .. } => {
                        drag_anchor = None;
                    }
                    TermEvent::Key(key) => {
                        // Cmd+C → copy selected messages to clipboard.
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::SUPER)
                            && cx.canvas.has_selection()
                        {
                            let text = cx.canvas.selected_text();
                            copy_to_clipboard(&text);
                            cx.canvas.clear_selection();
                            continue;
                        }

                        match key.code {
                            KeyCode::PageUp => {
                                cx.canvas.scroll_up(cx.canvas.viewport_height());
                                continue;
                            }
                            KeyCode::PageDown => {
                                cx.canvas.scroll_down(cx.canvas.viewport_height());
                                continue;
                            }
                            _ => {}
                        }

                        let old_ac_count = input
                            .autocomplete
                            .as_ref()
                            .map(|ac| ac.matches.len())
                            .unwrap_or(0);

                        let resolve: Option<&dyn Fn(&str) -> Vec<String>> =
                            if self.autocomplete_trigger.is_some() {
                                Some(&|query| handler.autocomplete(query))
                            } else {
                                None
                            };

                        let action = input.handle_key(key.code, key.modifiers, resolve);

                        match action {
                            InputAction::None => {}
                            InputAction::Quit => {
                                handler.on_quit();
                                return Ok(());
                            }
                            InputAction::ConfirmExit => {
                                cx.canvas
                                    .set_hint(Some("Press Ctrl+C again to exit.".into()));
                                cx.canvas.update_bubble(&input);
                            }
                            InputAction::Submit(text) => {
                                cx.canvas.set_hint(None);
                                if old_ac_count > 0 {
                                    cx.canvas.clear_autocomplete(old_ac_count);
                                }
                                // Interrupt any in-flight operation before sending new prompt
                                handler.on_interrupt();
                                cx.canvas.scroll_to_bottom();
                                cx.finish_stream_if_active();
                                cx.canvas.print_lines(&lines::user_message(&text));
                                cx.canvas.update_bubble(&input);
                                cx.indicator.set_activity(Activity::Thinking);
                                cx.sync_status_bar();
                                handler.on_submit(text, &mut cx);
                            }
                            InputAction::Interrupt => {
                                cx.canvas.set_hint(None);
                                if old_ac_count > 0 {
                                    cx.canvas.clear_autocomplete(old_ac_count);
                                }
                                cx.canvas.update_bubble(&input);
                                handler.on_interrupt();
                            }
                            InputAction::Changed => {
                                cx.canvas.clear_selection();
                                cx.canvas.set_hint(None);
                                if old_ac_count > 0 {
                                    cx.canvas.clear_autocomplete(old_ac_count);
                                }
                                cx.canvas.update_bubble(&input);
                                if let Some(ac) = &input.autocomplete
                                    && !ac.matches.is_empty()
                                {
                                    cx.canvas
                                        .draw_autocomplete(&ac.matches, ac.selected);
                                }
                            }
                        }
                    }
                }
            }

            // 3. Tick spinner
            if cx.indicator.is_active()
                && last_spinner_tick.elapsed() >= Duration::from_millis(80)
            {
                cx.indicator.tick();
                cx.sync_status_bar();
                last_spinner_tick = Instant::now();
            }

            // 4. Clear expired exit confirmation hint
            if input.check_exit_expired() {
                cx.canvas.set_hint(None);
                cx.canvas.update_bubble(&input);
            }
        }
    }
}

/// Copy text to the system clipboard (macOS: pbcopy).
fn copy_to_clipboard(text: &str) {
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write;
            stdin.write_all(text.as_bytes()).ok();
        }
        child.wait().ok();
    }
}
