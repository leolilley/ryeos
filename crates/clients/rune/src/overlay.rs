//! The overlay: one key over any Surface shows the open Surfaces, what needs
//! you and what is running. The core holds whether it is open and which row
//! is selected; this only draws it.

use rune::{El, col, el, row, text};
use ryeos_client_base::ui::RyeOsUiEvent;
use ryeos_client_base::ui::view_model::{RyeOsOverlayRowVm, RyeOsSurfaceChoiceVm, RyeOsViewModel};

use crate::App;
use crate::Local;
use crate::marks::{self, WorkDot};
use crate::theme::{self, Tokens};

/// Width of the sheet, before a narrow window takes it in.
const SHEET_WIDTH: f32 = 920.0;
/// The sheet's top, from the screen's.
const SHEET_TOP: f32 = 150.0;
const SHEET_RADIUS: f32 = 22.0;
const ROW_HEIGHT: f32 = 46.0;

/// The overlay's layer: the scrim and the sheet. Pressing the scrim closes it.
pub(crate) fn layer(vm: &RyeOsViewModel, local: &Local) -> El {
    let t = Tokens::get();
    let overlay = &vm.overlay;
    let now_ms = i64::try_from(vm.now_ms).unwrap_or(i64::MAX);
    let mut sheet = col()
        .key("overlay")
        .role("dialog")
        .label("Overlay")
        .w_full()
        .max_w(SHEET_WIDTH)
        .shrink()
        .min_h(0.0)
        .radius(SHEET_RADIUS)
        .clip_rounded(SHEET_RADIUS)
        .fill(t.sheet_solid())
        .border(1.0, t.ink_at(0.14))
        .shadow(30.0, 90.0, t.bg_at(0.7));
    sheet = sheet.child(surfaces_section(&vm.session.surfaces));
    // The selection runs across needs-you, then running.
    let selected = overlay.selected;
    let offset = overlay.needs_you.len();
    sheet = sheet.child(
        row()
            .w_full()
            .shrink()
            .min_h(0.0)
            .px(24.0)
            .pb(18.0)
            .gap(24.0)
            .child(work_column(
                "Needs you",
                "overlay:needs-you",
                &overlay.needs_you,
                overlay.needs_you_count,
                selected,
                now_ms,
                0,
            ))
            .child(work_column(
                "Running",
                "overlay:running",
                &overlay.running,
                overlay.running_count,
                selected.and_then(|index| index.checked_sub(offset)),
                now_ms,
                offset,
            )),
    );
    sheet = sheet.child(footer(&overlay.key_hints));
    let narrow = local.narrow();
    col()
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .items_center()
        .pt(if narrow { 36.0 } else { SHEET_TOP })
        .px(if narrow { 16.0 } else { 18.0 })
        .pb(18.0)
        .fill(t.bg_at(0.62))
        .modal(true, |app: &mut App| app.ui(RyeOsUiEvent::ToggleOverlay))
        .child(sheet)
}

/// A section's label, with its count in the accent when there is one.
fn section_label(label: &str, count: Option<usize>) -> El {
    let t = Tokens::get();
    let mut line = row().gap(8.0).center_y().child(
        text(label.to_uppercase())
            .font(theme::TYPE_LABEL)
            .color(t.ink_at(0.50)),
    );
    if let Some(count) = count {
        line = line.child(
            text(count.to_string())
                .font(theme::TYPE_KEY)
                .color(t.accent),
        );
    }
    line
}

/// The open Surfaces as cards in a row; the current one has the strong edge.
fn surfaces_section(surfaces: &[RyeOsSurfaceChoiceVm]) -> El {
    let t = Tokens::get();
    let mut cards = row().w_full().gap(12.0).scroll_x();
    for surface in surfaces {
        let (edge, edge_ink, ink) = if surface.active {
            (1.5, t.ink_at(0.95), t.ink)
        } else {
            (1.0, t.ink_at(0.14), t.ink_at(0.70))
        };
        let mark = match surface.chip.mark {
            ryeos_client_base::ui::view_model::RyeOsChipMarkVm::Grid => marks::grid_mark(ink),
            ryeos_client_base::ui::view_model::RyeOsChipMarkVm::Key => {
                match surface.chip.fingerprint.as_deref() {
                    Some(fingerprint) => marks::key_mark(fingerprint, 20.0, ink),
                    None => marks::initial_mark(&surface.title, ink),
                }
            }
            ryeos_client_base::ui::view_model::RyeOsChipMarkVm::Initial => {
                marks::initial_mark(&surface.title, ink)
            }
        };
        let mut name = surface.title.clone();
        if surface.chip.authored_in_project {
            name.push('·');
        }
        let key = format!("overlay:surface:{:?}", surface.surface_instance_id);
        let mut card = col()
            .key(key.clone())
            .w(206.0)
            .h(84.0)
            .padding(12.0)
            .gap(10.0)
            .radius(theme::RADIUS_ROW)
            .border(edge, edge_ink)
            .role("button")
            .label(surface.title.clone())
            .hover(t.select_fill())
            .child(mark)
            .child(
                row()
                    .gap(6.0)
                    .center_y()
                    .child(
                        text(format!("{}", surface.ordinal))
                            .font(theme::ui(10.5))
                            .color(t.ink_at(0.35)),
                    )
                    .child(
                        text(name)
                            .font(theme::medium(theme::TYPE_CELL))
                            .color(ink)
                            .truncate()
                            .min_w(0.0),
                    ),
            );
        let dot = WorkDot::of(
            surface.needs_you.is_some_and(|count| count > 0),
            surface.running.is_some_and(|count| count > 0),
        );
        if let Some(dot) = dot {
            card = card.child(marks::chip_dot(dot, format!("{}:{key}", dot.word())));
        }
        let select = surface.select_intent.clone();
        card = card.on_press(move |app: &mut App| {
            app.ui(RyeOsUiEvent::Activate {
                intent: select.clone(),
            });
        });
        cards = cards.child(card);
    }
    col()
        .w_full()
        .px(24.0)
        .pt(20.0)
        .pb(18.0)
        .gap(10.0)
        .child(section_label("Open Surfaces", Some(surfaces.len())))
        .child(cards)
}

/// One of the two work columns: its label, then its rows; "unavailable" when
/// the count is unknown, a quiet line when there is nothing.
fn work_column(
    label: &str,
    key: &str,
    rows: &[RyeOsOverlayRowVm],
    count: Option<usize>,
    selected: Option<usize>,
    now_ms: i64,
    offset: usize,
) -> El {
    let t = Tokens::get();
    let mut column = col()
        .key(key.to_owned())
        .grow(1.0)
        .w(0.0)
        .min_w(0.0)
        .gap(4.0)
        .child(section_label(label, count));
    if count.is_none() {
        return column.child(
            text("unavailable")
                .font(theme::TYPE_MICRO)
                .color(t.ink_dim())
                .py(8.0),
        );
    }
    if rows.is_empty() {
        return column.child(
            text("nothing here")
                .font(theme::TYPE_MICRO)
                .color(t.ink_dim())
                .py(8.0),
        );
    }
    for (index, item) in rows.iter().enumerate() {
        column = column.child(work_row(
            item,
            selected == Some(index),
            now_ms,
            format!("{key}:{}", offset + index),
        ));
    }
    if let Some(count) = count.filter(|count| *count > rows.len()) {
        column = column.child(
            text(format!("{} more", count - rows.len()))
                .font(theme::TYPE_MICRO)
                .color(t.ink_dim())
                .px(12.0),
        );
    }
    column
}

/// One row: tone dot, title over where, then the action keys (or, with none,
/// how long it has been).
fn work_row(item: &RyeOsOverlayRowVm, selected: bool, now_ms: i64, key: String) -> El {
    let t = Tokens::get();
    let mut line = row()
        .key(key)
        .w_full()
        .h(ROW_HEIGHT)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .radius(theme::RADIUS_ROW)
        .role("option")
        .label(item.title.clone())
        .focus_selected(selected);
    if selected {
        line = line.fill(t.ink_at(0.07));
    }
    let dot_color = t.tone(item.tone).unwrap_or(t.ink_dim());
    line = line
        .child(el().size(6.0, 6.0).radius(3.0).fill(dot_color))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(
                    text(item.title.clone())
                        .font(theme::TYPE_LINE)
                        .color(t.ink_at(0.92))
                        .truncate(),
                )
                .child(
                    text(item.where_.clone())
                        .font(theme::TYPE_MICRO)
                        .color(t.ink_at(0.42))
                        .truncate(),
                ),
        );
    if item.actions.is_empty() || !selected {
        line = line.child(
            text(elapsed(now_ms.saturating_sub(item.since_ms)))
                .font(theme::TYPE_MICRO)
                .color(t.ink_dim()),
        );
    } else {
        for action in &item.actions {
            let intent = action.intent.clone();
            line = line.child(
                row()
                    .role("button")
                    .label(action.word.clone())
                    .child(marks::key_hint(&action.keys, &action.word))
                    .on_press(move |app: &mut App| {
                        app.ui(RyeOsUiEvent::Activate {
                            intent: intent.clone(),
                        });
                    }),
            );
        }
    }
    line
}

/// The sheet's footer: its key hints over a rule.
fn footer(hints: &[ryeos_client_base::ui::view_model::RyeOsKeyHintVm]) -> El {
    let t = Tokens::get();
    let mut line = row()
        .w_full()
        .h(44.0)
        .px(24.0)
        .gap(14.0)
        .center_y()
        .border_sides([1.0, 0.0, 0.0, 0.0], t.rule());
    for hint in hints {
        line = line.child(marks::key_hint(&hint.keys, &hint.word));
    }
    line
}

/// How long since, in the largest whole unit: `now`, `5m`, `2h`, `3d`.
pub(crate) fn elapsed(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::elapsed;

    #[test]
    fn elapsed_time_reads_in_its_largest_whole_unit() {
        assert_eq!(elapsed(-5), "now");
        assert_eq!(elapsed(59_999), "now");
        assert_eq!(elapsed(60_000), "1m");
        assert_eq!(elapsed(3_600_000), "1h");
        assert_eq!(elapsed(2 * 86_400_000 + 1), "2d");
    }
}
