//! Desktop notifications via terminal escape sequences.
//!
//! Auto-detects the terminal from `TERM_PROGRAM` and sends the
//! appropriate sequence. No-ops silently on unsupported terminals.
//!
//! Supported terminals:
//! - OSC 9:  Ghostty, iTerm2, WezTerm
//! - OSC 99: kitty
//! - OSC 777: foot
//!
//! When running inside tmux, the inner sequence is wrapped in a DCS
//! passthrough (`\x1bPtmux;...\x1b\\`). Requires `allow-passthrough`
//! in the user's tmux config.

use std::io::{self, Write};

/// Send a desktop notification if the terminal supports it.
///
/// Detects the terminal from `TERM_PROGRAM` and sends the right escape
/// sequence. No-ops on unsupported terminals (Alacritty, Apple Terminal, etc.).
pub fn notify(message: &str) {
    let seq = match detect_protocol() {
        Some(Protocol::Osc9) => format!("\x1b]9;{message}\x1b\\"),
        Some(Protocol::Osc99) => format!("\x1b]99;;{message}\x1b\\"),
        Some(Protocol::Osc777) => format!("\x1b]777;notify;enki;{message}\x1b\\"),
        None => return,
    };

    let output = if in_tmux() { tmux_passthrough(&seq) } else { seq };

    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(output.as_bytes());
    let _ = stdout.flush();
}

enum Protocol {
    Osc9,
    Osc99,
    Osc777,
}

fn detect_protocol() -> Option<Protocol> {
    let term = std::env::var("TERM_PROGRAM").ok()?;
    match term.as_str() {
        "ghostty" | "iTerm.app" | "WezTerm" => Some(Protocol::Osc9),
        "kitty" => Some(Protocol::Osc99),
        "foot" => Some(Protocol::Osc777),
        _ => None,
    }
}

fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Wrap an escape sequence in tmux DCS passthrough.
///
/// Doubles inner ESC bytes and wraps in `\x1bPtmux;...\x1b\\`.
fn tmux_passthrough(seq: &str) -> String {
    let inner = seq.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{inner}\x1b\\")
}
