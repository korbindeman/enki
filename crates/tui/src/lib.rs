pub mod canvas;
pub mod chat;
pub mod indicator;
pub mod input;
pub mod lines;
pub mod notify;
pub mod style;
pub mod workers;

#[cfg(feature = "markdown")]
pub mod markdown;

pub use chat::{ImageData, UserInput};

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent, KeyEventKind, MouseButton, MouseEventKind};

// Re-export so consumers don't need crossterm for key handling.
pub use crossterm::event::{KeyCode, KeyModifiers};
pub use crossterm::style::Color;

/// Terminal event — key press, resize, scroll, or mouse click/drag.
pub enum TermEvent {
    Key(KeyEvent),
    Resize(u16, u16),
    ScrollUp(u16),
    ScrollDown(u16),
    MouseDown { row: u16, col: u16, modifiers: KeyModifiers },
    MouseDrag { row: u16, col: u16 },
    MouseUp { row: u16, col: u16 },
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
                MouseEventKind::Down(MouseButton::Left) => {
                    return Ok(Some(TermEvent::MouseDown {
                        row: mouse.row,
                        col: mouse.column,
                        modifiers: mouse.modifiers,
                    }));
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    return Ok(Some(TermEvent::MouseDrag {
                        row: mouse.row,
                        col: mouse.column,
                    }));
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    return Ok(Some(TermEvent::MouseUp {
                        row: mouse.row,
                        col: mouse.column,
                    }));
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(None)
}

