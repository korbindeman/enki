use std::sync::LazyLock;

use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use termimad::crossterm::style::Color;
use termimad::Area;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Render markdown text into a list of ANSI-styled lines.
///
/// Fenced code blocks are highlighted with syntect. Everything else is
/// rendered through termimad.
pub fn render_markdown_lines(text: &str, width: u16) -> Vec<String> {
    let segments = split_at_code_fences(text);
    let mut lines = Vec::new();
    for seg in segments {
        match seg {
            Segment::Prose(prose) => lines.extend(render_prose(prose, width)),
            Segment::Code { lang, body } => lines.extend(render_code(lang, body, width)),
        }
    }
    lines
}

// ─── Segment parsing ─────────────────────────────────────────

enum Segment<'a> {
    Prose(&'a str),
    Code { lang: &'a str, body: &'a str },
}

/// Split markdown text into alternating prose and fenced-code segments.
fn split_at_code_fences(text: &str) -> Vec<Segment<'_>> {
    let mut segments: Vec<Segment<'_>> = Vec::new();
    let mut in_code = false;
    let mut fence_lang = "";
    let mut code_start = 0;
    let mut prose_start = 0;

    let mut pos = 0;
    for line in text.lines() {
        let line_end = pos + line.len();
        // Advance past the newline (if present).
        let next = if line_end < text.len() { line_end + 1 } else { line_end };

        let trimmed = line.trim_start();
        if !in_code && trimmed.starts_with("```") {
            // Opening fence — flush preceding prose.
            if pos > prose_start {
                let prose = &text[prose_start..pos];
                if !prose.trim().is_empty() {
                    segments.push(Segment::Prose(prose));
                }
            }
            fence_lang = trimmed[3..].trim();
            code_start = next;
            in_code = true;
        } else if in_code && trimmed.starts_with("```") {
            // Closing fence.
            let body = &text[code_start..pos];
            segments.push(Segment::Code { lang: fence_lang, body });
            in_code = false;
            prose_start = next;
        }

        pos = next;
    }

    // Trailing content: unclosed fence → treat as prose, otherwise flush prose.
    if in_code {
        // Unclosed code fence — treat the whole thing from the opening ``` as prose.
        let remaining = &text[prose_start.min(code_start.saturating_sub(fence_lang.len() + 4))..];
        if !remaining.trim().is_empty() {
            segments.push(Segment::Prose(remaining));
        }
    } else if prose_start < text.len() {
        let remaining = &text[prose_start..];
        if !remaining.trim().is_empty() {
            segments.push(Segment::Prose(remaining));
        }
    }

    segments
}

// ─── Prose rendering (termimad) ──────────────────────────────

fn render_prose(text: &str, width: u16) -> Vec<String> {
    let skin = make_skin();
    let area = Area { left: 0, top: 0, width, height: u16::MAX };
    let formatted = skin.area_text(text, &area);
    let rendered = format!("{formatted}");
    rendered.lines().map(String::from).collect()
}

fn make_skin() -> termimad::MadSkin {
    let mut skin = termimad::MadSkin::default();
    skin.bold.set_fg(Color::White);
    skin.italic.set_fg(Color::Magenta);
    skin.inline_code.set_fg(Color::Green);
    skin.code_block.set_fg(Color::Green);
    skin
}

// ─── Code rendering (syntect) ────────────────────────────────

fn render_code(lang: &str, body: &str, width: u16) -> Vec<String> {
    let ss = &*SYNTAX_SET;
    let theme = &THEME_SET.themes["base16-ocean.dark"];

    let syntax = if lang.is_empty() {
        ss.find_syntax_plain_text()
    } else {
        ss.find_syntax_by_token(lang).unwrap_or_else(|| ss.find_syntax_plain_text())
    };

    let mut h = syntect::easy::HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for line in syntect::util::LinesWithEndings::from(body) {
        let ranges = h.highlight_line(line, ss).unwrap_or_default();
        let escaped = syntect::util::as_24_bit_terminal_escaped(&ranges[..], false);
        // Strip the trailing newline that syntect preserves, then hard-wrap.
        let escaped = escaped.trim_end_matches('\n').to_string();
        // Hard-wrap long lines to fit within the content area.
        for wrapped in hard_wrap_ansi(&escaped, width as usize) {
            lines.push(wrapped);
        }
    }
    // Reset styling after the code block.
    if let Some(last) = lines.last_mut() {
        last.push_str("\x1b[0m");
    }
    lines
}

/// Hard-wrap an ANSI-escaped string at `max_cols` visible characters.
///
/// Walks the string tracking visible character count vs. escape sequences,
/// splitting into lines that are at most `max_cols` printable characters wide.
fn hard_wrap_ansi(s: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![s.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut visible = 0;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Consume the entire escape sequence.
            current.push(ch);
            for inner in chars.by_ref() {
                current.push(inner);
                if inner.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            if visible >= max_cols {
                lines.push(current);
                current = String::new();
                visible = 0;
            }
            current.push(ch);
            visible += 1;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_prose_only() {
        let segments = split_at_code_fences("hello world");
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], Segment::Prose("hello world")));
    }

    #[test]
    fn split_code_block() {
        let input = "before\n```rust\nfn main() {}\n```\nafter";
        let segments = split_at_code_fences(input);
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], Segment::Prose(p) if p.trim() == "before"));
        assert!(matches!(segments[1], Segment::Code { lang: "rust", body } if body.trim() == "fn main() {}"));
        assert!(matches!(segments[2], Segment::Prose(p) if p.trim() == "after"));
    }

    #[test]
    fn hard_wrap_plain() {
        let lines = hard_wrap_ansi("abcdef", 3);
        assert_eq!(lines, vec!["abc", "def"]);
    }

    #[test]
    fn hard_wrap_preserves_escapes() {
        let s = "\x1b[31mabc\x1b[0m";
        let lines = hard_wrap_ansi(s, 10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], s);
    }

    #[test]
    fn render_highlights_rust() {
        let lines = render_code("rust", "let x = 42;\n", 80);
        assert!(!lines.is_empty());
        // Should contain ANSI escape sequences.
        assert!(lines[0].contains('\x1b'));
    }
}
