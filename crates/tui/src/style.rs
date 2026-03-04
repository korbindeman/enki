use std::io::{self, Write};

use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};

/// A text style applied to a `Span`.
#[derive(Clone, Debug, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub negative: bool,
}

impl Style {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub fn negative(mut self) -> Self {
        self.negative = true;
        self
    }

    fn is_empty(&self) -> bool {
        self.fg.is_none()
            && self.bg.is_none()
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && !self.negative
    }
}

/// A segment of styled text.
#[derive(Clone)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// A complete line of styled spans.
#[derive(Clone)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn new(spans: Vec<Span>) -> Self {
        Self { spans }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            spans: vec![Span::plain(text)],
        }
    }
}

/// Write a span's styled text to the given writer.
pub(crate) fn write_span(w: &mut impl Write, span: &Span) -> io::Result<()> {
    if span.style.is_empty() {
        write!(w, "{}", span.text)?;
        return Ok(());
    }

    if let Some(fg) = span.style.fg {
        write!(w, "{}", SetForegroundColor(fg))?;
    }
    if let Some(bg) = span.style.bg {
        write!(w, "{}", SetBackgroundColor(bg))?;
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

    Ok(())
}

/// Truncate a string to at most `max` characters, appending "…" if truncated.
pub fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

/// Write a full line (all spans) followed by \r\n.
pub(crate) fn write_line(w: &mut impl Write, line: &Line) -> io::Result<()> {
    for span in &line.spans {
        write_span(w, span)?;
    }
    write!(w, "\r\n")?;
    Ok(())
}

/// Write a line's spans WITHOUT a trailing newline. Used for viewport rendering
/// where cursor positioning is handled externally.
pub(crate) fn write_line_content(w: &mut impl Write, line: &Line) -> io::Result<()> {
    for span in &line.spans {
        write_span(w, span)?;
    }
    Ok(())
}
