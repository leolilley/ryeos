//! The stored theme preference: which values a principal may keep, and how a
//! stored value becomes a palette.
//!
//! The daemon validates the stored value against this list and every client
//! reads it from here, so the two cannot disagree. The list is a constant for
//! now; a signed theme catalogue is a later item and would replace it.

/// The stored value that follows the platform's light/dark preference.
pub const SYSTEM: &str = "system";

/// Palette ids a client really carries.
pub const PALETTE_IDS: [&str; 3] = ["night", "paper", "gruvbox"];

/// The palette `system` maps to when the platform prefers dark.
pub const SYSTEM_DARK: &str = "night";

/// The palette `system` maps to when the platform prefers light.
pub const SYSTEM_LIGHT: &str = "paper";

/// Every value the stored preference may hold: `system` and the palette ids.
pub const ALLOWED: [&str; 4] = [SYSTEM, PALETTE_IDS[0], PALETTE_IDS[1], PALETTE_IDS[2]];

/// The palette id a stored value selects, given the platform's preference.
/// `None` for a value outside [`ALLOWED`].
pub fn resolve(stored: &str, platform_dark: bool) -> Option<&'static str> {
    if stored == SYSTEM {
        return Some(if platform_dark {
            SYSTEM_DARK
        } else {
            SYSTEM_LIGHT
        });
    }
    PALETTE_IDS.iter().copied().find(|id| *id == stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_follows_the_platform_pair() {
        assert_eq!(resolve("system", true), Some("night"));
        assert_eq!(resolve("system", false), Some("paper"));
    }

    #[test]
    fn palette_ids_resolve_to_themselves_and_others_do_not() {
        for id in PALETTE_IDS {
            assert_eq!(resolve(id, true), Some(id));
            assert_eq!(resolve(id, false), Some(id));
        }
        for old in ["light", "dark", "neon", "", "Night"] {
            assert_eq!(resolve(old, true), None);
        }
    }

    #[test]
    fn the_system_pair_are_palettes() {
        assert!(PALETTE_IDS.contains(&SYSTEM_DARK));
        assert!(PALETTE_IDS.contains(&SYSTEM_LIGHT));
    }
}
