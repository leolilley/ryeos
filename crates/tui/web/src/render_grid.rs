//! Grid renderer — stubbed for splash screen.
//!
//! The splash screen has no tiles/grid, only Canvas primitives.
//! This module will be repopulated when tiles return.

use ryeos_tui_core::frame::Frame;

/// Render frame to HTML (empty for splash-only mode).
pub fn render_frame_html(_frame: &Frame) -> String {
    String::new()
}
