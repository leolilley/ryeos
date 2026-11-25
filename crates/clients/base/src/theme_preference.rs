//! Themes: the signed `theme` items a principal may choose, the stored
//! preference that names one, and how a stored value becomes a palette.
//!
//! A theme is a signed item (`theme:<id>`); its body is [`ThemeBody`]. The
//! daemon verifies the installed themes, validates a stored preference against
//! them and hands every client the same [`ThemeCatalogue`], so no client
//! carries a palette of its own. The stored preference is `system` or the ref
//! of an installed theme; `system` follows the platform's light or dark
//! preference through two themes a signed config names.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::art::{ArtRef, PictureBinding};

/// The stored value that follows the platform's light/dark preference.
pub const SYSTEM: &str = "system";

/// The ref prefix of a theme item.
pub const THEME_REF_PREFIX: &str = "theme:";

/// Schema marker of the catalogue the config service returns.
pub const THEME_CATALOGUE_CONTRACT: &str = "ryeos.ui.theme-catalogue.v1";

/// The word a ground carries when its picture binding failed to resolve.
pub use crate::art::PICTURE_STATE_UNAVAILABLE;

/// The id of the theme a renderer draws with until the daemon delivers the
/// catalogue.
pub const BOOT_THEME: &str = "night";

/// The shipped theme items, embedded as the files themselves (their
/// signature line is a YAML comment). They are the same files the daemon
/// verifies and serves; a renderer uses them only before the catalogue
/// arrives, and a fixture uses them with no daemon at all.
const EMBEDDED_THEMES: [(&str, &str); 3] = [
    (
        "night",
        include_str!("../../../../bundles/ryeos-ui/.ai/themes/ryeos/night.yaml"),
    ),
    (
        "paper",
        include_str!("../../../../bundles/ryeos-ui/.ai/themes/ryeos/paper.yaml"),
    ),
    (
        "gruvbox",
        include_str!("../../../../bundles/ryeos-ui/.ai/themes/ryeos/gruvbox.yaml"),
    ),
];

/// The ids of the embedded themes.
pub fn embedded_theme_ids() -> impl Iterator<Item = &'static str> {
    EMBEDDED_THEMES.iter().map(|(id, _)| *id)
}

/// An embedded shipped theme by id (`night`), validated. `None` for an id
/// that is not embedded.
pub fn embedded_theme(id: &str) -> Option<ThemeBody> {
    let (_, source) = EMBEDDED_THEMES.iter().find(|(name, _)| *name == id)?;
    let body: ThemeBody =
        serde_yaml::from_str(source).expect("an embedded theme item is a valid theme body");
    body.validate(id)
        .expect("an embedded theme item passes its own validation");
    Some(body)
}

fn is_word(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

fn is_item_ref(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|id| id.len() <= 200 && id.split('/').all(|segment| is_word(segment, 100)))
}

/// Whether `value` is a well-formed theme item ref (`theme:ryeos/night`).
pub fn is_theme_ref(value: &str) -> bool {
    is_item_ref(value, THEME_REF_PREFIX)
}

/// Whether `value` is a well-formed Surface item ref.
pub fn is_surface_ref(value: &str) -> bool {
    is_item_ref(value, "surface:")
}

/// Whether `value` is a stored preference's shape: `system` or a theme ref.
/// Whether the theme is installed is the catalogue's question.
pub fn is_preference_shape(value: &str) -> bool {
    value == SYSTEM || is_theme_ref(value)
}

fn is_hex_colour(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The six colours every other colour is derived from, each `#rrggbb`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemePalette {
    /// Ground under the picture; base of sheets.
    pub bg: String,
    /// All text and marks, at an opacity step.
    pub ink: String,
    pub accent: String,
    pub good: String,
    pub warn: String,
    pub danger: String,
}

impl ThemePalette {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("bg", &self.bg),
            ("ink", &self.ink),
            ("accent", &self.accent),
            ("good", &self.good),
            ("warn", &self.warn),
            ("danger", &self.danger),
        ] {
            if !is_hex_colour(value) {
                return Err(format!("palette {name} must be a #rrggbb colour"));
            }
        }
        Ok(())
    }
}

/// The body of a signed `theme` item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeBody {
    pub id: String,
    pub title: String,
    pub palette: ThemePalette,
    /// Pictures for a subject with none of its own, by opaque slot word.
    #[serde(default)]
    pub grounds: BTreeMap<String, Vec<PictureBinding>>,
    /// The picture drawn behind a named Surface, by Surface ref.
    #[serde(default)]
    pub surface_grounds: BTreeMap<String, PictureBinding>,
}

impl ThemeBody {
    /// Check the body is well formed and that `id` is the item's own name.
    pub fn validate(&self, expected_id: &str) -> Result<(), String> {
        if self.id != expected_id {
            return Err(format!(
                "theme id `{}` is not the item's name `{expected_id}`",
                self.id
            ));
        }
        if self.title.trim().is_empty() {
            return Err("theme title is empty".to_string());
        }
        self.palette.validate()?;
        for (slot, list) in &self.grounds {
            if !is_word(slot, 64) {
                return Err(format!(
                    "ground slot `{slot}` must be a short lowercase word"
                ));
            }
            if list.is_empty() {
                return Err(format!("ground slot `{slot}` lists no pictures"));
            }
        }
        for key in self.surface_grounds.keys() {
            if !is_surface_ref(key) {
                return Err(format!("surface ground key `{key}` is not a surface ref"));
            }
        }
        Ok(())
    }
}

/// The signed config that says which themes `system` follows and which theme
/// a node uses where no platform preference is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeSelection {
    pub system: SystemThemes,
    pub default: String,
}

/// The two themes `system` maps to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemThemes {
    pub dark: String,
    pub light: String,
}

/// What a client holds of a Surface's ground in one theme: the picture the
/// daemon granted this session, or `picture_state: unavailable` when the
/// binding failed (the Surface is then drawn flat; no other picture stands in).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeGround {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<ArtRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_state: Option<String>,
}

/// One installed theme as a client receives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeEntry {
    /// The theme's item ref, the value a preference stores.
    #[serde(rename = "ref")]
    pub theme_ref: String,
    pub id: String,
    pub title: String,
    pub palette: ThemePalette,
    /// Surface ground by Surface ref, resolved and granted for this session.
    /// Present only for the themes the session may draw now.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub surface_grounds: BTreeMap<String, ThemeGround>,
}

/// The installed themes and how `system` and the default pick among them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeCatalogue {
    pub schema_version: String,
    pub themes: Vec<ThemeEntry>,
    pub system: SystemThemes,
    pub default: String,
}

impl ThemeCatalogue {
    /// Check the catalogue's own consistency: every ref well formed and
    /// unique, and `system` and `default` naming themes it holds.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != THEME_CATALOGUE_CONTRACT {
            return Err("theme catalogue schema is not current".to_string());
        }
        let mut seen = std::collections::BTreeSet::new();
        for theme in &self.themes {
            if !is_theme_ref(&theme.theme_ref) {
                return Err(format!("theme ref `{}` is malformed", theme.theme_ref));
            }
            if !seen.insert(theme.theme_ref.as_str()) {
                return Err(format!("theme `{}` is listed twice", theme.theme_ref));
            }
            theme.palette.validate()?;
        }
        for named in [&self.system.dark, &self.system.light, &self.default] {
            if self.theme(named).is_none() {
                return Err(format!(
                    "`{named}` is named by the theme selection but not installed"
                ));
            }
        }
        Ok(())
    }

    /// The installed theme with this ref.
    pub fn theme(&self, theme_ref: &str) -> Option<&ThemeEntry> {
        self.themes
            .iter()
            .find(|theme| theme.theme_ref == theme_ref)
    }

    /// Whether `stored` is a value a principal may keep: `system` or the ref
    /// of an installed theme.
    pub fn allows(&self, stored: &str) -> bool {
        stored == SYSTEM || self.theme(stored).is_some()
    }

    /// The theme a stored preference selects, given the platform's
    /// preference. `None` for a value that is not allowed.
    pub fn resolve(&self, stored: &str, platform_dark: bool) -> Option<&ThemeEntry> {
        if stored == SYSTEM {
            let named = if platform_dark {
                &self.system.dark
            } else {
                &self.system.light
            };
            return self.theme(named);
        }
        self.theme(stored)
    }

    /// The refs a session may draw for `stored`: both `system` themes when it
    /// follows the platform, else the one named theme.
    pub fn active_refs(&self, stored: &str) -> Vec<&str> {
        if stored == SYSTEM {
            let mut refs = vec![self.system.dark.as_str()];
            if self.system.light != self.system.dark {
                refs.push(self.system.light.as_str());
            }
            refs
        } else {
            self.theme(stored)
                .map(|theme| vec![theme.theme_ref.as_str()])
                .unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> ThemePalette {
        ThemePalette {
            bg: "#000000".into(),
            ink: "#ffffff".into(),
            accent: "#ff8000".into(),
            good: "#00ff00".into(),
            warn: "#ffff00".into(),
            danger: "#ff0000".into(),
        }
    }

    fn entry(id: &str) -> ThemeEntry {
        ThemeEntry {
            theme_ref: format!("theme:ryeos/{id}"),
            id: id.into(),
            title: id.into(),
            palette: palette(),
            surface_grounds: BTreeMap::new(),
        }
    }

    fn catalogue() -> ThemeCatalogue {
        ThemeCatalogue {
            schema_version: THEME_CATALOGUE_CONTRACT.into(),
            themes: vec![entry("night"), entry("paper"), entry("gruvbox")],
            system: SystemThemes {
                dark: "theme:ryeos/night".into(),
                light: "theme:ryeos/paper".into(),
            },
            default: "theme:ryeos/night".into(),
        }
    }

    #[test]
    fn system_follows_the_platform_pair() {
        let catalogue = catalogue();
        catalogue.validate().unwrap();
        assert_eq!(catalogue.resolve("system", true).unwrap().id, "night");
        assert_eq!(catalogue.resolve("system", false).unwrap().id, "paper");
    }

    #[test]
    fn a_theme_ref_resolves_to_itself_and_other_values_do_not() {
        let catalogue = catalogue();
        for id in ["night", "paper", "gruvbox"] {
            let stored = format!("theme:ryeos/{id}");
            assert!(catalogue.allows(&stored));
            assert_eq!(catalogue.resolve(&stored, true).unwrap().id, id);
            assert_eq!(catalogue.resolve(&stored, false).unwrap().id, id);
        }
        for old in [
            "night",
            "light",
            "dark",
            "neon",
            "",
            "theme:ryeos/neon",
            "Night",
        ] {
            assert!(!catalogue.allows(old), "{old}");
            assert!(catalogue.resolve(old, true).is_none());
        }
    }

    #[test]
    fn a_catalogue_naming_an_absent_theme_is_refused() {
        let mut broken = catalogue();
        broken.system.light = "theme:ryeos/absent".into();
        assert!(broken.validate().is_err());
        let mut twice = catalogue();
        twice.themes.push(entry("night"));
        assert!(twice.validate().is_err());
    }

    #[test]
    fn active_refs_are_the_system_pair_or_the_one_theme() {
        let catalogue = catalogue();
        assert_eq!(
            catalogue.active_refs("system"),
            vec!["theme:ryeos/night", "theme:ryeos/paper"]
        );
        assert_eq!(
            catalogue.active_refs("theme:ryeos/gruvbox"),
            vec!["theme:ryeos/gruvbox"]
        );
        assert!(catalogue.active_refs("theme:ryeos/neon").is_empty());
    }

    #[test]
    fn a_theme_body_checks_its_name_palette_slots_and_surface_keys() {
        let ok = serde_json::json!({
            "id": "night", "title": "Night",
            "palette": {"bg": "#060708", "ink": "#f3f1ec", "accent": "#fe8019",
                        "good": "#9ccf8a", "warn": "#f2c24f", "danger": "#f0604d"},
            "grounds": {"project": [{"picture": "picture:ryeos/a", "role": "card"}]},
            "surface_grounds": {"surface:ryeos/ui/lens": {"picture": "picture:ryeos/a"}}
        });
        let body: ThemeBody = serde_json::from_value(ok.clone()).unwrap();
        body.validate("night").unwrap();
        assert!(body.validate("paper").is_err());
        let mutate = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut bad = ok.clone();
            edit(&mut bad);
            bad
        };
        let bad_bodies = [
            mutate(&|v| v["palette"]["bg"] = serde_json::json!("060708")),
            mutate(&|v| {
                v["grounds"] = serde_json::json!({"Project": [{"picture": "picture:ryeos/a"}]})
            }),
            mutate(&|v| v["grounds"] = serde_json::json!({"project": []})),
        ];
        for bad in bad_bodies {
            let parsed: ThemeBody = serde_json::from_value(bad).unwrap();
            assert!(parsed.validate("night").is_err());
        }
        for reserved in ["content", "voice"] {
            let mut extra = ok.clone();
            extra[reserved] = serde_json::json!({});
            assert!(serde_json::from_value::<ThemeBody>(extra).is_err());
        }
        let mut surface = ok.clone();
        surface["surface_grounds"] =
            serde_json::json!({"view:ryeos/x": {"picture": "picture:ryeos/a"}});
        let parsed: ThemeBody = serde_json::from_value(surface).unwrap();
        assert!(parsed.validate("night").is_err());
        let mut stray = ok;
        stray["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<ThemeBody>(stray).is_err());
    }

    #[test]
    fn the_embedded_themes_are_the_shipped_items_and_the_boot_theme_is_among_them() {
        let ids: Vec<&str> = embedded_theme_ids().collect();
        assert_eq!(ids, ["night", "paper", "gruvbox"]);
        for id in &ids {
            let body = embedded_theme(id).unwrap();
            assert_eq!(body.id, *id);
        }
        assert!(ids.contains(&BOOT_THEME));
        assert!(embedded_theme("solarized").is_none());
    }

    #[test]
    fn a_preference_shape_is_system_or_a_theme_ref() {
        for good in ["system", "theme:ryeos/night", "theme:a"] {
            assert!(is_preference_shape(good), "{good}");
        }
        for bad in [
            "night",
            "theme:",
            "theme:/a",
            "theme:A",
            "system ",
            "surface:a",
        ] {
            assert!(!is_preference_shape(bad), "{bad}");
        }
    }
}
