use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};

/// What happened after a key press.
pub enum InputAction {
    /// No visible change.
    None,
    /// User pressed Enter — submitted this text.
    Submit(String),
    /// User wants to quit (Ctrl+C confirmed).
    Quit,
    /// First Ctrl+C with empty input — caller should show "press again to exit".
    ConfirmExit,
    /// Input content or cursor changed — caller should redraw.
    Changed,
}

/// Autocomplete state.
pub struct Autocomplete {
    pub at_pos: usize,
    pub query: String,
    pub matches: Vec<String>,
    pub selected: usize,
}

/// Autocomplete resolver function type.
pub type ResolveFn<'a> = &'a dyn Fn(&str) -> Vec<String>;

/// Multi-line text input with cursor movement and optional autocomplete.
///
/// The input is always responsive — there is no "locked" state.
/// The application decides what to do when the user submits while
/// something is in-flight.
///
/// Newlines are stored as `\n` in the flat string buffer. The byte-position
/// cursor works unchanged — `\n` is a single byte.
pub struct InputLine {
    pub text: String,
    pub cursor: usize,
    pub autocomplete: Option<Autocomplete>,
    trigger: Option<char>,
    ctrl_c_time: Option<Instant>,
}

impl Default for InputLine {
    fn default() -> Self {
        Self::new()
    }
}

impl InputLine {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            autocomplete: None,
            trigger: None,
            ctrl_c_time: None,
        }
    }

    /// Set the character that triggers autocomplete (e.g., '@').
    /// Pass `None` to disable autocomplete triggering.
    pub fn set_autocomplete_trigger(&mut self, trigger: Option<char>) {
        self.trigger = trigger;
    }

    // ─── Line helpers ─────────────────────────────────────────

    /// Iterator over logical lines (split on `\n`).
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.split('\n')
    }

    /// Number of logical lines.
    pub fn line_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    /// Which line and column (both 0-based) the cursor is on.
    /// Column is in bytes from the start of the line.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let line = before.matches('\n').count();
        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let col = self.cursor - line_start;
        (line, col)
    }

    // ─── Key handling ─────────────────────────────────────────

    /// Process a key event. Returns what happened.
    ///
    /// `resolve` is called when autocomplete needs matches for a query.
    /// Pass `None` to disable autocomplete entirely.
    pub fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        resolve: Option<ResolveFn<'_>>,
    ) -> InputAction {
        // Ctrl+C: clear text, or confirm exit if already empty
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            if !self.text.is_empty() {
                self.text.clear();
                self.cursor = 0;
                self.ctrl_c_time = None;
                return InputAction::Changed;
            }
            // Empty input: check for double-tap
            if self
                .ctrl_c_time
                .is_some_and(|t| t.elapsed().as_millis() < 1500)
            {
                return InputAction::Quit;
            }
            self.ctrl_c_time = Some(Instant::now());
            return InputAction::ConfirmExit;
        }
        // Any other key clears the Ctrl+C pending state
        self.ctrl_c_time = None;

        // Shift+Enter or Alt+Enter inserts a newline
        if code == KeyCode::Enter
            && (modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::ALT))
        {
            self.text.insert(self.cursor, '\n');
            self.cursor += 1;
            return InputAction::Changed;
        }

        // If autocomplete is active, intercept navigation keys
        if self.autocomplete.is_some() {
            match code {
                KeyCode::Up => {
                    self.autocomplete_up();
                    return InputAction::Changed;
                }
                KeyCode::Down => {
                    self.autocomplete_down();
                    return InputAction::Changed;
                }
                KeyCode::Enter => {
                    self.accept_autocomplete();
                    return InputAction::Changed;
                }
                KeyCode::Esc => {
                    self.cancel_autocomplete();
                    return InputAction::Changed;
                }
                KeyCode::Char(c) => {
                    if c == ' ' {
                        self.cancel_autocomplete();
                        self.insert_char(c);
                    } else {
                        self.insert_char(c);
                        self.update_autocomplete(resolve);
                    }
                    return InputAction::Changed;
                }
                KeyCode::Backspace => {
                    if self.cursor > 0 {
                        let at_pos = self.autocomplete.as_ref().map(|ac| ac.at_pos);
                        let prev = prev_char_boundary(&self.text, self.cursor);
                        self.text.remove(prev);
                        self.cursor = prev;
                        if at_pos.is_some_and(|p| self.cursor <= p) {
                            self.cancel_autocomplete();
                        } else {
                            self.update_autocomplete(resolve);
                        }
                    }
                    return InputAction::Changed;
                }
                _ => {
                    self.cancel_autocomplete();
                }
            }
        }

        match code {
            KeyCode::Enter => {
                if self.text.is_empty() {
                    return InputAction::None;
                }
                let text: String = self.text.drain(..).collect();
                self.cursor = 0;
                InputAction::Submit(text)
            }
            KeyCode::Char(c) if self.trigger == Some(c) => {
                self.insert_char(c);
                self.start_autocomplete(resolve);
                InputAction::Changed
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                InputAction::Changed
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.text, self.cursor);
                    self.text.remove(prev);
                    self.cursor = prev;
                }
                InputAction::Changed
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.text, self.cursor);
                }
                InputAction::Changed
            }
            KeyCode::Right => {
                if self.cursor < self.text.len() {
                    self.cursor = next_char_boundary(&self.text, self.cursor);
                }
                InputAction::Changed
            }
            KeyCode::Up => {
                self.move_cursor_up();
                InputAction::Changed
            }
            KeyCode::Down => {
                self.move_cursor_down();
                InputAction::Changed
            }
            KeyCode::Home => {
                self.cursor = self.current_line_start();
                InputAction::Changed
            }
            KeyCode::End => {
                self.cursor = self.current_line_end();
                InputAction::Changed
            }
            KeyCode::Esc => {
                self.text.clear();
                self.cursor = 0;
                InputAction::Changed
            }
            _ => InputAction::None,
        }
    }

    fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    // ─── Vertical cursor movement ────────────────────────────

    /// Byte offset of the start of the line the cursor is on.
    fn current_line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0)
    }

    /// Byte offset of the end of the line the cursor is on (before \n or at text end).
    fn current_line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|p| self.cursor + p)
            .unwrap_or(self.text.len())
    }

    fn move_cursor_up(&mut self) {
        let line_start = self.current_line_start();
        if line_start == 0 {
            // Already on first line — move to start
            self.cursor = 0;
            return;
        }
        let col = self.cursor - line_start;
        // Previous line ends at line_start - 1 (the \n)
        let prev_line_end = line_start - 1;
        let prev_line_start = self.text[..prev_line_end]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let prev_line_len = prev_line_end - prev_line_start;
        self.cursor = prev_line_start + col.min(prev_line_len);
    }

    fn move_cursor_down(&mut self) {
        let line_end = self.current_line_end();
        if line_end == self.text.len() {
            // Already on last line — move to end
            self.cursor = self.text.len();
            return;
        }
        let line_start = self.current_line_start();
        let col = self.cursor - line_start;
        // Next line starts at line_end + 1 (after \n)
        let next_line_start = line_end + 1;
        let next_line_end = self.text[next_line_start..]
            .find('\n')
            .map(|p| next_line_start + p)
            .unwrap_or(self.text.len());
        let next_line_len = next_line_end - next_line_start;
        self.cursor = next_line_start + col.min(next_line_len);
    }

    // ─── Autocomplete ───────────────────────────────────────

    fn start_autocomplete(&mut self, resolve: Option<ResolveFn<'_>>) {
        let Some(resolve) = resolve else { return };
        let at_pos = self.text[..self.cursor]
            .rfind(self.trigger.unwrap_or('@'))
            .unwrap_or(self.cursor.saturating_sub(1));
        let query = self.text[at_pos + 1..self.cursor].to_string();
        let matches = resolve(&query);
        self.autocomplete = Some(Autocomplete {
            at_pos,
            query,
            matches,
            selected: 0,
        });
    }

    fn update_autocomplete(&mut self, resolve: Option<ResolveFn<'_>>) {
        let Some(resolve) = resolve else { return };
        if let Some(ac) = &mut self.autocomplete {
            ac.query = self.text[ac.at_pos + 1..self.cursor].to_string();
            ac.matches = resolve(&ac.query);
            ac.selected = ac.selected.min(ac.matches.len().saturating_sub(1));
        }
    }

    fn accept_autocomplete(&mut self) {
        let Some(ac) = self.autocomplete.take() else {
            return;
        };
        if ac.matches.is_empty() {
            return;
        }
        let chosen = &ac.matches[ac.selected];
        let trigger = self.trigger.unwrap_or('@');
        let before = &self.text[..ac.at_pos];
        let after = &self.text[self.cursor..];
        let new_input = format!("{before}{trigger}{chosen} {after}");
        let new_cursor = ac.at_pos + 1 + chosen.len() + 1;
        self.text = new_input;
        self.cursor = new_cursor;
    }

    fn cancel_autocomplete(&mut self) {
        self.autocomplete = None;
    }

    fn autocomplete_up(&mut self) {
        if let Some(ac) = &mut self.autocomplete {
            ac.selected = ac.selected.saturating_sub(1);
        }
    }

    fn autocomplete_down(&mut self) {
        if let Some(ac) = &mut self.autocomplete
            && !ac.matches.is_empty()
        {
            ac.selected = (ac.selected + 1).min(ac.matches.len() - 1);
        }
    }
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.saturating_sub(1);
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}
