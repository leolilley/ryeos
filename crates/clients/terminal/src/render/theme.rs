//! The terminal palette: gruvbox dark per style.md, and the tone →
//! style mapping. This is the ONLY color authority in the renderer —
//! tone semantics come from the VM; draw sites take styles from here
//! and never invent colors.

use ryeos_client_base::text_surface::{Border, Color, Style};
use ryeos_client_base::ui::view_model::RyeOsTone;

pub const BG: Color = Color::Rgb(0x1d, 0x20, 0x21);
pub const PANEL: Color = Color::Rgb(0x28, 0x28, 0x28);
pub const FG: Color = Color::Rgb(0xeb, 0xdb, 0xb2);
pub const FG_SOFT: Color = Color::Rgb(0xd5, 0xc4, 0xa1);
pub const MUTED: Color = Color::Rgb(0xa8, 0x99, 0x84);
pub const ACCENT: Color = Color::Rgb(0xd6, 0x5d, 0x0e);
pub const WARN: Color = Color::Rgb(0xfa, 0xbd, 0x2f);
pub const GOOD: Color = Color::Rgb(0x8e, 0xc0, 0x7c);
pub const DANGER: Color = Color::Rgb(0xfb, 0x49, 0x34);

// Content renders on the page background (BG), separated by borders —
// consistent across the backdrop, input box, tiles, and dock slots. There
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
    Style::new().fg(FG).bg(ACCENT)
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
pub(crate) fn shimmer_style(style: Style, changed_at_ms: Option<u64>, now_ms: u64) -> Style {
    let Some(changed_at_ms) = changed_at_ms else {
        return style;
    };
    let age = now_ms.saturating_sub(changed_at_ms);
    if age >= 1_200 {
        return style;
    }
    let weight = 0.35 * (1.0 - age as f32 / 1_200.0);
    style.fg(mix_toward(style.fg, ACCENT, weight))
}

/// An accent-toned row breathes: its foreground eases toward ACCENT on an
/// 8-phase wave so an actively-running row reads as alive.
pub(crate) fn active_pulse_style(style: Style, tone: RyeOsTone, now_ms: u64) -> Style {
    if tone != RyeOsTone::Accent {
        return style;
    }
    let phase = (now_ms / 180) % 8;
    let wave = match phase {
        0 | 7 => 0.08,
        1 | 6 => 0.14,
        2 | 5 => 0.20,
        _ => 0.26,
    };
    style.fg(mix_toward(style.fg, ACCENT, wave))
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
    fn border_for_maps_names_and_degrades_to_thin() {
        assert_eq!(border_for("thick"), Some(Border::Thick));
        assert_eq!(border_for("thin"), Some(Border::Sharp));
        assert_eq!(border_for("hidden"), Some(Border::None));
        assert_eq!(border_for("none"), None);
        assert_eq!(border_for(""), Some(Border::Sharp));
        assert_eq!(border_for("wavy"), Some(Border::Sharp));
    }
}
