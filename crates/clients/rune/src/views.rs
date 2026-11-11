//! View bodies, tile frames and key translation.
//!
//! Every view kind the core projects is matched here without a wildcard, so a
//! new kind fails to compile until this renderer decides how to draw it.

use rune::{El, Key, KeyEvent, ScrollSpec, col, el, row, text};
use ryeos_client_base::ids::RyeOsViewInstanceKey;
use ryeos_client_base::ui::view_model::{
    RyeOsInputVm, RyeOsRowDetailVm, RyeOsRowVm, RyeOsSectionVm, RyeOsSourceStatusVm, RyeOsViewVm,
};
use ryeos_client_base::ui::{RyeOsKey, RyeOsKeyEvent, RyeOsKeyModifiers, RyeOsUiEvent};

use crate::App;
use crate::theme::{self, HEADER_HEIGHT, Palette};

/// Translate a Rune key into the core's key vocabulary. Keys the core has no
/// name for stay with Rune.
pub fn key_event(event: &KeyEvent) -> Option<RyeOsKeyEvent> {
    let key = match &event.key {
        Key::Text(typed) => {
            let mut chars = typed.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                return None;
            };
            RyeOsKey::Char(ch)
        }
        Key::Command(ch) => RyeOsKey::Char(*ch),
        Key::Enter => RyeOsKey::Enter,
        Key::Escape => RyeOsKey::Escape,
        Key::Backspace => RyeOsKey::Backspace,
        Key::Tab => RyeOsKey::Tab,
        Key::Up => RyeOsKey::ArrowUp,
        Key::Down => RyeOsKey::ArrowDown,
        Key::Left => RyeOsKey::ArrowLeft,
        Key::Right => RyeOsKey::ArrowRight,
        _ => return None,
    };
    Some(RyeOsKeyEvent {
        key,
        modifiers: RyeOsKeyModifiers {
            ctrl: event.mods.ctrl,
            alt: event.mods.alt,
            shift: event.mods.shift,
            meta: event.mods.logo,
        },
    })
}

/// The uppercase data-face label used by tile and dock headers.
pub fn header_label(label: &str, color: rune::Color) -> El {
    text(label.to_uppercase())
        .font(theme::data(9.0).tracking(1.0))
        .color(color)
        .truncate()
}

/// A bordered frame with a header row and a body, as tiles and docks draw.
pub fn frame(
    key: String,
    title: &str,
    detail: Option<&str>,
    focused: bool,
    body: El,
    input: Option<&RyeOsInputVm>,
    edits: &crate::Edits,
) -> El {
    let p = Palette::get();
    let signal = el()
        .size(if focused { 13.0 } else { 5.0 }, 5.0)
        .fill(p.accent_bright);
    let mut identity = row()
        .center_y()
        .gap(10.0)
        .min_w(0.0)
        .child(signal)
        .child(header_label(title, p.text));
    if let Some(detail) = detail {
        identity = identity.child(
            text(detail.to_owned())
                .font(theme::data(9.0))
                .color(p.muted)
                .truncate(),
        );
    }
    let header = row()
        .w_full()
        .h(HEADER_HEIGHT)
        .px(12.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(identity);
    let mut frame = col()
        .key(key)
        .w_full()
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .fill(p.plane)
        .border(1.0, if focused { p.accent } else { p.line })
        .child(header)
        .child(col().w_full().grow(1.0).min_h(0.0).child(body));
    if let Some(input) = input {
        frame = frame.child(crate::shell::composer(input, edits));
    }
    frame
}

/// The body of one mounted view instance.
pub fn body(view: &RyeOsViewVm, instance: &RyeOsViewInstanceKey) -> El {
    let p = Palette::get();
    match view {
        RyeOsViewVm::Placeholder { title, message } => empty_state(title, message),
        RyeOsViewVm::RequiredSubject { title, input, .. } => empty_state(
            title,
            &format!("This view needs a {input} before it can show anything."),
        ),
        RyeOsViewVm::Rows {
            title,
            source_status,
            rows,
            ..
        } => rows_view(instance, title, source_status.as_ref(), rows),
        RyeOsViewVm::Sections {
            title, sections, ..
        } => sections_view(instance, title, sections),
        RyeOsViewVm::Field { .. } => not_ported("field graph"),
        RyeOsViewVm::Text { .. } => not_ported("text"),
        RyeOsViewVm::Document { .. } => not_ported("document"),
        RyeOsViewVm::Timeline { .. } => not_ported("timeline"),
        RyeOsViewVm::Table { .. } => not_ported("table"),
    }
    .fill(p.plane)
}

/// A scrolling list body with the list views' inset.
fn list(key: String, label: &str) -> El {
    col()
        .key(key)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(16.0)
        .py(12.0)
        .label(label.to_owned())
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
}

fn rows_view(
    instance: &RyeOsViewInstanceKey,
    title: &str,
    status: Option<&RyeOsSourceStatusVm>,
    rows: &[RyeOsRowVm],
) -> El {
    let mut body = list(format!("rows:{instance:?}"), title).role("list");
    if let Some(status) = status {
        body = body.child(source_status(status));
    }
    for item in rows {
        body = body.child(record(instance, item, RowStyle::Rows));
    }
    body
}

fn sections_view(instance: &RyeOsViewInstanceKey, title: &str, sections: &[RyeOsSectionVm]) -> El {
    let mut body = list(format!("sections:{instance:?}"), title);
    for section in sections {
        body = body.child(section_block(instance, section));
    }
    body
}

fn section_block(instance: &RyeOsViewInstanceKey, section: &RyeOsSectionVm) -> El {
    let p = Palette::get();
    let count = if section.source_status.is_some() {
        "—".to_owned()
    } else {
        format!("{:02}", section.count)
    };
    let ink = if section.header_selected {
        p.yellow
    } else {
        p.text
    };
    let fold = if section.collapsed { "▸" } else { "▾" };
    let toggle_key = instance.clone();
    let section_id = section.id.clone();
    let header = row()
        .key(format!("section:{instance:?}:{}", section.id))
        .w_full()
        .min_h(40.0)
        .px(10.0)
        .center_y()
        .gap(10.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line_strong)
        .hover(p.plane_raised)
        .role("button")
        .expanded(!section.collapsed)
        .child(
            text(format!("{fold} {}", section.title))
                .font(theme::ui(13.0))
                .color(ink)
                .truncate()
                .grow(1.0),
        )
        .child(text(count).font(theme::ui(13.0)).color(ink))
        .on_press(move |app: &mut App| {
            app.ui(RyeOsUiEvent::ToggleViewSection {
                instance_key: toggle_key.clone(),
                section_id: section_id.clone(),
            });
        });
    let mut block = col().w_full().child(header);
    if let Some(status) = &section.source_status {
        block = block.child(source_status(status));
    }
    if !section.collapsed {
        for item in &section.rows {
            block = block.child(record(instance, item, RowStyle::Section));
        }
    }
    block
}

#[derive(Clone, Copy, PartialEq)]
enum RowStyle {
    /// Glyph, primary over secondary, meta at the right.
    Rows,
    /// Primary with the meta (or secondary) beside it.
    Section,
}

/// One list record: the choosable line, its disclosure, and its details when
/// expanded. A `detail` record is only details.
fn record(instance: &RyeOsViewInstanceKey, item: &RyeOsRowVm, style: RowStyle) -> El {
    let p = Palette::get();
    let wrapper = col()
        .key(format!("item:{instance:?}:{}", item.id))
        .w_full()
        .min_w(0.0)
        .role("listitem")
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line);
    if style == RowStyle::Rows && item.kind.as_deref() == Some("detail") {
        return wrapper.child(details(&item.detail));
    }
    let unavailable = item.kind.as_deref() == Some("unavailable");
    let ink = p.tone(item.tone, p.text);
    let mut line = row()
        .grow(1.0)
        .min_w(0.0)
        .center_y()
        .role("button")
        .label(item.primary.clone())
        .disabled(unavailable)
        .focus_selected(item.selected);
    if item.selected {
        line = line.fill(p.plane_selected);
    }
    line = match style {
        RowStyle::Rows => {
            let mut copy = col().grow(1.0).min_w(0.0).gap(3.0).child(
                text(item.primary.clone())
                    .font(rune::tok::TextStyle::new(13.0, 450.0).family(theme::UI))
                    .color(ink)
                    .truncate(),
            );
            if let Some(secondary) = &item.secondary {
                copy = copy.child(
                    text(secondary.clone())
                        .font(theme::ui(10.0))
                        .color(p.muted)
                        .truncate(),
                );
            }
            let mut line = line
                .min_h(43.0)
                .px(10.0)
                .py(7.0)
                .child(
                    el().w(24.0).child(
                        text(item.glyph.clone().unwrap_or_else(|| "◇".to_owned()))
                            .font(theme::ui(13.0))
                            .color(p.yellow),
                    ),
                )
                .child(copy);
            if let Some(meta) = &item.meta {
                line = line.child(text(meta.clone()).font(theme::ui(10.0)).color(p.muted));
            }
            line
        }
        RowStyle::Section => {
            let aside = item.meta.as_ref().or(item.secondary.as_ref());
            let mut line = line.min_h(43.0).px(10.0).py(7.0).gap(10.0).child(
                text(item.primary.clone())
                    .font(theme::ui(13.0))
                    .color(ink)
                    .truncate()
                    .grow(1.0),
            );
            if let Some(aside) = aside {
                line = line.child(text(aside.clone()).font(theme::ui(10.0)).color(p.muted));
            }
            line
        }
    };
    if unavailable {
        line = line.opacity(0.5);
    } else {
        let choose_key = instance.clone();
        let item_id = item.id.clone();
        line = line.hover(p.plane_raised).on_press(move |app: &mut App| {
            app.ui(RyeOsUiEvent::ChooseViewItem {
                instance_key: choose_key.clone(),
                item_id: item_id.clone(),
                activate: true,
            });
        });
    }
    let mut head = row().w_full().min_w(0.0).child(line);
    if item.expandable {
        let expand_key = instance.clone();
        let item_id = item.id.clone();
        let expanded = item.expanded;
        head = head.child(
            col()
                .w(34.0)
                .center()
                .border_sides([0.0, 0.0, 0.0, 1.0], p.line)
                .fill(if expanded { p.plane_selected } else { p.plane })
                .hover(p.plane_selected)
                .role("button")
                .label(format!(
                    "{} {}",
                    if expanded { "Collapse" } else { "Expand" },
                    item.primary
                ))
                .expanded(expanded)
                .child(
                    text(if expanded { "▾" } else { "▸" })
                        .font(theme::ui(13.0))
                        .color(if expanded { p.yellow } else { p.muted }),
                )
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::ToggleViewItemExpansion {
                        instance_key: expand_key.clone(),
                        item_id: item_id.clone(),
                        expand: !expanded,
                    });
                }),
        );
    }
    let mut wrapper = wrapper.child(head);
    if item.expanded {
        wrapper = wrapper.child(details(&item.detail));
    }
    wrapper
}

/// Field/value pairs under an expanded record.
fn details(details: &[RyeOsRowDetailVm]) -> El {
    let p = Palette::get();
    let mut block = col()
        .w_full()
        .pt(7.0)
        .pb(9.0)
        .pl(34.0)
        .pr(12.0)
        .fill(p.plane_raised.alpha(0.45));
    for detail in details {
        let field = if detail.field.is_empty() {
            "detail"
        } else {
            &detail.field
        };
        let value_ink = detail.tone.map_or(p.muted, |tone| p.tone(tone, p.muted));
        block = block.child(
            row()
                .w_full()
                .min_w(0.0)
                .gap(12.0)
                .py(2.0)
                .child(
                    text(field.to_owned())
                        .font(theme::data(9.0))
                        .color(p.faint)
                        .wrap_anywhere()
                        .w(110.0),
                )
                .child(
                    text(detail.value.clone())
                        .font(theme::data(9.0))
                        .color(value_ink)
                        .wrap_anywhere()
                        .grow(1.0)
                        .min_w(0.0),
                ),
        );
    }
    block
}

/// The admitted source's state when it is not simply live.
fn source_status(status: &RyeOsSourceStatusVm) -> El {
    let p = Palette::get();
    let stale = if status.stale {
        "Last observation (stale) · "
    } else {
        ""
    };
    text(format!("{stale}{}: {}", status.state, status.message))
        .font(theme::data(11.0))
        .color(p.muted)
        .wrap_anywhere()
        .px(9.0)
        .py(6.0)
        .role("status")
}

fn empty_state(title: &str, message: &str) -> El {
    let p = Palette::get();
    col()
        .w_full()
        .grow(1.0)
        .min_h(160.0)
        .center()
        .gap(12.0)
        .padding(24.0)
        .child(text("◇").font(theme::ui(24.0)).color(p.yellow))
        .child(text(title.to_owned()).font(theme::ui(13.0)).color(p.text))
        .child(
            text(message.to_owned())
                .font(theme::ui(13.0))
                .color(p.muted)
                .max_w(360.0),
        )
}

/// A view kind this renderer does not draw yet. Visible on purpose, so a
/// missing piece is never mistaken for an empty source.
fn not_ported(kind: &str) -> El {
    let p = Palette::get();
    col()
        .w_full()
        .grow(1.0)
        .min_h(80.0)
        .center()
        .padding(16.0)
        .child(
            text(format!("{kind} view: not drawn by the Rune renderer yet"))
                .font(theme::data(10.0))
                .color(p.faint),
        )
}
