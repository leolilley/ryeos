//! The launcher sheet: a find line, the views grouped by library group, the
//! places a chosen view can land, and a footer with the keys. The core owns
//! the list, the filter, the stage and the keys; this only draws them.

use rune::{El, ScrollSpec, col, el, row, text};
use ryeos_client_base::ids::RyeOsViewInstanceKey;
use ryeos_client_base::ui::RyeOsUiEvent;
use ryeos_client_base::ui::view_model::{RyeOsDialogVm, RyeOsRowVm, RyeOsViewVm};

use crate::theme::{self, Tokens};
use crate::{App, Local};

/// The sheet's width, before a narrow window takes it in.
const SHEET_WIDTH: f32 = 860.0;
/// The sheet's tallest, so the list scrolls inside it.
const SHEET_MAX_HEIGHT: f32 = 640.0;
/// The find line's height and its text.
const FIND_HEIGHT: f32 = 64.0;
/// A view's row: its inset, the gap between its columns and its radius.
const ROW_PAD_X: f32 = 14.0;
const ROW_PAD_Y: f32 = 10.0;
const ROW_GAP: f32 = 14.0;
const ROW_RADIUS: f32 = 11.0;
/// A view's row is at least this tall, whatever lines it carries.
const ROW_HEIGHT: f32 = 34.0;
/// The ink of the commands' label and of a command's name at rest: quieter
/// than the views', which they follow.
const COMMAND_LABEL_INK: f32 = 0.30;
const COMMAND_NAME_INK: f32 = 0.60;
/// The title column's width.
const NAME_WIDTH: f32 = 190.0;
/// A place's height and radius.
const PLACE_HEIGHT: f32 = 44.0;
const PLACE_RADIUS: f32 = 10.0;
/// The footer's height.
const FOOT_HEIGHT: f32 = 44.0;
/// How far a row that cannot be chosen is dimmed.
const DIMMED: f32 = 0.55;

/// The group every record that is not a view is listed under.
const COMMANDS: &str = "Commands";

fn is_place(row: &RyeOsRowVm) -> bool {
    row.launch.as_ref().is_some_and(|launch| launch.place)
}

fn is_command(row: &RyeOsRowVm) -> bool {
    row.launch
        .as_ref()
        .is_some_and(|launch| !launch.place && launch.group == COMMANDS)
}

fn is_view(row: &RyeOsRowVm) -> bool {
    row.launch
        .as_ref()
        .is_some_and(|launch| !launch.place && launch.group != COMMANDS)
}

/// The launcher's layer: the scrim and the sheet. Pressing the scrim closes it.
pub(crate) fn layer(dialog: &RyeOsDialogVm, local: &Local) -> El {
    let t = Tokens::get();
    let close = dialog.instance_key.clone();
    let rows: &[RyeOsRowVm] = match &dialog.view {
        RyeOsViewVm::Rows { rows, .. } => rows,
        _ => &[],
    };
    let places = rows.iter().filter(|row| is_place(row)).collect::<Vec<_>>();
    let listed = rows.iter().filter(|row| is_view(row)).count();
    let rule = || el().w_full().h(1.0).fill(t.ink_at(0.08));
    let mut sheet = col()
        .key(format!("dialog:{:?}", dialog.instance_key))
        .role("dialog")
        // The accessibility mirror names the dialog by its own title.
        .label(dialog.title.clone())
        .w_full()
        .max_w(SHEET_WIDTH)
        .max_h(SHEET_MAX_HEIGHT)
        .shrink()
        .min_h(0.0)
        .radius(theme::RADIUS_SHEET)
        .clip_rounded(theme::RADIUS_SHEET)
        .fill(t.sheet_solid())
        .border(1.0, t.ink_at(0.14))
        .shadow(30.0, 90.0, t.bg_at(0.7))
        .child(find_line(dialog, local, !places.is_empty(), listed))
        .child(rule())
        .child(list(dialog, rows));
    if !places.is_empty() {
        sheet = sheet.child(rule()).child(places_block(dialog, &places));
    }
    sheet = sheet
        .child(rule())
        .child(footer(dialog.description.as_deref(), &dialog.instance_key));
    col()
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .center()
        .px(if local.narrow() { 8.0 } else { 18.0 })
        .py(if local.narrow() { 8.0 } else { 18.0 })
        .fill(t.bg_at(theme::SCRIM_FILL))
        .modal(true, move |app: &mut App| {
            app.ui(RyeOsUiEvent::DismissDialog {
                instance_key: close.clone(),
            });
        })
        .child(sheet)
}

/// The find line: the query at 19, and at the right how many views match.
fn find_line(dialog: &RyeOsDialogVm, local: &Local, places: bool, listed: usize) -> El {
    let t = Tokens::get();
    let mut line = row().w_full().h(FIND_HEIGHT).px(24.0).gap(2.0).center_y();
    match &dialog.input {
        Some(input) => {
            line = line.child(
                crate::shell::input_field(input, local, theme::TYPE_FIND, places)
                    .grow(1.0)
                    .min_w(0.0),
            );
        }
        None => line = line.child(el().grow(1.0)),
    }
    line.child(
        text(if listed == 1 {
            "1 view".to_owned()
        } else {
            format!("{listed} views")
        })
        .font(theme::TYPE_MICRO)
        .color(t.ink_at(0.40)),
    )
}

/// The scrolling list: a label for each group, then its rows.
fn list(dialog: &RyeOsDialogVm, rows: &[RyeOsRowVm]) -> El {
    let t = Tokens::get();
    let mut body = col()
        .key(format!("launcher:{:?}", dialog.instance_key))
        .role("list")
        .label(dialog.title.clone())
        .w_full()
        .shrink()
        .min_h(0.0)
        .pb(10.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    // Rows come grouped; each run of one group follows its label.
    let mut groups: Vec<(&str, Vec<&RyeOsRowVm>)> = Vec::new();
    for item in rows.iter().filter(|row| !is_place(row)) {
        let this = item
            .launch
            .as_ref()
            .map_or("", |launch| launch.group.as_str());
        match groups.last_mut() {
            Some((group, items)) if *group == this => items.push(item),
            _ => groups.push((this, vec![item])),
        }
    }
    for (group, items) in groups {
        if !group.is_empty() {
            body = body.child(
                row().w_full().px(24.0).pt(14.0).pb(6.0).child(
                    text(group.to_uppercase())
                        .font(theme::TYPE_LABEL)
                        .color(t.ink_at(if group == COMMANDS {
                            COMMAND_LABEL_INK
                        } else {
                            0.45
                        })),
                ),
            );
        }
        body = body.child(
            col().w_full().px(10.0).gap(2.0).children(
                items
                    .into_iter()
                    .map(|item| view_row(&dialog.instance_key, item)),
            ),
        );
    }
    body
}

/// One view: its title, its description and, when it cannot be chosen, the
/// reason beneath that, and at the right the widget it shows.
fn view_row(instance: &RyeOsViewInstanceKey, item: &RyeOsRowVm) -> El {
    let t = Tokens::get();
    let unavailable = item.kind.as_deref() == Some("unavailable");
    let reason = item
        .launch
        .as_ref()
        .and_then(|launch| launch.reason.as_deref());
    let quiet = is_command(item);
    let mut words = col().grow(1.0).min_w(0.0).gap(3.0);
    if let Some(description) = &item.secondary {
        words = words.child(
            text(description.clone())
                .font(theme::TYPE_CELL)
                .color(t.ink_at(if item.selected { 0.90 } else { 0.68 }))
                .truncate()
                .w_full(),
        );
    }
    if let Some(reason) = reason {
        words = words.child(
            text(reason.to_owned())
                .font(theme::TYPE_MICRO)
                .color(t.ink_at(0.42)),
        );
    }
    let mut line = row()
        .key(format!("item:{instance:?}:{}", item.id))
        .role("button")
        .label(item.primary.clone())
        .w_full()
        .min_h(ROW_HEIGHT)
        .px(ROW_PAD_X)
        .py(ROW_PAD_Y)
        .gap(ROW_GAP)
        .center_y()
        .radius(ROW_RADIUS)
        .disabled(unavailable)
        .focus_selected(item.selected)
        .child(
            col().w(NAME_WIDTH).child(
                text(item.primary.clone())
                    .font(theme::TYPE_TITLE)
                    .color(t.ink_at(if item.selected {
                        1.0
                    } else if quiet {
                        COMMAND_NAME_INK
                    } else {
                        0.85
                    }))
                    .truncate(),
            ),
        )
        .child(words);
    if let Some(kind) = &item.meta {
        line = line.child(
            text(kind.clone())
                .font(theme::TYPE_MICRO)
                .color(t.ink_at(0.40)),
        );
    }
    if item.selected {
        line = line.fill(t.ink_at(0.09));
    }
    if unavailable {
        return line.opacity(DIMMED);
    }
    let choose = instance.clone();
    let item_id = item.id.clone();
    line.hover(t.ink_at(0.05)).on_press(move |app: &mut App| {
        app.ui(RyeOsUiEvent::ChooseViewItem {
            instance_key: choose.clone(),
            item_id: item_id.clone(),
            activate: true,
        });
    })
}

/// Where the chosen view can land: one bordered choice each, the first
/// chosen, its key in a cap at the right.
fn places_block(dialog: &RyeOsDialogVm, places: &[&RyeOsRowVm]) -> El {
    let t = Tokens::get();
    let mut choices = row().w_full().px(24.0).pb(16.0).gap(10.0);
    for place in places {
        let chosen = place.selected;
        let instance = dialog.instance_key.clone();
        let item_id = place.id.clone();
        choices = choices.child(
            row()
                .key(format!("item:{:?}:{}", dialog.instance_key, place.id))
                .role("button")
                .label(place.primary.clone())
                .h(PLACE_HEIGHT)
                .px(10.0)
                .gap(10.0)
                .center_y()
                .radius(PLACE_RADIUS)
                .border(1.0, t.ink_at(if chosen { 0.40 } else { 0.10 }))
                .focus_selected(chosen)
                .hover(t.ink_at(0.05))
                .child(
                    text(place.primary.clone())
                        .font(if chosen {
                            theme::medium(theme::TYPE_SMALL)
                        } else {
                            theme::TYPE_SMALL
                        })
                        .color(t.ink_at(if chosen { 0.98 } else { 0.70 })),
                )
                .child(crate::marks::key_chip(
                    place.meta.as_deref().unwrap_or_default(),
                ))
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::ChooseViewItem {
                        instance_key: instance.clone(),
                        item_id: item_id.clone(),
                        activate: true,
                    });
                }),
        );
    }
    col()
        .w_full()
        .child(
            row().w_full().px(24.0).pt(14.0).pb(8.0).child(
                text("WHERE IT LANDS")
                    .font(theme::TYPE_LABEL)
                    .color(t.ink_at(0.45)),
            ),
        )
        .child(choices)
}

/// The footer: the keys, then the view's own note at the right. The Esc cap
/// closes the sheet.
fn footer(note: Option<&str>, instance: &RyeOsViewInstanceKey) -> El {
    let t = Tokens::get();
    let word = |what: &'static str| text(what).font(theme::TYPE_MICRO).color(t.ink_at(0.45));
    let close = instance.clone();
    let mut line = row()
        .w_full()
        .h(FOOT_HEIGHT)
        .px(24.0)
        .gap(18.0)
        .center_y()
        .child(
            row()
                .gap(7.0)
                .center_y()
                .child(crate::marks::key_chip("↑"))
                .child(crate::marks::key_chip("↓"))
                .child(word("choose")),
        )
        .child(
            row()
                .key(format!("dialog:{instance:?}:close"))
                .role("button")
                .label("Close")
                .gap(7.0)
                .center_y()
                .child(crate::marks::key_chip("Esc"))
                .child(word("cancel"))
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::CloseDialog {
                        instance_key: close.clone(),
                    });
                }),
        )
        .child(el().grow(1.0));
    if let Some(note) = note {
        line = line.child(
            text(note.to_owned())
                .font(theme::TYPE_MICRO)
                .color(t.ink_at(0.40))
                .truncate(),
        );
    }
    line
}
