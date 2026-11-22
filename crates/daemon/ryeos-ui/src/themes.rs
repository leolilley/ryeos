//! The installed themes: loading the signed `theme` items and the signed
//! selection config, validating a stored preference against them, granting a
//! session the ground pictures it may draw, and the stable pick of a ground
//! from a slot's list.
//!
//! A theme is data. This module decides no look: it verifies what the
//! installed bundles carry, resolves their picture refs through the
//! [`ArtIndex`], and hands the clients one [`ThemeCatalogue`]. A theme that
//! is untrusted, malformed or misnamed is left out with a warning; a
//! selection config naming a theme that is not installed refuses the whole
//! catalogue, because `system` would then have nothing to follow.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::Value;

use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_client_base::art::{PICTURE_STATE_UNAVAILABLE, PictureBinding};
use ryeos_client_base::theme_preference::{
    SYSTEM, THEME_CATALOGUE_CONTRACT, ThemeBody, ThemeCatalogue, ThemeEntry, ThemeGround,
    ThemeSelection,
};
use ryeos_engine::contracts::SubjectResolutionAuthority;
use ryeos_engine::engine::EffectiveItemRequest;

use crate::art::ArtIndex;
use crate::browser_session::ArtGrantRefusal;

/// Ref of the signed config that names the themes `system` follows.
pub const SELECTION_CONFIG_REF: &str = "config:ryeos/ui/themes";
const SELECTION_CONFIG_PATH: &str = "ryeos/ui/themes.yaml";
const THEME_KIND: &str = "theme";

/// One installed, verified theme.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledTheme {
    pub theme_ref: String,
    pub body: ThemeBody,
}

/// The installed themes and the signed selection.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledThemes {
    /// Sorted by ref.
    pub themes: Vec<InstalledTheme>,
    pub selection: ThemeSelection,
}

impl InstalledThemes {
    pub fn theme(&self, theme_ref: &str) -> Option<&InstalledTheme> {
        self.themes
            .iter()
            .find(|theme| theme.theme_ref == theme_ref)
    }

    /// Whether `stored` is a value a principal may keep: `system` or the ref
    /// of an installed theme.
    pub fn allows(&self, stored: &str) -> bool {
        stored == SYSTEM || self.theme(stored).is_some()
    }

    /// The themes a session may draw for `stored`: both `system` themes when
    /// it follows the platform, else the one named theme.
    pub fn active_refs(&self, stored: &str) -> Vec<String> {
        if stored == SYSTEM {
            let mut refs = vec![self.selection.system.dark.clone()];
            if self.selection.system.light != self.selection.system.dark {
                refs.push(self.selection.system.light.clone());
            }
            refs
        } else {
            vec![stored.to_owned()]
        }
    }
}

/// Parse the selection config's body: the envelope fields are dropped first
/// and any other unknown field is refused.
pub fn selection_from_config(mut value: Value) -> Result<ThemeSelection, String> {
    if let Some(object) = value.as_object_mut() {
        for envelope in ["category", "version", "description"] {
            object.remove(envelope);
        }
    }
    serde_json::from_value(value).map_err(|error| format!("theme selection: {error}"))
}

/// Check each installed theme's body, leaving out the ones that fail, and
/// require the selection to name installed themes.
pub fn assemble(
    bodies: Vec<(String, Value)>,
    selection: ThemeSelection,
) -> Result<InstalledThemes, String> {
    let mut themes = Vec::new();
    for (theme_ref, value) in bodies {
        let name = theme_ref.rsplit('/').next().unwrap_or_default().to_owned();
        let parsed = serde_json::from_value::<ThemeBody>(value)
            .map_err(|error| error.to_string())
            .and_then(|body| body.validate(&name).map(|()| body));
        match parsed {
            Ok(body) => themes.push(InstalledTheme { theme_ref, body }),
            Err(reason) => {
                tracing::warn!(%theme_ref, %reason, "installed theme is malformed and is left out")
            }
        }
    }
    themes.sort_by(|left, right| left.theme_ref.cmp(&right.theme_ref));
    let installed = InstalledThemes { themes, selection };
    for named in [
        &installed.selection.system.dark,
        &installed.selection.system.light,
        &installed.selection.default,
    ] {
        if installed.theme(named).is_none() {
            return Err(format!(
                "{SELECTION_CONFIG_REF} names `{named}`, which is not an installed theme"
            ));
        }
    }
    Ok(installed)
}

/// Load every trusted installed theme and the signed selection.
pub fn load_installed(state: &AppState) -> Result<InstalledThemes> {
    let selection = selection_from_config(crate::handlers::ui_attention::load_signed_config(
        state,
        SELECTION_CONFIG_PATH,
        SELECTION_CONFIG_REF,
    )?)
    .map_err(HandlerError::Internal)?;
    let engine = &state.engine;
    let schema = engine
        .kinds
        .get(THEME_KIND)
        .ok_or_else(|| HandlerError::Internal("the theme kind is not installed".into()))?;
    let roots = engine.resolution_roots(None);
    let refs = ryeos_engine::item_resolution::enumerate_kind_refs(&roots, schema, THEME_KIND);
    let bodies = engine.with_checked_bundle_generation(|generation| -> Result<_> {
        let mut bodies = Vec::new();
        for item_ref in refs {
            let theme_ref = item_ref.to_string();
            let request = EffectiveItemRequest {
                logical_project_root: None,
                expected_kind: Some(THEME_KIND.to_string()),
                item_ref,
                project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
                    None,
                ),
            };
            match generation.effective_item(request) {
                Ok(item) if item.trusted => bodies.push((theme_ref, item.composed_value)),
                Ok(_) => tracing::warn!(%theme_ref, "installed theme is not trusted and is left out"),
                Err(error) => {
                    tracing::warn!(%theme_ref, %error, "installed theme did not resolve and is left out")
                }
            }
        }
        Ok(bodies)
    })?;
    Ok(assemble(bodies, selection).map_err(HandlerError::Internal)?)
}

/// How a session's ground grants are made: the session's picture index and a
/// function that records one hash in its grant set.
pub struct GroundGrants<'a> {
    pub index: &'a ArtIndex,
    pub grant: &'a mut dyn FnMut(&str) -> Result<(), ArtGrantRefusal>,
}

/// The catalogue a client receives for a principal whose stored preference is
/// `stored` (already known to be allowed). Every theme carries its palette;
/// only the themes the session may draw now carry Surface grounds, each
/// resolved through the picture index and granted to the session. A ground
/// whose binding fails to resolve, or whose grant is refused, is an
/// `unavailable` entry: nothing stands in for it. Without `grants` (a caller
/// that is not a browser session) no ground is resolved.
pub fn build_catalogue(
    installed: &InstalledThemes,
    stored: &str,
    mut grants: Option<GroundGrants<'_>>,
) -> ThemeCatalogue {
    let active = installed.active_refs(stored);
    let themes = installed
        .themes
        .iter()
        .map(|theme| {
            let mut surface_grounds = BTreeMap::new();
            if let Some(grants) = grants.as_mut()
                && active.contains(&theme.theme_ref)
            {
                for (surface_ref, binding) in &theme.body.surface_grounds {
                    surface_grounds.insert(surface_ref.clone(), resolve_ground(binding, grants));
                }
            }
            ThemeEntry {
                theme_ref: theme.theme_ref.clone(),
                id: theme.body.id.clone(),
                title: theme.body.title.clone(),
                palette: theme.body.palette.clone(),
                surface_grounds,
            }
        })
        .collect();
    ThemeCatalogue {
        schema_version: THEME_CATALOGUE_CONTRACT.to_string(),
        themes,
        system: installed.selection.system.clone(),
        default: installed.selection.default.clone(),
    }
}

fn unavailable() -> ThemeGround {
    ThemeGround {
        picture: None,
        picture_state: Some(PICTURE_STATE_UNAVAILABLE.to_string()),
    }
}

fn resolve_ground(binding: &PictureBinding, grants: &mut GroundGrants<'_>) -> ThemeGround {
    match grants.index.resolve_binding(binding) {
        Ok(resolved) => match (grants.grant)(&resolved.art.sha256) {
            Ok(()) => ThemeGround {
                picture: Some(resolved.art),
                picture_state: None,
            },
            Err(refusal) => {
                tracing::warn!(?refusal, "ground picture was not granted to the session");
                unavailable()
            }
        },
        Err(refusal) => {
            tracing::warn!(?refusal, picture = %binding.picture, "ground picture did not resolve");
            unavailable()
        }
    }
}

/// The index into a slot's list that a subject takes: the SHA-256 of the
/// subject key read as a big-endian number, modulo the list length. The same
/// key and list always give the same index, across sessions and nodes.
/// `None` for an empty list.
pub fn stable_index(subject_key: &str, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let digest = lillux::sha256_hex(subject_key.as_bytes());
    let modulus = len as u128;
    let mut remainder: u128 = 0;
    for pair in digest.as_bytes().chunks(2) {
        let byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        remainder = (remainder * 256 + u128::from(byte)) % modulus;
    }
    usize::try_from(remainder).ok()
}

/// The picture binding a theme gives a subject with none of its own: the
/// entry of the slot's list at [`stable_index`] of the subject key. Slot words
/// are opaque; they meet by string equality.
pub fn pick_ground<'a>(
    body: &'a ThemeBody,
    slot: &str,
    subject_key: &str,
) -> Option<&'a PictureBinding> {
    let list = body.grounds.get(slot)?;
    list.get(stable_index(subject_key, list.len())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::art::ArtRef;
    use serde_json::json;

    fn body(id: &str) -> Value {
        json!({
            "id": id, "title": id,
            "palette": {"bg": "#000000", "ink": "#ffffff", "accent": "#ff8000",
                        "good": "#00ff00", "warn": "#ffff00", "danger": "#ff0000"},
            "surface_grounds": {"surface:ryeos/ui/lens": {"picture": "picture:ryeos/a", "role": "hero"}},
            "grounds": {"project": [
                {"picture": "picture:ryeos/a"}, {"picture": "picture:ryeos/b"},
                {"picture": "picture:ryeos/c"}]}
        })
    }

    fn selection() -> ThemeSelection {
        selection_from_config(json!({
            "category": "ryeos/ui", "version": "1.0.0", "description": "x",
            "system": {"dark": "theme:ryeos/night", "light": "theme:ryeos/paper"},
            "default": "theme:ryeos/night"
        }))
        .unwrap()
    }

    fn installed() -> InstalledThemes {
        assemble(
            vec![
                ("theme:ryeos/paper".into(), body("paper")),
                ("theme:ryeos/night".into(), body("night")),
                ("theme:ryeos/gruvbox".into(), body("gruvbox")),
                // Misnamed: its id is not its name, so it is left out.
                ("theme:ryeos/other".into(), body("night")),
            ],
            selection(),
        )
        .unwrap()
    }

    #[test]
    fn installed_themes_are_sorted_and_a_misnamed_one_is_left_out() {
        let installed = installed();
        let refs: Vec<&str> = installed
            .themes
            .iter()
            .map(|t| t.theme_ref.as_str())
            .collect();
        assert_eq!(
            refs,
            [
                "theme:ryeos/gruvbox",
                "theme:ryeos/night",
                "theme:ryeos/paper"
            ]
        );
        assert!(installed.allows("system"));
        assert!(installed.allows("theme:ryeos/gruvbox"));
        for refused in ["night", "dark", "theme:ryeos/other", "theme:ryeos/neon"] {
            assert!(!installed.allows(refused), "{refused}");
        }
    }

    #[test]
    fn a_selection_naming_an_absent_theme_refuses_the_catalogue() {
        let mut broken = selection();
        broken.system.light = "theme:ryeos/absent".into();
        assert!(assemble(vec![("theme:ryeos/night".into(), body("night"))], broken).is_err());
        assert!(selection_from_config(json!({"system": {"dark": "a", "light": "b"}})).is_err());
    }

    fn index_with(hash: char, record: &str, role: &str) -> ArtIndex {
        use ryeos_bundle::pictures::{PictureEntry, PictureMediaType};
        let entry = PictureEntry {
            sha256: hash.to_string().repeat(64),
            media_type: PictureMediaType::Webp,
            role: role.into(),
            rendition_index: 0,
            width: 16,
            height: 10,
            bytes: 52,
            blob_path: std::path::PathBuf::from("/nonexistent"),
            record: record.into(),
        };
        let mut index = ArtIndex::default();
        index.insert_for_test(entry);
        index
    }

    #[test]
    fn only_the_active_themes_carry_grounds_and_each_is_granted() {
        let installed = installed();
        let index = index_with('a', "pictures/ryeos/a.yaml", "hero");
        let mut granted = Vec::new();
        let mut grant = |hash: &str| {
            granted.push(hash.to_owned());
            Ok(())
        };
        let catalogue = build_catalogue(
            &installed,
            "theme:ryeos/gruvbox",
            Some(GroundGrants {
                index: &index,
                grant: &mut grant,
            }),
        );
        catalogue.validate().unwrap();
        for theme in &catalogue.themes {
            assert_eq!(
                !theme.surface_grounds.is_empty(),
                theme.theme_ref == "theme:ryeos/gruvbox",
                "{}",
                theme.theme_ref
            );
        }
        assert_eq!(granted, vec!["a".repeat(64)]);
        let ground = &catalogue
            .theme("theme:ryeos/gruvbox")
            .unwrap()
            .surface_grounds["surface:ryeos/ui/lens"];
        assert_eq!(
            ground.picture,
            Some(ArtRef {
                sha256: "a".repeat(64),
                media_type: "image/webp".into()
            })
        );

        // `system` grants the dark and the light theme, once each hash.
        granted.clear();
        let mut grant = |hash: &str| {
            granted.push(hash.to_owned());
            Ok(())
        };
        let catalogue = build_catalogue(
            &installed,
            "system",
            Some(GroundGrants {
                index: &index,
                grant: &mut grant,
            }),
        );
        let with_grounds: Vec<&str> = catalogue
            .themes
            .iter()
            .filter(|t| !t.surface_grounds.is_empty())
            .map(|t| t.theme_ref.as_str())
            .collect();
        assert_eq!(with_grounds, ["theme:ryeos/night", "theme:ryeos/paper"]);

        // No session, no grounds.
        let catalogue = build_catalogue(&installed, "system", None);
        assert!(
            catalogue
                .themes
                .iter()
                .all(|t| t.surface_grounds.is_empty())
        );
    }

    #[test]
    fn a_failed_binding_or_a_refused_grant_is_unavailable_never_a_substitute() {
        let installed = installed();
        // The index holds a different record, so the theme's ref is not indexed.
        let other = index_with('b', "pictures/ryeos/other.yaml", "hero");
        let mut grant = |_: &str| Ok(());
        let catalogue = build_catalogue(
            &installed,
            "theme:ryeos/night",
            Some(GroundGrants {
                index: &other,
                grant: &mut grant,
            }),
        );
        let ground = &catalogue
            .theme("theme:ryeos/night")
            .unwrap()
            .surface_grounds["surface:ryeos/ui/lens"];
        assert!(ground.picture.is_none());
        assert_eq!(ground.picture_state.as_deref(), Some("unavailable"));

        // A full grant set is the same typed state, not a silent skip.
        let index = index_with('a', "pictures/ryeos/a.yaml", "hero");
        let mut full = |_: &str| Err(ArtGrantRefusal::SetFull);
        let catalogue = build_catalogue(
            &installed,
            "theme:ryeos/night",
            Some(GroundGrants {
                index: &index,
                grant: &mut full,
            }),
        );
        let ground = &catalogue
            .theme("theme:ryeos/night")
            .unwrap()
            .surface_grounds["surface:ryeos/ui/lens"];
        assert_eq!(ground.picture_state.as_deref(), Some("unavailable"));
    }

    #[test]
    fn the_stable_pick_is_the_digest_mod_the_list_length() {
        assert_eq!(stable_index("k", 0), None);
        assert_eq!(stable_index("k", 1), Some(0));
        // sha256("") = e3b0c442...b855; reduced mod 7 by an independent
        // big-number computation.
        let expected = {
            let digest = lillux::sha256_hex(b"");
            let mut value = 0u128;
            for ch in digest.chars() {
                value = (value * 16 + u128::from(ch.to_digit(16).unwrap())) % 7;
            }
            value as usize
        };
        assert_eq!(stable_index("", 7), Some(expected));
        // Same key and list, same pick; a longer list may move it, by design.
        for key in ["ryeos", "/home/leo/projects/x", "a"] {
            assert_eq!(stable_index(key, 3), stable_index(key, 3));
            assert!(stable_index(key, 3).unwrap() < 3);
        }
        let installed = installed();
        let theme = &installed.themes[0].body;
        let first = pick_ground(theme, "project", "ryeos").unwrap();
        assert_eq!(first, pick_ground(theme, "project", "ryeos").unwrap());
        assert!(pick_ground(theme, "node", "ryeos").is_none());
    }
}
