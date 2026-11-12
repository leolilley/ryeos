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

fn main() {
    let fonts: Vec<&'static [u8]> = std::env::var("RYEOS_FIXTURE_FONTS")
        .unwrap_or_default()
        .split(':')
        .filter(|path| !path.is_empty())
        .filter_map(|path| std::fs::read(path).ok())
        .map(|bytes| &*Box::leak(bytes.into_boxed_slice()))
        .collect();
    let scene = std::env::var("RYEOS_FIXTURE_SCENE").unwrap_or_default();
    ryeos_client_rune::start_fixture(
        presentation::core(&scene),
        Box::leak(fonts.into_boxed_slice()),
    );
}
