//! The terminal palette: gruvbox dark per style.md, and the tone →
//! style mapping. This is the ONLY color authority in the renderer —
//! tone semantics come from the VM; draw sites take styles from here
//! and never invent colors.

use ryeos_client_base::studio::view_model::StudioTone;
use ryeos_client_base::text_surface::{Border, Color, Style};

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
// is no distinct PANEL fill for content; PANEL stays only for the launcher
// overlay, which deliberately stands out against the dimmed scrim.
pub fn tone_style(tone: StudioTone) -> Style {
    match tone {
        StudioTone::Good => Style::new().fg(GOOD).bg(BG),
        StudioTone::Warn => Style::new().fg(WARN).bg(BG),
        StudioTone::Danger => Style::new().fg(DANGER).bg(BG),
        StudioTone::Accent => Style::new().fg(ACCENT).bg(BG),
        StudioTone::Neutral => style_fg(),
    }
}

pub fn tone_glyph(tone: StudioTone) -> &'static str {
    match tone {
        StudioTone::Good => "✓",
        StudioTone::Warn => "!",
        StudioTone::Danger => "✗",
        StudioTone::Accent => "›",
        StudioTone::Neutral => "•",
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
