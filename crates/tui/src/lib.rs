pub mod canvas;
pub mod input;
pub mod style;

#[cfg(feature = "markdown")]
pub mod markdown;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent, KeyEventKind, MouseEventKind};

// Re-export so consumers don't need crossterm for key handling.
pub use crossterm::event::{KeyCode, KeyModifiers};
pub use crossterm::style::Color;

/// Terminal event — key press, resize, or scroll.
pub enum TermEvent {
    Key(KeyEvent),
    Resize(u16, u16),
    ScrollUp(u16),
    ScrollDown(u16),
}

/// Poll for a terminal event (key press, resize, or mouse scroll).
///
/// Returns `None` if the timeout expires or the event is not relevant.
/// Key events are filtered to `Press` kind only (ignoring Release/Repeat).
/// Mouse scroll events are converted to `ScrollUp`/`ScrollDown` (3 lines per tick).
pub fn poll_event(timeout: Duration) -> io::Result<Option<TermEvent>> {
    if event::poll(timeout)? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                return Ok(Some(TermEvent::Key(key)));
            }
            Event::Resize(w, h) => {
                return Ok(Some(TermEvent::Resize(w, h)));
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    return Ok(Some(TermEvent::ScrollUp(3)));
                }
                MouseEventKind::ScrollDown => {
                    return Ok(Some(TermEvent::ScrollDown(3)));
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(None)
}

/// Poll for a key press event.
///
/// Returns `None` if the timeout expires or the event is not a key press.
/// Filters to `Press` kind only (ignoring Release/Repeat).
pub fn poll_key(timeout: Duration) -> io::Result<Option<KeyEvent>> {
    if event::poll(timeout)?
        && let Event::Key(key) = event::read()?
        && key.kind == KeyEventKind::Press
    {
        return Ok(Some(key));
    }
    Ok(None)
}
