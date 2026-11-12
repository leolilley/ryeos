//! The RyeOS palette and type ramp: the same tokens the browser shell's
//! stylesheet declares, as Rune colours and text styles.

use rune::Color;
use rune::tok::{Family, TextStyle};

use ryeos_client_base::ui::view_model::RyeOsTone;

fn hex(value: &str) -> Color {
    rune::color::hex(value).expect("palette entries are valid hex")
}

/// Surface and ink colours.
pub struct Palette {
    pub bg: Color,
    pub bar: Color,
    pub plane: Color,
    pub plane_raised: Color,
    pub plane_selected: Color,
    pub line: Color,
    pub line_strong: Color,
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    pub accent_bright: Color,
    pub yellow: Color,
    pub green: Color,
    pub red: Color,
    /// Ink on the accent-filled launcher button.
    pub on_accent: Color,
}

impl Palette {
    pub fn get() -> Palette {
        Palette {
            bg: hex("#1d2021"),
            bar: hex("#1b1e1f"),
            plane: hex("#242423"),
            plane_raised: hex("#282827"),
            plane_selected: hex("#3b3028"),
            line: hex("#4a4842"),
            line_strong: hex("#665f52"),
            text: hex("#ebdbb2"),
            muted: hex("#a89984"),
            faint: hex("#7c7468"),
            accent: hex("#d65d0e"),
            accent_bright: hex("#fe8019"),
            yellow: hex("#fabd2f"),
            green: hex("#8ec07c"),
            red: hex("#fb4934"),
            on_accent: hex("#282828"),
        }
    }

    /// The ink a semantic tone takes; neutral keeps `fallback`.
    pub fn tone(&self, tone: RyeOsTone, fallback: Color) -> Color {
        match tone {
            RyeOsTone::Good => self.green,
            RyeOsTone::Warn => self.yellow,
            RyeOsTone::Danger => self.red,
            RyeOsTone::Accent => self.accent_bright,
            RyeOsTone::Neutral => fallback,
        }
    }

    /// The left rule of a notice in a tone.
    pub fn notice_rule(&self, tone: RyeOsTone) -> Color {
        match tone {
            RyeOsTone::Good => self.green,
            RyeOsTone::Warn => self.yellow,
            RyeOsTone::Danger => self.red,
            RyeOsTone::Accent | RyeOsTone::Neutral => self.accent,
        }
    }
}

/// The interface face (Inter) and the data face (IBM Plex Mono).
pub const UI: Family = Family::Sans;
pub const DATA: Family = Family::Mono;

pub const fn ui(size: f32) -> TextStyle {
    TextStyle::new(size, 400.0).family(UI)
}

pub const fn data(size: f32) -> TextStyle {
    TextStyle::new(size, 400.0).family(DATA)
}

/// Fixed shell metrics, in points.
pub const SYSTEM_HEIGHT: f32 = 24.0;
pub const STRIP_HEIGHT: f32 = 53.0;
pub const STATUS_HEIGHT: f32 = 27.0;
pub const NAV_WIDTH: f32 = 205.0;
pub const HEADER_HEIGHT: f32 = 34.0;

/// Which embedded face a string is set in.
#[derive(Clone, Copy, PartialEq)]
pub enum Face {
    Ui,
    Data,
}

/// Core-projected text with each symbol its face lacks replaced by one it has.
/// On the web Rune draws only from its embedded faces, so a missing symbol
/// would show as a missing glyph; the core's strings are shared with the
/// terminal and stay as they are.
pub fn drawable(text: &str, face: Face) -> std::borrow::Cow<'_, str> {
    let swap = |ch: char| -> Option<&'static str> {
        match (ch, face) {
            // Neither face has the small triangles.
            ('▸', _) => Some("›"),
            ('▾', Face::Ui) => Some("⌄"),
            ('▾', Face::Data) => Some("↓"),
            // Inter has no partial block; IBM Plex Mono does.
            ('▍', Face::Ui) => Some("|"),
            // IBM Plex Mono has no erase-left key symbol.
            ('⌫', Face::Data) => Some("Bksp"),
            _ => None,
        }
    };
    if !text.chars().any(|ch| swap(ch).is_some()) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match swap(ch) {
            Some(with) => out.push_str(with),
            None => out.push(ch),
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::{Face, drawable};

    #[test]
    fn missing_symbols_are_swapped_for_the_face_in_use() {
        assert_eq!(drawable("Earlier ▸", Face::Data), "Earlier ›");
        assert_eq!(drawable("▍ working…", Face::Ui), "| working…");
        assert_eq!(drawable("▍ working…", Face::Data), "▍ working…");
        assert_eq!(drawable("⌫ back", Face::Data), "Bksp back");
        assert_eq!(drawable("▾ open", Face::Data), "↓ open");
        assert_eq!(drawable("plain", Face::Ui), "plain");
    }
}
