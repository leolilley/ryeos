//! Gruvbox Dark theme — semantic tokens for the TUI.
//!
//! All colors map to states (alive/cold, running/completed, signed/unsigned).
//! Dim text uses semantic colors, not ANSI dim attribute.

use crate::text_surface::{Color, Style};

// ---------------------------------------------------------------------------
// Raw palette
// ---------------------------------------------------------------------------

pub const BG_DARK: Color = Color::Rgb(0x1d, 0x20, 0x21);
pub const BG: Color = Color::Rgb(0x28, 0x28, 0x28);
pub const BG_LIGHT: Color = Color::Rgb(0x3c, 0x38, 0x36);
pub const BG_LIGHTER: Color = Color::Rgb(0x50, 0x49, 0x45);

pub const FG: Color = Color::Rgb(0xeb, 0xdb, 0xb2);
pub const FG_DIM: Color = Color::Rgb(0xa8, 0x99, 0x84);
pub const FG_MUTED: Color = Color::Rgb(0x66, 0x5c, 0x54);
pub const FG_SUBTLE: Color = Color::Rgb(0x50, 0x49, 0x45);

pub const ACCENT: Color = Color::Rgb(0x83, 0xa5, 0x98);
pub const ACCENT_BRIGHT: Color = Color::Rgb(0x8e, 0xc0, 0x7c);
pub const RED: Color = Color::Rgb(0xfb, 0x49, 0x34);
pub const RED_DIM: Color = Color::Rgb(0xcc, 0x24, 0x1d);
pub const YELLOW: Color = Color::Rgb(0xfa, 0xbd, 0x2f);
pub const YELLOW_DIM: Color = Color::Rgb(0xd7, 0x99, 0x21);
pub const GREEN: Color = Color::Rgb(0xb8, 0xbb, 0x26);
pub const GREEN_DIM: Color = Color::Rgb(0x98, 0x97, 0x1a);
pub const BLUE: Color = Color::Rgb(0x83, 0xa5, 0x98);
pub const BLUE_DIM: Color = Color::Rgb(0x45, 0x85, 0x88);
pub const PURPLE: Color = Color::Rgb(0xd3, 0x86, 0x9b);
pub const AQUA: Color = Color::Rgb(0x8e, 0xc0, 0x7c);
pub const ORANGE: Color = Color::Rgb(0xfe, 0x80, 0x19);

// ---------------------------------------------------------------------------
// Semantic tokens
// ---------------------------------------------------------------------------

pub const BORDER: Color = FG_SUBTLE;
pub const BORDER_ACTIVE: Color = ACCENT;
pub const BORDER_SELECTED: Color = YELLOW;
pub const CURSOR: Color = ACCENT;
pub const CURSOR_TEXT: Color = BG_DARK;

pub const STATUS_OK: Color = GREEN;
pub const STATUS_ERR: Color = RED;
pub const STATUS_BUSY: Color = YELLOW;
pub const STATUS_IDLE: Color = FG_MUTED;

pub const TRUST_OK: Color = GREEN;
pub const TRUST_WARN: Color = YELLOW;
pub const TRUST_ERR: Color = RED;

pub const BUDGET_LOW: Color = FG;
pub const BUDGET_HIGH: Color = ORANGE;
pub const BUDGET_OVER: Color = RED;

// ---------------------------------------------------------------------------
// Model colors
// ---------------------------------------------------------------------------

pub const MODEL_OPUS: Color = PURPLE;
pub const MODEL_SONNET: Color = BLUE;
pub const MODEL_HAIKU: Color = GREEN;
pub const MODEL_DEFAULT: Color = FG_DIM;

// ---------------------------------------------------------------------------
// Convenience style constructors
// ---------------------------------------------------------------------------

/// Default text style on the primary background.
pub fn style_fg() -> Style {
    Style::new().fg(FG).bg(BG)
}

/// Dim/secondary text.
pub fn style_dim() -> Style {
    Style::new().fg(FG_DIM).bg(BG)
}

/// Muted/disabled text.
pub fn style_muted() -> Style {
    Style::new().fg(FG_MUTED).bg(BG)
}

/// Panel border (unfocused).
pub fn style_border() -> Style {
    Style::new().fg(BORDER).bg(BG)
}

/// Active/focused panel border.
pub fn style_border_active() -> Style {
    Style::new().fg(BORDER_ACTIVE).bg(BG)
}

/// Status bar background style.
pub fn style_status_bar() -> Style {
    Style::new().fg(FG_DIM).bg(BG_DARK)
}

/// Input bar background style.
pub fn style_input_bar() -> Style {
    Style::new().fg(FG).bg(BG_DARK)
}

/// Error text.
pub fn style_error() -> Style {
    Style::new().fg(RED).bg(BG)
}

/// Success/completed text.
pub fn style_ok() -> Style {
    Style::new().fg(GREEN).bg(BG)
}

/// Warning/busy text.
pub fn style_busy() -> Style {
    Style::new().fg(YELLOW).bg(BG)
}

/// Cursor/selection highlight.
pub fn style_cursor() -> Style {
    Style::new().fg(CURSOR_TEXT).bg(CURSOR)
}

// ---------------------------------------------------------------------------
// Content width cap
// ---------------------------------------------------------------------------

/// Maximum content width in columns for readability.
pub const CONTENT_MAX_WIDTH: usize = 160;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_has_required_semantic_tokens() {
        // Verify semantic tokens are distinct from each other
        assert_ne!(STATUS_OK, STATUS_ERR);
        assert_ne!(STATUS_OK, STATUS_BUSY);
        assert_ne!(BORDER, BORDER_ACTIVE);
        assert_ne!(BUDGET_LOW, BUDGET_HIGH);
        assert_ne!(BUDGET_HIGH, BUDGET_OVER);
        assert_ne!(TRUST_OK, TRUST_WARN);
        assert_ne!(TRUST_WARN, TRUST_ERR);
    }

    #[test]
    fn style_constructors_use_correct_colors() {
        assert_eq!(style_fg().fg, FG);
        assert_eq!(style_dim().fg, FG_DIM);
        assert_eq!(style_muted().fg, FG_MUTED);
        assert_eq!(style_error().fg, RED);
        assert_eq!(style_ok().fg, GREEN);
        assert_eq!(style_busy().fg, YELLOW);
    }
}
