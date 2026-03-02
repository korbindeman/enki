use termimad::crossterm::style::Color;
use termimad::Area;

/// Render markdown text into a list of ANSI-styled lines.
///
/// Each returned string is a single rendered line containing ANSI escape
/// sequences for styling (bold, color, etc.).
pub fn render_markdown_lines(text: &str, width: u16) -> Vec<String> {
    let skin = make_skin();
    let area = Area {
        left: 0,
        top: 0,
        width,
        height: u16::MAX,
    };
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
