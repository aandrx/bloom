//! Bloom TUI library: state, daemon session handling, and rendering.

pub mod app;
pub mod daemon;
pub mod ui;

/// Re-exported so callers and integration tests can build key events without
/// depending on the terminal backend directly.
pub use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
