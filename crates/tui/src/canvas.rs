use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Stdout, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self, ClearType};

use crate::input::InputLine;
use crate::style::{self, Line, Span, Style};

/// Maximum number of content lines in the input bubble (not counting borders).
const MAX_BUBBLE_LINES: u16 = 10;

/// Background color for selected messages.
const HIGHLIGHT_BG: Color = Color::Rgb { r: 30, g: 40, b: 60 };

// ─── Logical buffer ───────────────────────────────────────────
//
// The canvas keeps TWO parallel representations of every piece of output:
//
//   `logical`  — original content, one entry per semantic unit. Used to
//                rebuild the rendered buffer when the terminal is resized.
//
//   `buffer`   — pre-wrapped single-row entries at the current
//                `content_width()`. Each entry occupies exactly one
//                terminal row. This is what `redraw_viewport` iterates.
//
// The invariant "1 buffer entry == 1 terminal row" is what makes absolute
// cursor positioning in `redraw_viewport` correct.

/// Original content stored for resize re-rendering.
#[derive(Clone)]
enum LogicalEntry {
    /// A styled line — re-wrapped from spans at rebuild time.
    Styled(Line),
    /// A single plain-text line — re-wrapped from chars at rebuild time.
    PlainText(String),
    /// Markdown source — re-rendered via termimad at rebuild time.
    Markdown(String),
    /// Pre-rendered ANSI lines from an external renderer. Stored as-is;
    /// cannot be meaningfully re-wrapped on resize (no source text).
    RawAnsi(Vec<String>),
    /// Empty line.
    Blank,
}

/// A single rendered row for display (one terminal row wide).
#[derive(Clone)]
enum BufferedLine {
    Styled(Line),
    Raw(String),
    Blank,
}

// ─── Canvas ───────────────────────────────────────────────────

/// Terminal surface with a pinned input bubble at the bottom.
///
/// Uses ANSI scroll regions to split the terminal into two zones:
/// - **Scroll region** (top) — messages, streaming text, status lines.
/// - **Input bubble** (bottom) — pinned, always visible.
pub struct Canvas {
    out: Stdout,
    prompt: String,
    streaming: bool,
    bubble_height: u16,
    term_rows: u16,
    term_cols: u16,

    /// Logical content — original entries for rebuilding the rendered buffer.
    logical: VecDeque<LogicalEntry>,
    /// Rendered single-row entries at the current `content_width()`.
    buffer: VecDeque<BufferedLine>,

    /// Lines from bottom. 0 = showing latest output.
    scroll_offset: usize,
    /// Whether we auto-scroll with new output.
    follow: bool,

    /// Partial line being streamed (not yet terminated by `\n`).
    streaming_line: String,
    /// Buffer index where the current stream block started.
    stream_start_idx: usize,
    /// Logical index where the current stream block started.
    logical_stream_start: usize,
    /// Accumulates finalized plain-text streaming lines (natural `\n` only,
    /// no soft-wraps) so `end_streaming` can build logical entries.
    stream_logical_text: String,

    /// Cached input text for internal bubble redraws.
    cached_input_text: String,
    /// Cached cursor position for internal bubble redraws.
    cached_cursor: usize,
    /// Hint text shown in the input bubble when input is empty.
    hint: Option<String>,

    /// Status bar lines rendered between the scroll region and the input bubble.
    /// Empty when no status bar is visible.
    status_bar_lines: Vec<Line>,
    /// Height of the status bar in rows (0 when hidden).
    status_bar_height: u16,

    /// Banner lines rendered at the top of the screen (fixed, non-scrolling).
    banner_lines: Vec<Line>,
    /// Height of the banner in rows (0 when hidden).
    banner_height: u16,

    // ─── Message tracking ────────────────────────────────────
    /// Message id for each logical entry (parallel to `logical`).
    logical_message_id: VecDeque<Option<usize>>,
    /// Message id for each buffer row (parallel to `buffer`).
    buffer_message_id: VecDeque<Option<usize>>,
    /// Total number of messages created.
    message_count: usize,
    /// Active message being built (during streaming).
    current_message: Option<usize>,

    // ─── Selection ───────────────────────────────────────────
    /// Indices of selected messages.
    selected_messages: BTreeSet<usize>,
}

impl Canvas {
    /// Enter raw mode, set up scroll regions, and create the canvas.
    pub fn enter(prompt: &str) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // Clear screen before taking over the terminal
        write!(io::stdout(), "\x1b[2J\x1b[H")?;
        io::stdout().flush()?;
        crossterm::execute!(io::stdout(), EnableMouseCapture, Hide)?;
        let _ = crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );

        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let bubble_height: u16 = 3;

        let mut canvas = Self {
            out: io::stdout(),
            prompt: prompt.to_string(),
            streaming: false,
            bubble_height,
            term_rows: rows,
            term_cols: cols,
            logical: VecDeque::new(),
            buffer: VecDeque::new(),
            scroll_offset: 0,
            follow: true,
            streaming_line: String::new(),
            stream_start_idx: 0,
            logical_stream_start: 0,
            stream_logical_text: String::new(),
            cached_input_text: String::new(),
            cached_cursor: 0,
            hint: None,
            status_bar_lines: Vec::new(),
            status_bar_height: 0,
            banner_lines: Vec::new(),
            banner_height: 0,
            logical_message_id: VecDeque::new(),
            buffer_message_id: VecDeque::new(),
            message_count: 0,
            current_message: None,
            selected_messages: BTreeSet::new(),
        };

        canvas.apply_scroll_region();
        let sr_top = canvas.scroll_region_top();
        write!(canvas.out, "\x1b[{sr_top};1H").ok();
        canvas.out.flush().ok();

        Ok(canvas)
    }

    // ─── Scroll region management ─────────────────────────────

    fn apply_scroll_region(&mut self) {
        let scroll_top = self.banner_height + 1;
        let scroll_bottom = self.term_rows
            .saturating_sub(self.bubble_height)
            .saturating_sub(self.status_bar_height);
        write!(self.out, "\x1b[{scroll_top};{scroll_bottom}r").ok();
        self.out.flush().ok();
    }

    /// First terminal row of the scroll region (1-based).
    fn scroll_region_top(&self) -> u16 {
        self.banner_height + 1
    }

    /// Height of the scroll region in rows.
    pub fn viewport_height(&self) -> u16 {
        self.term_rows
            .saturating_sub(self.bubble_height)
            .saturating_sub(self.status_bar_height)
            .saturating_sub(self.banner_height)
    }

    /// Handle terminal resize.
    pub fn handle_resize(&mut self, cols: u16, rows: u16, input: &InputLine) {
        self.term_cols = cols;
        self.term_rows = rows;
        let needed = (self.visual_line_count(&input.text) as u16).min(MAX_BUBBLE_LINES) + 2;
        self.bubble_height = needed;
        self.apply_scroll_region();
        self.rebuild_buffer();
        self.draw_banner();
        self.redraw_viewport();
        self.draw_status_bar();
        self.draw_bubble(input);
    }

    // ─── Width helpers ────────────────────────────────────────

    /// Full terminal width in columns.
    pub fn width(&self) -> u16 {
        self.term_cols
    }

    /// Usable content width: terminal width minus the scrollbar column.
    ///
    /// All content (styled lines, markdown rendering, word-wrapping) must
    /// stay within this width so the scrollbar never overlaps content and
    /// the 1-buffer-entry-per-row invariant holds on any redraw.
    pub fn content_width(&self) -> u16 {
        self.term_cols.saturating_sub(1)
    }

    // ─── Logical → rendered buffer rebuild ───────────────────

    /// Rebuild the rendered `buffer` from `logical` at the current
    /// `content_width()`. Called after a terminal resize.
    ///
    /// Also re-adds any in-progress streaming entries so viewport
    /// calculations remain correct while a stream is active.
    fn rebuild_buffer(&mut self) {
        // Take logical out to avoid borrow conflicts while writing to buffer.
        let logical = std::mem::take(&mut self.logical);
        let logical_msg_ids = std::mem::take(&mut self.logical_message_id);
        self.buffer.clear();
        self.buffer_message_id.clear();

        let width = self.content_width() as usize;
        let cw = self.content_width();

        for (i, entry) in logical.iter().enumerate() {
            let msg_id = logical_msg_ids.get(i).copied().flatten();
            let before = self.buffer.len();
            Self::push_logical_to_buffer(&mut self.buffer, entry, width, cw);
            let after = self.buffer.len();
            for _ in before..after {
                self.buffer_message_id.push_back(msg_id);
            }
        }

        self.logical = logical;
        self.logical_message_id = logical_msg_ids;

        // If a stream is active, the streaming entries are NOT yet in `logical`
        // (they're added at end_streaming / replace_stream_block_markdown).
        // Re-add the finalized streaming lines from stream_logical_text and
        // update stream_start_idx / logical_stream_start to point past them.
        if self.streaming {
            self.stream_start_idx = self.buffer.len();
            self.logical_stream_start = self.logical.len();
            for line in self.stream_logical_text.lines() {
                for chunk in wrap_plain_text(line, width) {
                    self.buffer.push_back(BufferedLine::Raw(chunk));
                    self.buffer_message_id.push_back(self.current_message);
                }
            }
            // streaming_line (partial row) is accounted for by buffer_len().
        }
    }

    /// Expand a single `LogicalEntry` into rendered rows and append to `buf`.
    fn push_logical_to_buffer(
        buf: &mut VecDeque<BufferedLine>,
        entry: &LogicalEntry,
        width: usize,
        cw: u16,
    ) {
        match entry {
            LogicalEntry::Styled(line) => {
                for row in wrap_styled_line(line, width) {
                    buf.push_back(BufferedLine::Styled(row));
                }
            }
            LogicalEntry::PlainText(text) => {
                for chunk in wrap_plain_text(text, width) {
                    buf.push_back(BufferedLine::Raw(chunk));
                }
            }
            LogicalEntry::Markdown(source) => {
                #[cfg(feature = "markdown")]
                for row in crate::markdown::render_markdown_lines(source, cw) {
                    buf.push_back(BufferedLine::Raw(row));
                }
                #[cfg(not(feature = "markdown"))]
                for chunk in wrap_plain_text(source, width) {
                    buf.push_back(BufferedLine::Raw(chunk));
                }
            }
            LogicalEntry::RawAnsi(lines) => {
                // Cannot re-wrap ANSI lines; output as-is.
                for line in lines {
                    buf.push_back(BufferedLine::Raw(line.clone()));
                }
            }
            LogicalEntry::Blank => {
                buf.push_back(BufferedLine::Blank);
            }
        }
    }

    // ─── Scrollback buffer ───────────────────────────────────

    /// Total visual rows in the buffer (including any in-progress streaming row).
    fn buffer_len(&self) -> usize {
        self.buffer.len() + if self.streaming && !self.streaming_line.is_empty() { 1 } else { 0 }
    }

    /// Redraw the entire viewport from the buffer.
    fn redraw_viewport(&mut self) {
        let sr_height = self.viewport_height() as usize;
        if sr_height == 0 {
            return;
        }

        let total = self.buffer_len();
        let view_end = total.saturating_sub(self.scroll_offset);
        let view_start = view_end.saturating_sub(sr_height);
        let is_full = (view_end - view_start) >= sr_height;

        let draw_start = if self.follow && !self.streaming && is_full {
            view_start + 1
        } else {
            view_start
        };
        let draw_count = view_end.saturating_sub(draw_start).min(sr_height);

        // Pre-compute selection state for visible rows.
        let selected_rows: Vec<bool> = (0..draw_count)
            .map(|i| {
                let buf_idx = draw_start + i;
                self.is_buffer_row_selected(buf_idx)
            })
            .collect();

        let sr_top = self.scroll_region_top();

        // Clear all rows first.
        for i in 0..(sr_height as u16) {
            let row = sr_top + i;
            write!(self.out, "\x1b[{row};1H{}", terminal::Clear(ClearType::CurrentLine)).ok();
        }

        // Draw content lines.
        for i in 0..draw_count {
            let buf_idx = draw_start + i;
            if buf_idx < total {
                let row = sr_top + i as u16;
                let selected = selected_rows[i];

                write!(self.out, "\x1b[{row};1H").ok();
                if selected {
                    // Clear the line with highlight bg (BCE fills the row).
                    write!(self.out, "{}", SetBackgroundColor(HIGHLIGHT_BG)).ok();
                    write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
                }

                self.render_buffered_line(buf_idx, selected);

                if selected {
                    write!(self.out, "{}", ResetColor).ok();
                }
            }
        }

        // Park cursor.
        if self.follow {
            if !self.streaming {
                let cursor_row = sr_top + (draw_count as u16).min(sr_height as u16);
                write!(self.out, "\x1b[{cursor_row};1H").ok();
            }
        } else {
            let sr_bottom = sr_top + self.viewport_height().saturating_sub(1);
            write!(self.out, "\x1b[{sr_bottom};1H").ok();
        }

        self.out.flush().ok();
        self.draw_scrollbar();
        self.draw_status_bar();
        self.redraw_bubble_cached();
    }

    /// Render a single buffered row at the current cursor position.
    fn render_buffered_line(&mut self, idx: usize, selected: bool) {
        if idx < self.buffer.len() {
            match &self.buffer[idx] {
                BufferedLine::Styled(line) => {
                    if selected {
                        write_line_content_highlighted(&mut self.out, line, HIGHLIGHT_BG).ok();
                    } else {
                        style::write_line_content(&mut self.out, line).ok();
                    }
                }
                BufferedLine::Raw(text) => {
                    write!(self.out, "{text}").ok();
                }
                BufferedLine::Blank => {}
            }
        } else if self.streaming && !self.streaming_line.is_empty() {
            // Partial streaming row: truncate to content_width so it cannot
            // overflow into the scrollbar column or wrap to the next row.
            let cw = self.content_width() as usize;
            let display: String = self.streaming_line.chars().take(cw).collect();
            write!(self.out, "{display}").ok();
        }
    }

    // ─── Scrolling output ────────────────────────────────────

    /// Print a styled line in the scroll region.
    pub fn print_line(&mut self, line: &Line) {
        let width = self.content_width() as usize;
        let rows = wrap_styled_line(line, width);
        let msg_id = self.next_message_id();

        self.logical.push_back(LogicalEntry::Styled(line.clone()));
        self.logical_message_id.push_back(Some(msg_id));
        for row in &rows {
            self.buffer.push_back(BufferedLine::Styled(row.clone()));
            self.buffer_message_id.push_back(Some(msg_id));
        }
        if self.follow {
            for row in &rows {
                style::write_line(&mut self.out, row).ok();
            }
            self.out.flush().ok();
            self.draw_scrollbar();
        }
    }

    /// Print multiple styled lines in the scroll region (as a single message).
    pub fn print_lines(&mut self, lines: &[Line]) {
        let width = self.content_width() as usize;
        let msg_id = self.next_message_id();
        for line in lines {
            let rows = wrap_styled_line(line, width);
            self.logical.push_back(LogicalEntry::Styled(line.clone()));
            self.logical_message_id.push_back(Some(msg_id));
            for row in &rows {
                self.buffer.push_back(BufferedLine::Styled(row.clone()));
                self.buffer_message_id.push_back(Some(msg_id));
            }
            if self.follow {
                for row in &rows {
                    style::write_line(&mut self.out, row).ok();
                }
            }
        }
        if self.follow {
            self.out.flush().ok();
            self.draw_scrollbar();
        }
    }

    /// Print a blank line in the scroll region.
    pub fn blank_line(&mut self) {
        self.logical.push_back(LogicalEntry::Blank);
        self.logical_message_id.push_back(None);
        self.buffer.push_back(BufferedLine::Blank);
        self.buffer_message_id.push_back(None);
        if self.follow {
            write!(self.out, "\r\n").ok();
            self.out.flush().ok();
            self.draw_scrollbar();
        }
    }

    // ─── Streaming ───────────────────────────────────────────

    /// Begin a streaming block.
    pub fn begin_streaming(&mut self) {
        self.streaming = true;
        self.streaming_line.clear();
        self.stream_logical_text.clear();
        self.stream_start_idx = self.buffer.len();
        self.logical_stream_start = self.logical.len();
        // Start a new message for this stream block.
        self.current_message = Some(self.message_count);
        self.message_count += 1;
    }

    /// Write streaming text. Handles `\n` → `\r\n` for raw mode.
    ///
    /// Each newline finalizes the current streaming line into the rendered
    /// buffer as a single `Raw` row and appends it to `stream_logical_text`
    /// for later logical-buffer population.
    pub fn print_streaming(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                let finished = std::mem::take(&mut self.streaming_line);
                // Record in logical accumulator (natural newline only).
                self.stream_logical_text.push_str(&finished);
                self.stream_logical_text.push('\n');
                // One row in the rendered buffer.
                self.buffer.push_back(BufferedLine::Raw(finished));
                self.buffer_message_id.push_back(self.current_message);
                if self.follow {
                    write!(self.out, "\r\n").ok();
                }
            } else {
                self.streaming_line.push(ch);
                if self.follow {
                    write!(self.out, "{ch}").ok();
                }
            }
        }
        if self.follow {
            self.out.flush().ok();
        }
    }

    /// Finish a streaming block. Ensures a trailing newline.
    ///
    /// Finalizes `stream_logical_text` into `PlainText` logical entries so
    /// the content is available for resize re-rendering.
    pub fn end_streaming(&mut self) {
        if !self.streaming_line.is_empty() {
            let finished = std::mem::take(&mut self.streaming_line);
            self.stream_logical_text.push_str(&finished);
            self.buffer.push_back(BufferedLine::Raw(finished));
            self.buffer_message_id.push_back(self.current_message);
        }
        self.streaming_line.clear();
        self.streaming = false;

        // Rebuild the rendered buffer entries for the stream block with proper
        // word-wrapping, and populate the logical buffer.
        let width = self.content_width() as usize;
        let logical_text = std::mem::take(&mut self.stream_logical_text);
        let msg_id = self.current_message;

        self.buffer.truncate(self.stream_start_idx);
        self.buffer_message_id.truncate(self.stream_start_idx);
        self.logical.truncate(self.logical_stream_start);
        self.logical_message_id.truncate(self.logical_stream_start);

        for line in logical_text.lines() {
            self.logical.push_back(LogicalEntry::PlainText(line.to_string()));
            self.logical_message_id.push_back(msg_id);
            for chunk in wrap_plain_text(line, width) {
                self.buffer.push_back(BufferedLine::Raw(chunk));
                self.buffer_message_id.push_back(msg_id);
            }
        }

        self.current_message = None;

        if self.follow {
            write!(self.out, "\r\n").ok();
            self.out.flush().ok();
            self.draw_scrollbar();
        }
    }

    /// Whether we're currently in a streaming block.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Replace the raw streamed lines with pre-rendered ANSI lines (e.g.
    /// from an external renderer). The original source is not retained;
    /// these lines cannot be re-wrapped on resize.
    ///
    /// Prefer `replace_stream_block_markdown` when you have the source text.
    pub fn replace_stream_block_raw(&mut self, lines: Vec<String>) {
        let msg_id = self.current_message;
        self.buffer.truncate(self.stream_start_idx);
        self.buffer_message_id.truncate(self.stream_start_idx);
        for line in &lines {
            self.buffer.push_back(BufferedLine::Raw(line.clone()));
            self.buffer_message_id.push_back(msg_id);
        }
        self.logical.truncate(self.logical_stream_start);
        self.logical_message_id.truncate(self.logical_stream_start);
        self.logical.push_back(LogicalEntry::RawAnsi(lines));
        self.logical_message_id.push_back(msg_id);
        self.current_message = None;
        if self.follow {
            self.redraw_viewport();
        }
    }

    /// Replace the raw streamed lines with pre-rendered markdown output,
    /// retaining the original `source` text for resize re-rendering.
    pub fn replace_stream_block_markdown(&mut self, source: String, rendered: Vec<String>) {
        let msg_id = self.current_message;
        self.buffer.truncate(self.stream_start_idx);
        self.buffer_message_id.truncate(self.stream_start_idx);
        for line in &rendered {
            self.buffer.push_back(BufferedLine::Raw(line.clone()));
            self.buffer_message_id.push_back(msg_id);
        }
        self.logical.truncate(self.logical_stream_start);
        self.logical_message_id.truncate(self.logical_stream_start);
        self.logical.push_back(LogicalEntry::Markdown(source));
        self.logical_message_id.push_back(msg_id);
        self.current_message = None;
        if self.follow {
            self.redraw_viewport();
        }
    }

    // ─── Scroll control ──────────────────────────────────────

    /// Scroll up by `n` lines.
    pub fn scroll_up(&mut self, n: u16) {
        let vh = self.viewport_height() as usize;
        let max_offset = self.buffer_len().saturating_sub(vh);
        self.scroll_offset = (self.scroll_offset + n as usize).min(max_offset);
        self.follow = false;
        self.redraw_viewport();
    }

    /// Scroll down by `n` lines.
    pub fn scroll_down(&mut self, n: u16) {
        if self.scroll_offset <= n as usize {
            self.scroll_offset = 0;
            self.follow = true;
        } else {
            self.scroll_offset -= n as usize;
        }
        self.redraw_viewport();
    }

    /// Scroll to the bottom and resume auto-follow.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.follow = true;
        self.redraw_viewport();
    }

    // ─── Scrollbar ───────────────────────────────────────────

    fn draw_scrollbar(&mut self) {
        let vh = self.viewport_height() as usize;
        if vh == 0 {
            return;
        }

        write!(self.out, "\x1b7").ok(); // save cursor

        let col = self.term_cols;
        let total = self.buffer_len();

        let sr_top = self.scroll_region_top() as usize;

        if total <= vh {
            for i in 0..vh {
                let row = sr_top + i;
                write!(self.out, "\x1b[{row};{col}H ").ok();
            }
            write!(self.out, "\x1b8").ok();
            self.out.flush().ok();
            return;
        }

        let thumb_size = (vh * vh / total).max(1);
        let max_offset = total.saturating_sub(vh);
        let scrollable_rows = vh.saturating_sub(thumb_size);
        let thumb_top = if max_offset == 0 {
            scrollable_rows
        } else {
            scrollable_rows - (self.scroll_offset * scrollable_rows / max_offset)
        };

        let track_bg = Color::Rgb { r: 40, g: 40, b: 40 };
        let thumb_bg = Color::Rgb { r: 100, g: 100, b: 100 };

        for row_idx in 0..vh {
            let row = sr_top + row_idx;
            write!(self.out, "\x1b[{row};{col}H").ok();
            let bg = if row_idx >= thumb_top && row_idx < thumb_top + thumb_size {
                thumb_bg
            } else {
                track_bg
            };
            write!(self.out, "{} {}", SetBackgroundColor(bg), ResetColor).ok();
        }

        write!(self.out, "\x1b8").ok(); // restore cursor
        self.out.flush().ok();
    }

    // ─── Input bubble ────────────────────────────────────────

    /// Update the bubble: recalculate height if needed, then redraw.
    pub fn update_bubble(&mut self, input: &InputLine) {
        let needed = (self.visual_line_count(&input.text) as u16).min(MAX_BUBBLE_LINES) + 2;
        if needed != self.bubble_height {
            self.clear_bubble_area();
            self.bubble_height = needed;
            self.apply_scroll_region();
            self.redraw_viewport();
        }
        self.draw_bubble(input);
    }

    /// Number of visual lines after wrapping text to the bubble's text width.
    fn visual_line_count(&self, text: &str) -> usize {
        let inner_w = (self.term_cols as usize).saturating_sub(4);
        let prompt_width = self.prompt.chars().count();
        let text_width = inner_w.saturating_sub(prompt_width);
        wrap_text(text, text_width).len()
    }

    /// Set or clear the hint text shown in the bubble when input is empty.
    pub fn set_hint(&mut self, hint: Option<String>) {
        self.hint = hint;
    }

    // ─── Status bar ──────────────────────────────────────

    /// Set the status bar content. Pass an empty slice to hide it.
    ///
    /// Recalculates layout and redraws if the height changed.
    pub fn set_status_bar(&mut self, lines: &[Line]) {
        let new_height = lines.len() as u16;
        let height_changed = new_height != self.status_bar_height;

        self.status_bar_lines = lines.to_vec();
        self.status_bar_height = new_height;

        if height_changed {
            self.apply_scroll_region();
            self.redraw_viewport();
        }

        self.draw_status_bar();
    }

    /// Clear the status bar (equivalent to `set_status_bar(&[])`).
    pub fn clear_status_bar(&mut self) {
        if self.status_bar_height == 0 {
            return;
        }
        self.set_status_bar(&[]);
    }

    /// Render the status bar between the scroll region and the input bubble.
    fn draw_status_bar(&mut self) {
        if self.status_bar_height == 0 {
            return;
        }

        write!(self.out, "\x1b7").ok(); // save cursor

        let bar_top = self.term_rows
            .saturating_sub(self.bubble_height)
            .saturating_sub(self.status_bar_height)
            + 1;

        for (i, line) in self.status_bar_lines.iter().enumerate() {
            let row = bar_top + i as u16;
            write!(self.out, "\x1b[{row};1H").ok();
            write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
            style::write_line_content(&mut self.out, line).ok();
        }

        write!(self.out, "\x1b8").ok(); // restore cursor
        self.out.flush().ok();
    }

    // ─── Banner ──────────────────────────────────────────

    /// Set the banner content at the top of the screen.
    ///
    /// Recalculates layout and redraws if the height changed.
    pub fn set_banner(&mut self, lines: &[Line]) {
        let new_height = lines.len() as u16;
        let height_changed = new_height != self.banner_height;

        self.banner_lines = lines.to_vec();
        self.banner_height = new_height;

        if height_changed {
            self.apply_scroll_region();
            self.redraw_viewport();
        }

        self.draw_banner();
    }

    /// Render the banner at the top of the screen.
    fn draw_banner(&mut self) {
        if self.banner_height == 0 {
            return;
        }

        write!(self.out, "\x1b7").ok(); // save cursor

        for (i, line) in self.banner_lines.iter().enumerate() {
            let row = (i + 1) as u16;
            write!(self.out, "\x1b[{row};1H").ok();
            write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
            style::write_line_content(&mut self.out, line).ok();
        }

        write!(self.out, "\x1b8").ok(); // restore cursor
        self.out.flush().ok();
    }

    pub fn draw_bubble(&mut self, input: &InputLine) {
        self.cached_input_text = input.text.clone();
        self.cached_cursor = input.cursor;
        self.draw_bubble_inner();
    }

    fn redraw_bubble_cached(&mut self) {
        self.draw_bubble_inner();
    }

    fn draw_bubble_inner(&mut self) {
        write!(self.out, "\x1b7").ok(); // save cursor

        let bubble_top = self.term_rows - self.bubble_height + 1;
        let w = self.term_cols as usize;

        let inner_w = w.saturating_sub(4);
        let horiz = "─".repeat(inner_w + 2);
        let prompt_width = self.prompt.chars().count();
        let text_width = inner_w.saturating_sub(prompt_width);

        // Top border
        write!(self.out, "\x1b[{bubble_top};1H").ok();
        write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
        style::write_span(
            &mut self.out,
            &Span::styled(format!("╭{horiz}╮"), Style::new().fg(Color::DarkGrey)),
        )
        .ok();

        // Wrap text into visual lines
        let visual_lines = wrap_text(&self.cached_input_text, text_width);
        let visible_count = visual_lines.len().min(MAX_BUBBLE_LINES as usize);

        let cursor = self.cached_cursor.min(self.cached_input_text.len());
        let cursor_vline = visual_lines
            .iter()
            .position(|vl| cursor >= vl.byte_start && cursor < vl.byte_end)
            .unwrap_or(visual_lines.len() - 1);
        let cursor_vcol = cursor.saturating_sub(visual_lines[cursor_vline].byte_start);

        let indent: String = " ".repeat(prompt_width);
        let border_fg = Style::new().fg(Color::DarkGrey);

        for (i, vline) in visual_lines.iter().enumerate().take(visible_count) {
            let row = bubble_top + 1 + i as u16;
            write!(self.out, "\x1b[{row};1H").ok();
            write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();

            style::write_span(&mut self.out, &Span::styled("│ ", border_fg.clone())).ok();

            if i == 0 {
                write!(
                    self.out,
                    "{}{}{}{}",
                    SetForegroundColor(Color::DarkMagenta),
                    SetAttribute(Attribute::Bold),
                    self.prompt,
                    SetAttribute(Attribute::Reset),
                )
                .ok();
            } else {
                write!(self.out, "{indent}").ok();
            }

            let is_cursor_line = i == cursor_vline;
            if i == 0 && self.cached_input_text.is_empty() && self.hint.is_some() {
                let hint = self.hint.as_ref().unwrap();
                write!(
                    self.out,
                    "{}{}{}{}",
                    SetForegroundColor(Color::DarkGrey),
                    SetAttribute(Attribute::Italic),
                    hint,
                    SetAttribute(Attribute::Reset),
                )
                .ok();
            } else if is_cursor_line {
                self.draw_line_with_cursor(&vline.text, cursor_vcol);
            } else {
                write!(self.out, "{}", vline.text).ok();
            }

            let showing_hint = i == 0 && self.cached_input_text.is_empty() && self.hint.is_some();
            let text_w = if showing_hint {
                prompt_width + self.hint.as_ref().unwrap().chars().count()
            } else {
                prompt_width + display_width_with_cursor(&vline.text, is_cursor_line)
            };
            let pad = inner_w.saturating_sub(text_w);
            write!(self.out, "{:pad$}", "").ok();
            style::write_span(&mut self.out, &Span::styled(" │", border_fg.clone())).ok();
        }

        // Clear extra rows if bubble shrank.
        for i in visible_count..(self.bubble_height.saturating_sub(2) as usize) {
            let row = bubble_top + 1 + i as u16;
            write!(self.out, "\x1b[{row};1H").ok();
            write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
            style::write_span(&mut self.out, &Span::styled("│ ", border_fg.clone())).ok();
            write!(self.out, "{:inner_w$}", "").ok();
            style::write_span(&mut self.out, &Span::styled(" │", border_fg.clone())).ok();
        }

        // Bottom border
        let bottom_row = bubble_top + self.bubble_height - 1;
        write!(self.out, "\x1b[{bottom_row};1H").ok();
        write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
        style::write_span(
            &mut self.out,
            &Span::styled(format!("╰{horiz}╯"), Style::new().fg(Color::DarkGrey)),
        )
        .ok();

        write!(self.out, "\x1b8").ok(); // restore cursor
        self.out.flush().ok();
    }

    fn draw_line_with_cursor(&mut self, text: &str, cursor_col: usize) {
        if text.is_empty() {
            write!(
                self.out,
                "{}█{}",
                SetForegroundColor(Color::DarkGrey),
                SetAttribute(Attribute::Reset),
            )
            .ok();
            return;
        }

        let before = &text[..cursor_col];
        let after = &text[cursor_col..];
        write!(self.out, "{before}").ok();

        if after.is_empty() {
            write!(
                self.out,
                "{}█{}",
                SetForegroundColor(Color::DarkGrey),
                SetAttribute(Attribute::Reset),
            )
            .ok();
        } else {
            let mut chars = after.chars();
            let cursor_char = chars.next().unwrap();
            write!(
                self.out,
                "{}{}{}",
                SetAttribute(Attribute::Reverse),
                cursor_char,
                SetAttribute(Attribute::Reset),
            )
            .ok();
            let rest: String = chars.collect();
            write!(self.out, "{rest}").ok();
        }
    }

    fn clear_bubble_area(&mut self) {
        write!(self.out, "\x1b7").ok();
        let bubble_top = self.term_rows - self.bubble_height + 1;
        for row in bubble_top..=self.term_rows {
            write!(self.out, "\x1b[{row};1H").ok();
            write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
        }
        write!(self.out, "\x1b8").ok();
        self.out.flush().ok();
    }

    // ─── Autocomplete ────────────────────────────────────────

    pub fn draw_autocomplete(&mut self, matches: &[String], selected: usize) {
        let count = matches.len().min(10);
        if count == 0 {
            return;
        }

        write!(self.out, "\x1b7").ok();
        let bubble_top = self.term_rows - self.bubble_height + 1;
        for (i, m) in matches.iter().take(10).enumerate() {
            let row = bubble_top.saturating_sub(count as u16) + (i as u16);
            write!(self.out, "\x1b[{row};1H").ok();
            write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
            if i == selected {
                style::write_span(
                    &mut self.out,
                    &Span::styled(format!("  > {m}"), Style::new().fg(Color::Cyan).bold()),
                )
                .ok();
            } else {
                style::write_span(
                    &mut self.out,
                    &Span::styled(format!("    {m}"), Style::new().fg(Color::DarkGrey)),
                )
                .ok();
            }
        }

        write!(self.out, "\x1b8").ok();
        self.out.flush().ok();
    }

    pub fn clear_autocomplete(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        write!(self.out, "\x1b7").ok();
        let bubble_top = self.term_rows - self.bubble_height + 1;
        for i in 0..count.min(10) {
            let row = bubble_top.saturating_sub(count as u16) + (i as u16);
            write!(self.out, "\x1b[{row};1H").ok();
            write!(self.out, "{}", terminal::Clear(ClearType::CurrentLine)).ok();
        }
        write!(self.out, "\x1b8").ok();
        self.out.flush().ok();
    }

    // ─── Utility ─────────────────────────────────────────────

    pub fn clear_screen(&mut self) {
        self.buffer.clear();
        self.logical.clear();
        self.logical_message_id.clear();
        self.buffer_message_id.clear();
        self.stream_logical_text.clear();
        self.scroll_offset = 0;
        self.follow = true;
        self.message_count = 0;
        self.current_message = None;
        self.selected_messages.clear();
        write!(self.out, "\x1b[r").ok();
        write!(self.out, "\x1b[2J\x1b[H").ok();
        self.apply_scroll_region();
        self.out.flush().ok();
    }

    pub fn flush(&mut self) {
        self.out.flush().ok();
    }

    // ─── Message tracking ────────────────────────────────────

    /// Allocate a new message id.
    fn next_message_id(&mut self) -> usize {
        let id = self.message_count;
        self.message_count += 1;
        id
    }

    /// Check if a buffer row belongs to a selected message.
    fn is_buffer_row_selected(&self, buf_idx: usize) -> bool {
        self.buffer_message_id
            .get(buf_idx)
            .and_then(|id| *id)
            .is_some_and(|id| self.selected_messages.contains(&id))
    }

    // ─── Hit testing ─────────────────────────────────────────

    /// Return the message id at the given terminal row, if any.
    pub fn message_at_viewport_row(&self, term_row: u16) -> Option<usize> {
        let sr_top = self.scroll_region_top();
        if term_row < sr_top {
            return None; // Banner area
        }

        let sr_height = self.viewport_height() as usize;
        let viewport_offset = (term_row - sr_top) as usize;
        if viewport_offset >= sr_height {
            return None; // Below scroll region
        }

        let total = self.buffer_len();
        let view_end = total.saturating_sub(self.scroll_offset);
        let view_start = view_end.saturating_sub(sr_height);
        let is_full = (view_end - view_start) >= sr_height;

        let draw_start = if self.follow && !self.streaming && is_full {
            view_start + 1
        } else {
            view_start
        };

        let buf_idx = draw_start + viewport_offset;
        if buf_idx < self.buffer_message_id.len() {
            self.buffer_message_id[buf_idx]
        } else {
            None
        }
    }

    // ─── Selection ───────────────────────────────────────────

    /// Select a single message (clearing any prior selection).
    pub fn set_selection_single(&mut self, msg_id: usize) {
        self.selected_messages.clear();
        self.selected_messages.insert(msg_id);
        self.redraw_viewport();
    }

    /// Toggle a message's selection state.
    pub fn toggle_selection(&mut self, msg_id: usize) {
        if !self.selected_messages.remove(&msg_id) {
            self.selected_messages.insert(msg_id);
        }
        self.redraw_viewport();
    }

    /// Select all messages in a range (inclusive), replacing prior selection.
    pub fn select_range(&mut self, from: usize, to: usize) {
        self.selected_messages.clear();
        for id in from..=to {
            self.selected_messages.insert(id);
        }
        self.redraw_viewport();
    }

    /// Clear all selections.
    pub fn clear_selection(&mut self) {
        if !self.selected_messages.is_empty() {
            self.selected_messages.clear();
            self.redraw_viewport();
        }
    }

    /// Whether any messages are selected.
    pub fn has_selection(&self) -> bool {
        !self.selected_messages.is_empty()
    }

    /// Number of selected messages.
    pub fn selection_count(&self) -> usize {
        self.selected_messages.len()
    }

    /// Whether a specific message is selected.
    pub fn is_message_selected(&self, msg_id: usize) -> bool {
        self.selected_messages.contains(&msg_id)
    }

    /// Extract plain text from all selected messages, joined by blank lines.
    pub fn selected_text(&self) -> String {
        if self.selected_messages.is_empty() {
            return String::new();
        }

        let mut messages: Vec<(usize, Vec<String>)> = Vec::new();

        for &msg_id in &self.selected_messages {
            let mut lines: Vec<String> = Vec::new();
            for (i, entry) in self.logical.iter().enumerate() {
                if self.logical_message_id.get(i).copied().flatten() == Some(msg_id) {
                    match entry {
                        LogicalEntry::Styled(line) => {
                            let text: String =
                                line.spans.iter().map(|s| s.text.as_str()).collect();
                            lines.push(text);
                        }
                        LogicalEntry::PlainText(s) => lines.push(s.clone()),
                        LogicalEntry::Markdown(s) => lines.push(s.clone()),
                        LogicalEntry::RawAnsi(raw) => {
                            for l in raw {
                                lines.push(strip_ansi(l));
                            }
                        }
                        LogicalEntry::Blank => lines.push(String::new()),
                    }
                }
            }
            if !lines.is_empty() {
                messages.push((msg_id, lines));
            }
        }

        messages.sort_by_key(|(id, _)| *id);
        messages
            .iter()
            .map(|(_, lines)| lines.join("\n"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // Reset scroll region
        let _ = write!(self.out, "\x1b[r");
        // Clear screen and move cursor home
        let _ = write!(self.out, "\x1b[2J\x1b[H");
        let _ = crossterm::execute!(self.out, PopKeyboardEnhancementFlags);
        let _ = crossterm::execute!(self.out, DisableMouseCapture, Show);
        let _ = terminal::disable_raw_mode();
    }
}

// ─── Word-wrap helpers ────────────────────────────────────────
//
// Both helpers use Unicode scalar value count (`chars().count()`) as a proxy
// for display width. This is correct for ASCII and most Latin/Cyrillic text.
// Double-width CJK characters would require the `unicode-width` crate; that
// can be added as a follow-up if needed.

/// Wrap a styled `Line` into multiple single-row `Line`s, each at most
/// `width` characters wide, splitting at word boundaries where possible.
fn wrap_styled_line(line: &Line, width: usize) -> Vec<Line> {
    if width == 0 {
        return vec![line.clone()];
    }

    // Flatten spans into (char, span_index) pairs.
    let mut flat: Vec<(char, usize)> = Vec::new();
    for (i, span) in line.spans.iter().enumerate() {
        for ch in span.text.chars() {
            flat.push((ch, i));
        }
    }

    if flat.is_empty() {
        return vec![Line::new(vec![])];
    }

    let mut rows: Vec<Line> = Vec::new();
    let mut start = 0;

    while start < flat.len() {
        let max_end = (start + width).min(flat.len());

        if max_end == flat.len() {
            rows.push(assemble_spans(&flat[start..], &line.spans));
            break;
        }

        // Find last space at or before max_end for word-wrap.
        let split_at = flat[start..max_end]
            .iter()
            .rposition(|(ch, _)| *ch == ' ')
            .map(|i| start + i)
            .unwrap_or(max_end); // hard-wrap if no space found

        rows.push(assemble_spans(&flat[start..split_at], &line.spans));

        // Advance past the space (if the split point is one).
        start = if split_at < flat.len() && flat[split_at].0 == ' ' {
            split_at + 1
        } else {
            split_at
        };
        // Skip any additional leading spaces on the new row.
        while start < flat.len() && flat[start].0 == ' ' {
            start += 1;
        }
    }

    if rows.is_empty() {
        rows.push(Line::new(vec![]));
    }

    rows
}

/// Reassemble a flat `(char, span_index)` slice back into a `Line`,
/// grouping consecutive characters from the same span.
fn assemble_spans(flat: &[(char, usize)], spans: &[Span]) -> Line {
    if flat.is_empty() {
        return Line::new(vec![]);
    }

    let mut result: Vec<Span> = Vec::new();
    let mut text = String::new();
    let mut cur_idx = flat[0].1;

    for &(ch, idx) in flat {
        if idx == cur_idx {
            text.push(ch);
        } else {
            result.push(Span::styled(std::mem::take(&mut text), spans[cur_idx].style.clone()));
            cur_idx = idx;
            text.push(ch);
        }
    }
    if !text.is_empty() {
        result.push(Span::styled(text, spans[cur_idx].style.clone()));
    }

    Line::new(result)
}

/// Wrap a plain-text string into chunks of at most `width` characters,
/// splitting at word boundaries where possible.
///
/// Returns at least one element (an empty string for empty input).
fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if width == 0 {
        return vec![text.to_string()];
    }

    let chars: Vec<char> = text.chars().collect();
    let mut rows: Vec<String> = Vec::new();
    let mut start = 0;

    while start < chars.len() {
        let max_end = (start + width).min(chars.len());

        if max_end == chars.len() {
            rows.push(chars[start..].iter().collect());
            break;
        }

        let split_at = chars[start..max_end]
            .iter()
            .rposition(|&c| c == ' ')
            .map(|i| start + i)
            .unwrap_or(max_end);

        rows.push(chars[start..split_at].iter().collect());

        start = if split_at < chars.len() && chars[split_at] == ' ' {
            split_at + 1
        } else {
            split_at
        };
        while start < chars.len() && chars[start] == ' ' {
            start += 1;
        }
    }

    if rows.is_empty() {
        rows.push(String::new());
    }

    rows
}

// ─── Bubble word-wrap ─────────────────────────────────────────

/// A visual line produced by wrapping logical input text to a given width.
struct VisualLine {
    text: String,
    byte_start: usize,
    byte_end: usize,
}

/// Wrap input text into visual lines of at most `width` characters.
///
/// Splits on `\n` first (logical lines), then wraps each logical line at
/// character boundaries when it exceeds `width`. Used only by the bubble.
fn wrap_text(text: &str, width: usize) -> Vec<VisualLine> {
    let width = width.max(1);
    let mut result = Vec::new();
    let mut byte_offset: usize = 0;

    for (li, logical_line) in text.split('\n').enumerate() {
        if li > 0 {
            byte_offset += 1; // skip the `\n`
        }

        if logical_line.is_empty() {
            result.push(VisualLine {
                text: String::new(),
                byte_start: byte_offset,
                byte_end: byte_offset,
            });
        } else {
            let chars: Vec<char> = logical_line.chars().collect();
            let mut char_pos = 0;
            let mut local_byte = 0;

            while char_pos < chars.len() {
                let chunk_end = (char_pos + width).min(chars.len());
                let chunk: String = chars[char_pos..chunk_end].iter().collect();
                let chunk_bytes = chunk.len();

                result.push(VisualLine {
                    text: chunk,
                    byte_start: byte_offset + local_byte,
                    byte_end: byte_offset + local_byte + chunk_bytes,
                });

                local_byte += chunk_bytes;
                char_pos = chunk_end;
            }
        }

        byte_offset += logical_line.len();
    }

    if result.is_empty() {
        result.push(VisualLine { text: String::new(), byte_start: 0, byte_end: 0 });
    }

    result
}

/// Display width of a line including the block cursor (adds 1 char).
fn display_width_with_cursor(text: &str, has_cursor: bool) -> usize {
    let base = text.chars().count();
    if has_cursor { base + 1 } else { base }
}

// ─── Selection rendering ──────────────────────────────────────

/// Write a line's spans with a forced background color for selection highlighting.
///
/// Each span gets the highlight `bg` unless it already has its own background.
/// After each span's reset, the highlight bg is restored so remaining space
/// on the row stays highlighted.
fn write_line_content_highlighted(w: &mut impl Write, line: &Line, bg: Color) -> io::Result<()> {
    for span in &line.spans {
        write!(w, "{}", SetBackgroundColor(span.style.bg.unwrap_or(bg)))?;
        if let Some(fg) = span.style.fg {
            write!(w, "{}", SetForegroundColor(fg))?;
        }
        if span.style.bold {
            write!(w, "{}", SetAttribute(Attribute::Bold))?;
        }
        if span.style.dim {
            write!(w, "{}", SetAttribute(Attribute::Dim))?;
        }
        if span.style.italic {
            write!(w, "{}", SetAttribute(Attribute::Italic))?;
        }
        if span.style.underline {
            write!(w, "{}", SetAttribute(Attribute::Underlined))?;
        }
        if span.style.negative {
            write!(w, "{}", SetAttribute(Attribute::Reverse))?;
        }
        write!(w, "{}", span.text)?;
        write!(w, "{}{}", SetAttribute(Attribute::Reset), ResetColor)?;
        // Restore highlight bg for next span and trailing space.
        write!(w, "{}", SetBackgroundColor(bg))?;
    }
    Ok(())
}

/// Strip ANSI escape sequences from a string.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip CSI sequence: ESC [ ... (letter)
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

// ─── StreamBuffer ─────────────────────────────────────────────

/// Accumulates streaming text and helps flush it to a `Canvas`.
pub struct StreamBuffer {
    pending: String,
    /// Full accumulated text for the current stream (for markdown rendering).
    full_text: String,
    active: bool,
}

impl Default for StreamBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamBuffer {
    pub fn new() -> Self {
        Self { pending: String::new(), full_text: String::new(), active: false }
    }

    /// Append text from a streaming source.
    pub fn push(&mut self, text: &str) {
        self.active = true;
        self.pending.push_str(text);
        self.full_text.push_str(text);
    }

    /// Flush accumulated text to the canvas.
    pub fn flush(&mut self, canvas: &mut Canvas) {
        if !self.pending.is_empty() {
            canvas.print_streaming(&self.pending);
            self.pending.clear();
        }
    }

    /// End the streaming block (plain text, no markdown rendering).
    pub fn finish(&mut self, canvas: &mut Canvas) {
        self.flush(canvas);
        if self.active {
            canvas.end_streaming();
            self.active = false;
        }
        self.full_text.clear();
    }

    /// End the streaming block and replace raw output with rendered markdown.
    ///
    /// Uses `content_width()` so termimad wraps to the usable content area
    /// (excluding the scrollbar column). The markdown source is retained in
    /// the logical buffer so it can be re-rendered at the new width on resize.
    #[cfg(feature = "markdown")]
    pub fn finish_markdown(&mut self, canvas: &mut Canvas) {
        self.flush(canvas);
        if self.active {
            canvas.end_streaming();
            self.active = false;
        }
        if !self.full_text.is_empty() {
            let source = std::mem::take(&mut self.full_text);
            let rendered =
                crate::markdown::render_markdown_lines(&source, canvas.content_width());
            canvas.replace_stream_block_markdown(source, rendered);
        } else {
            self.full_text.clear();
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
}
