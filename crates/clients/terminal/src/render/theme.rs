//! The terminal palette: the terminal's own. Default foreground and
//! background, and the named ANSI colours for tones, so the client follows
//! however the operator's terminal is themed. This is the ONLY color authority
//! in the renderer — tone semantics come from the VM; draw sites take styles
//! from here and never invent colors.

use ryeos_client_base::text_surface::{Border, Color, Style};
use ryeos_client_base::ui::view_model::RyeOsTone;

pub const BG: Color = Color::Default;
/// The highlighted-row ground: bright black.
pub const PANEL: Color = Color::Index(8);
pub const FG: Color = Color::Default;
pub const FG_SOFT: Color = Color::Default;
/// Dim ink: bright black.
pub const MUTED: Color = Color::Index(8);
/// Tone `accent`: bright yellow, the nearest named colour to an accent.
pub const ACCENT: Color = Color::Index(11);
/// Tone `warn`: yellow.
pub const WARN: Color = Color::Index(3);
/// Tone `good`: green.
pub const GOOD: Color = Color::Index(2);
/// Tone `danger`: red.
pub const DANGER: Color = Color::Index(1);

// Content renders on the page background (BG), separated by borders —
// consistent across the input box, and tiles. There
// is no distinct PANEL fill for content; PANEL stays only for overlays
// overlay, which deliberately stands out against the dimmed scrim.
pub fn tone_style(tone: RyeOsTone) -> Style {
    match tone {
        RyeOsTone::Good => Style::new().fg(GOOD).bg(BG),
        RyeOsTone::Warn => Style::new().fg(WARN).bg(BG),
        RyeOsTone::Danger => Style::new().fg(DANGER).bg(BG),
        RyeOsTone::Accent => Style::new().fg(ACCENT).bg(BG),
        RyeOsTone::Neutral => style_fg(),
    }
}

pub fn tone_glyph(tone: RyeOsTone) -> &'static str {
    match tone {
        RyeOsTone::Good => "✓",
        RyeOsTone::Warn => "!",
        RyeOsTone::Danger => "✗",
        RyeOsTone::Accent => "›",
        RyeOsTone::Neutral => "•",
    }
}

pub fn style_fg() -> Style {
    Style::new().fg(FG_SOFT).bg(BG)
}

pub fn style_muted() -> Style {
    Style::new().fg(MUTED).bg(BG)
}

pub fn style_selected() -> Style {
    // Black on the accent reads on any terminal theme.
    Style::new().fg(Color::Index(0)).bg(ACCENT)
}

/// Blend `from` toward `to` by `t` (0 = untouched, 1 = fully `to`). Theme
/// constants in, theme blends out — non-RGB colours pass through.
pub fn mix_toward(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(r, g, b), Color::Rgb(tr, tg, tb)) => {
            let mix = |a: u8, b: u8| ((a as f32) * (1.0 - t) + (b as f32) * t).round() as u8;
            Color::Rgb(mix(r, tr), mix(g, tg), mix(b, tb))
        }
        _ => from,
    }
}

/// A row's foreground eases toward ACCENT for a moment after it changed,
/// fading out over 1.2s — shared by every widget that draws `RyeOsRowVm`-like
/// rows so a changed row animates identically no matter which widget hosts it.
pub(crate) fn shimmer_style(
    style: Style,
    changed_at_ms: Option<u64>,
    flash: Color,
    now_ms: u64,
) -> Style {
    let Some(changed_at_ms) = changed_at_ms else {
        return style;
    };
    let age = now_ms.saturating_sub(changed_at_ms);
    if age >= 1_200 {
        return style;
    }
    let weight = 0.45 * (1.0 - age as f32 / 1_200.0);
    style.fg(mix_toward(style.fg, flash, weight))
}

/// Motion is a vocabulary, one treatment per state:
/// - ACCENT (running) breathes DEEP and quick — work is happening now;
/// - WARN (suspended parents, waiting states) swells slow and shallow —
///   alive but parked, unmistakably not the same motion as running;
/// - the shimmer above is a third voice: a one-shot flash for "this row
///   just changed", whatever its state;
/// - terminal states (good/danger/neutral) hold still.
pub(crate) fn active_pulse_style(style: Style, tone: RyeOsTone, now_ms: u64) -> Style {
    match tone {
        RyeOsTone::Accent => {
            let phase = (now_ms / 180) % 8;
            let wave = match phase {
                0 | 7 => 0.12,
                1 | 6 => 0.22,
                2 | 5 => 0.32,
                _ => 0.40,
            };
            style.fg(mix_toward(style.fg, ACCENT, wave))
        }
        RyeOsTone::Warn => {
            let phase = (now_ms / 320) % 12;
            let wave = match phase {
                0 | 11 => 0.04,
                1..=3 | 8..=10 => 0.08,
                _ => 0.14,
            };
            style.fg(mix_toward(style.fg, WARN, wave))
        }
        _ => style,
    }
}

/// The single authority mapping the VM-declared border name to a
/// drawable. `None` means draw no border cells at all; `hidden` keeps
/// the border cells but draws them blank (layout stable); unknown or
/// empty names degrade to thin.
pub fn border_for(name: &str) -> Option<Border> {
    match name {
        "thick" => Some(Border::Thick),
        "hidden" => Some(Border::None),
        "none" => None,
        _ => Some(Border::Sharp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_takes_the_terminals_named_colours() {
        let fg = |tone| tone_style(tone).fg;
        assert_eq!(fg(RyeOsTone::Accent), ACCENT);
        assert_eq!(fg(RyeOsTone::Good), Color::Index(2));
        assert_eq!(fg(RyeOsTone::Warn), Color::Index(3));
        assert_eq!(fg(RyeOsTone::Danger), Color::Index(1));
        // No tone: the default ink, not a fifth colour.
        assert_eq!(fg(RyeOsTone::Neutral), Color::Default);
        assert_eq!(BG, Color::Default);
        assert_eq!(FG, Color::Default);
    }

    #[test]
    fn border_for_maps_names_and_degrades_to_thin() {
        assert_eq!(border_for("thick"), Some(Border::Thick));
        assert_eq!(border_for("thin"), Some(Border::Sharp));
        assert_eq!(border_for("hidden"), Some(Border::None));
        assert_eq!(border_for("none"), None);
        assert_eq!(border_for(""), Some(Border::Sharp));
        assert_eq!(border_for("wavy"), Some(Border::Sharp));
    }
}
