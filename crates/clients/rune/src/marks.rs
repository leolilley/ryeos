//! The shell's small drawings: a Surface chip's mark, the dots that say work
//! needs you or is running, and a key hint's chip. Each takes plain values so
//! the shell decides what to show and these only draw it.

use rune::{Color, El, col, el, row, text};
use ryeos_client_base::layout_table::LAYOUT;

use crate::theme::{self, Tokens};

/// Home's mark: a 2 by 2 grid of 5 px squares.
pub(crate) fn grid_mark(ink: Color) -> El {
    let square = || el().size(5.0, 5.0).radius(1.5).fill(ink);
    col()
        .gap(3.0)
        .child(row().gap(3.0).child(square()).child(square()))
        .child(row().gap(3.0).child(square()).child(square()))
}

/// Side of the key figure, in cells. The figure is mirrored left to right, so
/// only the first `KEY_FIGURE_CELLS.div_ceil(2)` columns carry bits.
const KEY_FIGURE_CELLS: usize = LAYOUT.px.field.key_figure_cells;

/// The cells of a key figure, row by row, from a key fingerprint. The same
/// fingerprint always gives the same figure; it is a picture of the key, not
/// a proof of it, so the fold need not be cryptographic.
pub(crate) fn key_figure_cells(fingerprint: &str) -> [[bool; KEY_FIGURE_CELLS]; KEY_FIGURE_CELLS] {
    // FNV-1a over the fingerprint's text: every character moves the figure.
    let mut bits: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in fingerprint.bytes() {
        bits ^= u64::from(byte);
        bits = bits.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let half = KEY_FIGURE_CELLS.div_ceil(2);
    let mut cells = [[false; KEY_FIGURE_CELLS]; KEY_FIGURE_CELLS];
    for (y, line) in cells.iter_mut().enumerate() {
        for x in 0..half {
            let on = bits >> (y * half + x) & 1 == 1;
            line[x] = on;
            line[KEY_FIGURE_CELLS - 1 - x] = on;
        }
    }
    cells
}

/// A node's mark: its key's figure, `size` px square, in `ink`.
pub(crate) fn key_mark(fingerprint: &str, size: f32, ink: Color) -> El {
    let cell = size / KEY_FIGURE_CELLS as f32;
    let mut figure = col().size(size, size);
    for line in key_figure_cells(fingerprint) {
        let mut cells = row().h(cell);
        for on in line {
            let square = el().size(cell, cell);
            cells = cells.child(if on { square.fill(ink) } else { square });
        }
        figure = figure.child(cells);
    }
    figure
}

/// The first letter of a title, upper-cased; empty when the title is.
pub(crate) fn initial_of(title: &str) -> String {
    title
        .chars()
        .find(|c| !c.is_whitespace())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default()
}

/// The default mark: the title's first letter.
pub(crate) fn initial_mark(title: &str, ink: Color) -> El {
    text(initial_of(title))
        .font(theme::medium(theme::TYPE_MICRO))
        .color(ink)
}

/// What a dot says, if anything. Needing you wins over running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkDot {
    NeedsYou,
    Running,
}

impl WorkDot {
    /// The dot for these flags. Unknown counts (`None`) draw nothing, the same
    /// as nothing pending: the shell never guesses a count.
    pub(crate) fn of(needs_you: bool, running: bool) -> Option<Self> {
        match (needs_you, running) {
            (true, _) => Some(Self::NeedsYou),
            (false, true) => Some(Self::Running),
            (false, false) => None,
        }
    }

    fn color(self) -> Color {
        let t = Tokens::get();
        match self {
            Self::NeedsYou => t.warn,
            Self::Running => t.accent,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs you",
            Self::Running => "running",
        }
    }
}

/// The kind of dot, as its key's first word.
impl WorkDot {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs-you",
            Self::Running => "running",
        }
    }
}

/// A chip's dot: 6 px at its top-right corner.
pub(crate) fn chip_dot(dot: WorkDot, key: String) -> El {
    el().key(key)
        .size(6.0, 6.0)
        .radius(3.0)
        .fill(dot.color())
        .pin(None, Some(1.0), Some(1.0), None)
        .role("status")
        .label(dot.label())
}

/// A tab's dot: 5 px after its word.
pub(crate) fn tab_dot(dot: WorkDot, key: String) -> El {
    el().key(key)
        .size(5.0, 5.0)
        .radius(2.5)
        .fill(dot.color())
        .role("status")
        .label(dot.label())
}

/// A key hint's word at the status bar's ink.
pub(crate) const HINT_WORD_STATUS: f32 = 0.55;
/// A key hint's word at the overlay's footer ink.
pub(crate) const HINT_WORD_SHEET: f32 = 0.45;

/// A key's cap: a 20 high chip with the key's name.
pub(crate) fn key_chip(keys: &str) -> El {
    let t = Tokens::get();
    row()
        .h(20.0)
        .min_w(20.0)
        .px(6.0)
        .center()
        .radius(5.0)
        .border(1.0, t.ink_at(0.16))
        .child(
            text(theme::drawable(keys, theme::Face::Ui).into_owned())
                .font(theme::TYPE_KEY)
                .color(t.ink_at(0.60)),
        )
}

/// One key hint: the key chip, then its word at `word_ink` of `ink`.
pub(crate) fn key_hint(keys: &str, word: &str, word_ink: f32) -> El {
    let t = Tokens::get();
    row().gap(6.0).center_y().child(key_chip(keys)).child(
        text(theme::drawable(word, theme::Face::Ui).into_owned())
            .font(theme::TYPE_MICRO)
            .color(t.ink_at(word_ink)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_figure_is_stable_mirrored_and_moves_with_the_key() {
        let a = key_figure_cells("ed25519:3f9a0c");
        assert_eq!(a, key_figure_cells("ed25519:3f9a0c"));
        for line in a {
            for x in 0..KEY_FIGURE_CELLS {
                assert_eq!(line[x], line[KEY_FIGURE_CELLS - 1 - x]);
            }
        }
        assert_ne!(a, key_figure_cells("ed25519:3f9a0d"));
    }

    #[test]
    fn the_initial_skips_leading_space_and_is_empty_for_an_empty_title() {
        assert_eq!(initial_of("  ryeos"), "R");
        assert_eq!(initial_of(""), "");
    }

    #[test]
    fn needing_you_wins_over_running() {
        assert_eq!(WorkDot::of(true, true), Some(WorkDot::NeedsYou));
        assert_eq!(WorkDot::of(false, true), Some(WorkDot::Running));
        assert_eq!(WorkDot::of(false, false), None);
    }
}
