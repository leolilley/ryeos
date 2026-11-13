//! The RyeOS shell over an in-memory presentation, for drawing checks without
//! a node or a seat:
//!
//!     cargo run -p ryeos-client-rune --example fixture -- --snap shell.png --size 1280x800
//!
//! `RYEOS_FIXTURE_SCENE` picks the presentation (`field`, `launcher`, or the
//! default shell); `RYEOS_FIXTURE_FONTS` adds colon-separated font files
//! (native Rune has no IBM Plex Mono of its own; without it the data face
//! falls back).

#[path = "../fixtures/presentation.rs"]
mod presentation;

use std::sync::OnceLock;

/// The font files, read once and held for the life of the process: the app
/// spec borrows them for `'static`.
static FONT_FILES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
static FONTS: OnceLock<Vec<&'static [u8]>> = OnceLock::new();

fn main() {
    let files = FONT_FILES.get_or_init(|| {
        std::env::var("RYEOS_FIXTURE_FONTS")
            .unwrap_or_default()
            .split(':')
            .filter(|path| !path.is_empty())
            .filter_map(|path| std::fs::read(path).ok())
            .collect()
    });
    let fonts = FONTS.get_or_init(|| files.iter().map(Vec::as_slice).collect());
    let scene = std::env::var("RYEOS_FIXTURE_SCENE").unwrap_or_default();
    ryeos_client_rune::start_fixture(presentation::core(&scene), fonts.as_slice());
}
