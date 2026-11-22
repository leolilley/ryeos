//! The RyeOS look: the colour, opacity, surface, radius, gap and type tokens,
//! the colours of the theme in force (served by the daemon as signed theme
//! items; the embedded boot theme draws until they arrive), and the shell
//! palette derived from them.

use std::sync::{OnceLock, RwLock};

use rune::Color;
use rune::tok::{Family, TextStyle};

use ryeos_client_base::theme_preference::{ThemeCatalogue, ThemeEntry, ThemePalette};
use ryeos_client_base::ui::view_model::RyeOsTone;

fn hex(value: &str) -> Color {
    rune::color::hex(value).expect("palette entries are valid hex")
}

/// The theme in force: its ref (`None` before a catalogue theme is selected,
/// when the embedded boot theme draws) and its tokens. A theme changes colour
/// only; it never moves or resizes anything.
struct Current {
    theme_ref: Option<String>,
    tokens: Tokens,
}

fn current() -> &'static RwLock<Current> {
    static CURRENT: OnceLock<RwLock<Current>> = OnceLock::new();
    CURRENT.get_or_init(|| {
        let boot = embedded(ryeos_client_base::theme_preference::BOOT_THEME)
            .expect("the boot theme is embedded");
        RwLock::new(Current {
            theme_ref: None,
            tokens: boot,
        })
    })
}

/// The tokens of an embedded shipped theme (`night`), for the boot palette
/// and for fixtures. A daemon-served theme is applied with [`use_theme`].
pub fn embedded(id: &str) -> Option<Tokens> {
    ryeos_client_base::theme_preference::embedded_theme(id)
        .map(|body| Tokens::from_palette(&body.palette))
}

/// The theme a stored preference selects from the catalogue the daemon sent,
/// given the platform's light/dark preference: `system` follows the
/// catalogue's pair, a theme ref selects itself, anything else nothing.
pub fn for_preference<'a>(
    catalogue: &'a ThemeCatalogue,
    stored: &str,
    system_dark: bool,
) -> Option<&'a ThemeEntry> {
    catalogue.resolve(stored, system_dark)
}

/// Select the theme every later draw uses.
pub fn use_theme(theme: &ThemeEntry) {
    let tokens = Tokens::from_palette(&theme.palette);
    let mut current = current()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    current.theme_ref = Some(theme.theme_ref.clone());
    current.tokens = tokens;
}

/// Select the embedded theme `id` for every later draw; false, with nothing
/// changed, for an id that is not embedded.
pub fn use_embedded(id: &str) -> bool {
    let Some(tokens) = embedded(id) else {
        return false;
    };
    let mut current = current()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    current.theme_ref = Some(format!("theme:ryeos/{id}"));
    current.tokens = tokens;
    true
}

/// The ref of the theme in force; `None` while the boot theme draws.
pub fn current_theme_ref() -> Option<String> {
    current()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .theme_ref
        .clone()
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

/// `picture`: the ground behind the tiles is drawn this strong (opacity) and
/// this soft (blur sigma, points), under a vertical wash of `bg` that runs
/// from [`GROUND_WASH_TOP`] at the top to [`GROUND_WASH_BOTTOM`] at the bottom.
pub const GROUND_OPACITY: f32 = 0.70;
pub const GROUND_BLUR: f32 = 10.0;
pub const GROUND_WASH_TOP: f32 = 0.45;
pub const GROUND_WASH_BOTTOM: f32 = 0.90;

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
    /// The tokens a theme's palette names.
    pub fn from_palette(palette: &ThemePalette) -> Tokens {
        Tokens {
            bg: hex(&palette.bg),
            ink: hex(&palette.ink),
            accent: hex(&palette.accent),
            good: hex(&palette.good),
            warn: hex(&palette.warn),
            danger: hex(&palette.danger),
        }
    }

    /// The tokens of the theme in force.
    pub fn get() -> Tokens {
        current()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tokens
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

/// The launcher's find line.
pub const TYPE_FIND: TextStyle = TextStyle::new(19.0, 400.0);
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
/// A key chip's text (status bar hints).
pub const TYPE_KEY: TextStyle = TextStyle::new(10.5, 500.0);
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
}

impl Palette {
    pub fn get() -> Palette {
        Palette::of(Tokens::get())
    }

    pub fn of(t: Tokens) -> Palette {
        let over = |opacity: f32| t.bg.mix(t.ink, opacity);
        Palette {
            bg: t.bg,
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

/// Which embedded face a string is set in.
#[derive(Clone, Copy, PartialEq)]
pub enum Face {
    Ui,
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
            // Inter has no partial block.
            ('▍', Face::Ui) => Some("|"),
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

/// The tabs band's height, and how far the words drop from its top: they sit
/// level with the switcher's chips.
pub const TABS_HEIGHT: f32 = 50.0;
pub const TABS_TOP: f32 = 6.0;
/// The Surfaces switcher along the top: its band, 10 above its 30 high island.
pub const SWITCHER_BAND_HEIGHT: f32 = 40.0;
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
        Face, INK_BODY, INK_DIM, INK_FAINT, INK_MUTE, INK_SOFT, INK_STRONG, Palette, Tokens,
        drawable, embedded, hex,
    };
    use ryeos_client_base::theme_preference::{
        SystemThemes, THEME_CATALOGUE_CONTRACT, ThemeCatalogue, ThemeEntry, embedded_theme,
        embedded_theme_ids,
    };
    use ryeos_client_base::ui::view_model::RyeOsTone;

    /// The spec's six colours, by theme: bg, ink, accent, good, warn, danger.
    /// The theme items are the single source; this table pins that the items
    /// still carry the spec's values.
    const COLOURS: [(&str, [&str; 6]); 3] = [
        (
            "night",
            [
                "#060708", "#f3f1ec", "#fe8019", "#9ccf8a", "#f2c24f", "#f0604d",
            ],
        ),
        (
            "paper",
            [
                "#ece8df", "#1a1917", "#c2410c", "#3f7a3a", "#a16207", "#b42318",
            ],
        ),
        (
            "gruvbox",
            [
                "#1d2021", "#ebdbb2", "#fe8019", "#b8bb26", "#fabd2f", "#fb4934",
            ],
        ),
    ];

    fn tokens(id: &str) -> Tokens {
        embedded(id).expect("embedded theme")
    }

    #[test]
    fn every_theme_carries_every_colour_token() {
        for (id, [bg, ink, accent, good, warn, danger]) in COLOURS {
            let t = tokens(id);
            assert_eq!(t.bg, hex(bg), "{id} bg");
            assert_eq!(t.ink, hex(ink), "{id} ink");
            assert_eq!(t.accent, hex(accent), "{id} accent");
            assert_eq!(t.good, hex(good), "{id} good");
            assert_eq!(t.warn, hex(warn), "{id} warn");
            assert_eq!(t.danger, hex(danger), "{id} danger");
        }
    }

    #[test]
    fn the_themes_differ_and_each_reaches_the_shell_palette() {
        let ground = |id| Palette::of(tokens(id)).bg;
        assert_ne!(ground("night"), ground("paper"));
        assert_ne!(ground("night"), ground("gruvbox"));
        assert_ne!(ground("paper"), ground("gruvbox"));
        let paper = Palette::of(tokens("paper"));
        assert_eq!(paper.green, tokens("paper").good);
    }

    #[test]
    fn ink_steps_and_surfaces_are_the_specs_opacities() {
        let t = tokens("night");
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
        for id in embedded_theme_ids() {
            let t = tokens(id);
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

    fn catalogue() -> ThemeCatalogue {
        let themes = embedded_theme_ids()
            .map(|id| {
                let body = embedded_theme(id).unwrap();
                ThemeEntry {
                    theme_ref: format!("theme:ryeos/{id}"),
                    id: body.id,
                    title: body.title,
                    palette: body.palette,
                    surface_grounds: Default::default(),
                }
            })
            .collect();
        ThemeCatalogue {
            schema_version: THEME_CATALOGUE_CONTRACT.into(),
            themes,
            system: SystemThemes {
                dark: "theme:ryeos/night".into(),
                light: "theme:ryeos/paper".into(),
            },
            default: "theme:ryeos/night".into(),
        }
    }

    #[test]
    fn a_stored_preference_selects_its_palette_from_the_catalogue() {
        use super::for_preference;
        let catalogue = catalogue();
        let id =
            |stored, dark| for_preference(&catalogue, stored, dark).map(|theme| theme.id.as_str());
        assert_eq!(id("theme:ryeos/gruvbox", true), Some("gruvbox"));
        assert_eq!(id("theme:ryeos/paper", true), Some("paper"));
        assert_eq!(id("system", true), Some("night"));
        assert_eq!(id("system", false), Some("paper"));
        assert_eq!(id("gruvbox", true), None);
        assert_eq!(id("dark", true), None);
    }

    #[test]
    fn the_boot_theme_draws_until_a_theme_is_selected() {
        // No test selects a theme (the selection is process-wide), so the
        // theme in force is still the embedded boot theme.
        assert_eq!(Tokens::get().bg, tokens("night").bg);
        assert_eq!(super::current_theme_ref(), None);
        assert!(embedded("solarized").is_none());
    }

    #[test]
    fn missing_symbols_are_swapped_for_the_face_in_use() {
        assert_eq!(drawable("▍ working…", Face::Ui), "| working…");
        assert_eq!(drawable("plain", Face::Ui), "plain");
    }
}
