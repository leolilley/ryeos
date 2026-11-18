//! The RyeOS look: the colour, opacity, surface, radius, gap and type tokens,
//! the three themes that supply the colours, and the shell palette derived
//! from them.

use std::sync::atomic::{AtomicU8, Ordering};

use rune::Color;
use rune::tok::{Family, TextStyle};

use ryeos_client_base::ui::view_model::RyeOsTone;

fn hex(value: &str) -> Color {
    rune::color::hex(value).expect("palette entries are valid hex")
}

/// The three themes. A theme changes colour only; it never moves or resizes
/// anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeId {
    Night,
    Paper,
    Gruvbox,
}

impl ThemeId {
    pub const ALL: [ThemeId; 3] = [ThemeId::Night, ThemeId::Paper, ThemeId::Gruvbox];

    pub fn name(self) -> &'static str {
        match self {
            ThemeId::Night => "night",
            ThemeId::Paper => "paper",
            ThemeId::Gruvbox => "gruvbox",
        }
    }

    pub fn from_name(name: &str) -> Option<ThemeId> {
        ThemeId::ALL
            .into_iter()
            .find(|id| id.name().eq_ignore_ascii_case(name))
    }
}

/// The theme in force. Night until something selects another.
static THEME: AtomicU8 = AtomicU8::new(0);

/// Select the theme every later draw uses.
pub fn use_theme(id: ThemeId) {
    THEME.store(id as u8, Ordering::Relaxed);
}

/// The theme in force.
pub fn current_theme() -> ThemeId {
    match THEME.load(Ordering::Relaxed) {
        1 => ThemeId::Paper,
        2 => ThemeId::Gruvbox,
        _ => ThemeId::Night,
    }
}

// ---- Tokens ----

/// The six colours of a theme. Everything else is `ink` at an opacity step or
/// `bg` at an opacity.
#[derive(Clone, Copy)]
pub struct Tokens {
    /// Ground under the picture; base of sheets.
    pub bg: Color,
    /// All text and marks, at an opacity step.
    pub ink: Color,
    /// Tone `accent`: running, live, the focused tile's border.
    pub accent: Color,
    /// Tone `good`.
    pub good: Color,
    /// Tone `warn`: needs you, unavailable, cancelled.
    pub warn: Color,
    /// Tone `danger`.
    pub danger: Color,
}

/// Opacity steps on `ink`: titles, the selected row's name, values.
pub const INK_STRONG: f32 = 0.96;
/// Prose, table values.
pub const INK_BODY: f32 = 0.90;
/// One-line entry headlines, the kind column.
pub const INK_SOFT: f32 = 0.72;
/// Field names, detail text, state words.
pub const INK_DIM: f32 = 0.45;
/// Coverage and "does not prove" sentences, placeholders.
pub const INK_FAINT: f32 = 0.38;
/// Chevrons, separators.
pub const INK_MUTE: f32 = 0.30;

/// `sheet.fill`: `bg` at this opacity over the picture.
pub const SHEET_FILL: f32 = 0.58;
/// `sheet.border`: `ink` at this opacity, 1 px.
pub const SHEET_BORDER: f32 = 0.08;
/// `sheet.focus`: `accent` at this opacity, 1 px.
pub const SHEET_FOCUS: f32 = 0.70;
/// `select.fill`: `ink` at this opacity.
pub const SELECT_FILL: f32 = 0.08;
/// `rule`: `ink` at this opacity, 1 px.
pub const RULE: f32 = 0.10;
/// `control.border`: `ink` at this opacity, 1 px, for chips and the composer.
pub const CONTROL_BORDER: f32 = 0.12;
/// `control.border` at its strong end, for secondary buttons.
pub const CONTROL_BORDER_STRONG: f32 = 0.18;

pub const RADIUS_TILE: f32 = 16.0;
pub const RADIUS_CARD: f32 = 12.0;
pub const RADIUS_BUBBLE: f32 = 14.0;
pub const RADIUS_ROW: f32 = 8.0;
pub const RADIUS_BUTTON: f32 = 9.0;

/// `radius.pill`: half the height.
pub const fn radius_pill(height: f32) -> f32 {
    height / 2.0
}

/// `gap.tile`: between tiles.
pub const GAP_TILE: f32 = 12.0;
/// `pad.tile`: inside a tile, horizontal.
pub const PAD_TILE: f32 = 18.0;

impl Tokens {
    pub fn of(theme: ThemeId) -> Tokens {
        match theme {
            ThemeId::Night => Tokens {
                bg: hex("#060708"),
                ink: hex("#f3f1ec"),
                accent: hex("#fe8019"),
                good: hex("#9ccf8a"),
                warn: hex("#f2c24f"),
                danger: hex("#f0604d"),
            },
            ThemeId::Paper => Tokens {
                bg: hex("#ece8df"),
                ink: hex("#1a1917"),
                accent: hex("#c2410c"),
                good: hex("#3f7a3a"),
                warn: hex("#a16207"),
                danger: hex("#b42318"),
            },
            ThemeId::Gruvbox => Tokens {
                bg: hex("#1d2021"),
                ink: hex("#ebdbb2"),
                accent: hex("#fe8019"),
                good: hex("#b8bb26"),
                warn: hex("#fabd2f"),
                danger: hex("#fb4934"),
            },
        }
    }

    /// The tokens of the theme in force.
    pub fn get() -> Tokens {
        Tokens::of(current_theme())
    }

    /// `ink` at an opacity.
    pub fn ink_at(&self, opacity: f32) -> Color {
        self.ink.alpha(opacity)
    }

    /// `bg` at an opacity.
    pub fn bg_at(&self, opacity: f32) -> Color {
        self.bg.alpha(opacity)
    }

    pub fn ink_strong(&self) -> Color {
        self.ink_at(INK_STRONG)
    }
    pub fn ink_body(&self) -> Color {
        self.ink_at(INK_BODY)
    }
    pub fn ink_soft(&self) -> Color {
        self.ink_at(INK_SOFT)
    }
    pub fn ink_dim(&self) -> Color {
        self.ink_at(INK_DIM)
    }
    pub fn ink_faint(&self) -> Color {
        self.ink_at(INK_FAINT)
    }
    pub fn ink_mute(&self) -> Color {
        self.ink_at(INK_MUTE)
    }

    pub fn sheet_fill(&self) -> Color {
        self.bg_at(SHEET_FILL)
    }
    pub fn sheet_border(&self) -> Color {
        self.ink_at(SHEET_BORDER)
    }
    pub fn sheet_focus(&self) -> Color {
        self.accent.alpha(SHEET_FOCUS)
    }
    pub fn select_fill(&self) -> Color {
        self.ink_at(SELECT_FILL)
    }
    pub fn rule(&self) -> Color {
        self.ink_at(RULE)
    }
    pub fn control_border(&self) -> Color {
        self.ink_at(CONTROL_BORDER)
    }
    pub fn control_border_strong(&self) -> Color {
        self.ink_at(CONTROL_BORDER_STRONG)
    }

    /// The colour of a tone; no tone has none, and there is no fifth colour.
    pub fn tone(&self, tone: RyeOsTone) -> Option<Color> {
        match tone {
            RyeOsTone::Accent => Some(self.accent),
            RyeOsTone::Good => Some(self.good),
            RyeOsTone::Warn => Some(self.warn),
            RyeOsTone::Danger => Some(self.danger),
            RyeOsTone::Neutral => None,
        }
    }
}

// ---- Type ----

/// Prose, message, composer placeholder.
pub const TYPE_PROSE: TextStyle = TextStyle::new(15.0, 400.0);
/// Tile title, dock title.
pub const TYPE_TITLE: TextStyle = TextStyle::new(13.5, 500.0);
/// One-line entry headline and detail.
pub const TYPE_LINE: TextStyle = TextStyle::new(13.0, 400.0);
/// Section values.
pub const TYPE_CELL: TextStyle = TextStyle::new(12.5, 400.0);
/// Table cells, state words, field names.
pub const TYPE_SMALL: TextStyle = TextStyle::new(12.0, 400.0);
/// Chips, key hints, usage.
pub const TYPE_MICRO: TextStyle = TextStyle::new(11.5, 400.0);
/// Coverage sentences.
pub const TYPE_NOTE: TextStyle = TextStyle::new(11.0, 400.0);
/// Column headers and section titles; the caller sets the text in upper case.
pub const TYPE_LABEL: TextStyle = TextStyle::new(10.5, 500.0).tracking(1.5);

/// The same style at weight 500.
pub const fn medium(style: TextStyle) -> TextStyle {
    TextStyle {
        weight: 500.0,
        ..style
    }
}

/// `type.mono`: the monospace face at the size of the sans token it replaces.
pub const fn mono(style: TextStyle) -> TextStyle {
    style.family(Family::Mono)
}

// ---- The shell palette ----

/// Surface and ink colours the shell draws with, derived from the theme's
/// tokens so every theme reaches every surface.
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
    /// Ink on a filled control.
    pub on_accent: Color,
}

impl Palette {
    pub fn get() -> Palette {
        Palette::of(Tokens::get())
    }

    pub fn of(t: Tokens) -> Palette {
        let over = |opacity: f32| t.bg.mix(t.ink, opacity);
        Palette {
            bg: t.bg,
            bar: over(0.02),
            plane: over(0.04),
            plane_raised: over(0.07),
            plane_selected: over(0.12),
            line: over(RULE),
            line_strong: over(0.22),
            text: t.ink,
            muted: over(0.62),
            faint: over(0.46),
            accent: t.accent,
            accent_bright: t.accent,
            yellow: t.warn,
            green: t.good,
            red: t.danger,
            on_accent: t.bg,
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
pub const SYSTEM_HEIGHT: f32 = 28.0;
pub const STRIP_HEIGHT: f32 = 53.0;
pub const STATUS_HEIGHT: f32 = 28.0;

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

// ---- The shell look (part 5) ----

/// The tabs band's height.
pub const TABS_HEIGHT: f32 = 50.0;
/// The floating status bar's row: its 30 high islands and the 10 beneath.
pub const STATUS_BAR_HEIGHT: f32 = 40.0;
/// An island of the status bar.
pub const ISLAND_HEIGHT: f32 = 30.0;
/// The Surfaces switcher: its width, its chips and its inset from the edges.
pub const SWITCHER_WIDTH: f32 = 40.0;
/// The switcher while a Surface is being renamed.
pub const SWITCHER_RENAME_WIDTH: f32 = 260.0;
pub const SWITCHER_CHIP: f32 = 28.0;
pub const SWITCHER_INSET: f32 = 10.0;
/// Room the frame leaves at the left of the body (the body adds its own 18).
pub const SWITCHER_ROOM: f32 = 44.0;
pub const RADIUS_SWITCHER: f32 = 14.0;
/// `radius.sheet`: the overlay sheet.
pub const RADIUS_SHEET: f32 = 22.0;
/// The switcher and the status islands: `bg` at this opacity.
pub const BAR_FILL: f32 = 0.50;
/// Their border: `ink` at this opacity.
pub const BAR_BORDER: f32 = 0.10;
/// The overlay scrim: `bg` at this opacity.
pub const SCRIM_FILL: f32 = 0.62;
/// The overlay sheet's fill: `bg` mixed this far toward `ink`.
pub const SHEET_MIX: f32 = 0.035;

impl Tokens {
    /// A sheet's solid fill: `bg` mixed toward `ink`.
    pub fn sheet_solid(&self) -> Color {
        self.bg.mix(self.ink, SHEET_MIX)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Face, INK_BODY, INK_DIM, INK_FAINT, INK_MUTE, INK_SOFT, INK_STRONG, Palette, ThemeId,
        Tokens, drawable, hex,
    };
    use ryeos_client_base::ui::view_model::RyeOsTone;

    /// The spec's six colours, by theme: bg, ink, accent, good, warn, danger.
    const COLOURS: [(ThemeId, [&str; 6]); 3] = [
        (
            ThemeId::Night,
            [
                "#060708", "#f3f1ec", "#fe8019", "#9ccf8a", "#f2c24f", "#f0604d",
            ],
        ),
        (
            ThemeId::Paper,
            [
                "#ece8df", "#1a1917", "#c2410c", "#3f7a3a", "#a16207", "#b42318",
            ],
        ),
        (
            ThemeId::Gruvbox,
            [
                "#1d2021", "#ebdbb2", "#fe8019", "#b8bb26", "#fabd2f", "#fb4934",
            ],
        ),
    ];

    #[test]
    fn every_theme_carries_every_colour_token() {
        for (id, [bg, ink, accent, good, warn, danger]) in COLOURS {
            let t = Tokens::of(id);
            assert_eq!(t.bg, hex(bg), "{id:?} bg");
            assert_eq!(t.ink, hex(ink), "{id:?} ink");
            assert_eq!(t.accent, hex(accent), "{id:?} accent");
            assert_eq!(t.good, hex(good), "{id:?} good");
            assert_eq!(t.warn, hex(warn), "{id:?} warn");
            assert_eq!(t.danger, hex(danger), "{id:?} danger");
        }
    }

    #[test]
    fn the_themes_differ_and_each_reaches_the_shell_palette() {
        let ground = |id| Palette::of(Tokens::of(id)).bg;
        assert_ne!(ground(ThemeId::Night), ground(ThemeId::Paper));
        assert_ne!(ground(ThemeId::Night), ground(ThemeId::Gruvbox));
        assert_ne!(ground(ThemeId::Paper), ground(ThemeId::Gruvbox));
        let paper = Palette::of(Tokens::of(ThemeId::Paper));
        assert_eq!(paper.green, Tokens::of(ThemeId::Paper).good);
    }

    #[test]
    fn ink_steps_and_surfaces_are_the_specs_opacities() {
        let t = Tokens::of(ThemeId::Night);
        assert_eq!(t.ink_strong(), t.ink.alpha(0.96));
        assert_eq!(t.ink_body(), t.ink.alpha(0.90));
        assert_eq!(t.ink_soft(), t.ink.alpha(0.72));
        assert_eq!(t.ink_dim(), t.ink.alpha(0.45));
        assert_eq!(t.ink_faint(), t.ink.alpha(0.38));
        assert_eq!(t.ink_mute(), t.ink.alpha(0.30));
        assert_eq!(
            [INK_STRONG, INK_BODY, INK_SOFT, INK_DIM, INK_FAINT, INK_MUTE],
            [0.96, 0.90, 0.72, 0.45, 0.38, 0.30]
        );
        assert_eq!(t.sheet_fill(), t.bg.alpha(0.58));
        assert_eq!(t.sheet_border(), t.ink.alpha(0.08));
        assert_eq!(t.sheet_focus(), t.accent.alpha(0.70));
        assert_eq!(t.select_fill(), t.ink.alpha(0.08));
        assert_eq!(t.rule(), t.ink.alpha(0.10));
        assert_eq!(t.control_border(), t.ink.alpha(0.12));
        assert_eq!(t.control_border_strong(), t.ink.alpha(0.18));
    }

    #[test]
    fn a_tone_takes_the_specs_colour_and_no_tone_takes_none() {
        for id in ThemeId::ALL {
            let t = Tokens::of(id);
            assert_eq!(t.tone(RyeOsTone::Accent), Some(t.accent));
            assert_eq!(t.tone(RyeOsTone::Good), Some(t.good));
            assert_eq!(t.tone(RyeOsTone::Warn), Some(t.warn));
            assert_eq!(t.tone(RyeOsTone::Danger), Some(t.danger));
            assert_eq!(t.tone(RyeOsTone::Neutral), None);
            // The shell palette's tones are the same colours.
            let p = Palette::of(t);
            let fallback = t.ink;
            assert_eq!(p.tone(RyeOsTone::Good, fallback), t.good);
            assert_eq!(p.tone(RyeOsTone::Warn, fallback), t.warn);
            assert_eq!(p.tone(RyeOsTone::Danger, fallback), t.danger);
            assert_eq!(p.tone(RyeOsTone::Accent, fallback), t.accent);
            assert_eq!(p.tone(RyeOsTone::Neutral, fallback), fallback);
        }
    }

    #[test]
    fn themes_are_named_and_night_is_the_default() {
        for id in ThemeId::ALL {
            assert_eq!(ThemeId::from_name(id.name()), Some(id));
        }
        assert_eq!(ThemeId::from_name("PAPER"), Some(ThemeId::Paper));
        assert_eq!(ThemeId::from_name("solarized"), None);
        assert_eq!(super::current_theme(), ThemeId::Night);
    }

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
