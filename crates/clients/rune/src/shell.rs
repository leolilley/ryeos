//! The presentation shell: system bar, view-set strip, docks around the view
//! set, status bar, notices and dialogs. Layout and metrics follow the
//! browser shell's stylesheet.

use rune::{El, Key, KeyEvent, col, el, row, text, text_input};
use ryeos_client_base::ui::event::{RyeOsInputAction, RyeOsTransportFreshness};
use ryeos_client_base::ui::view_model::{
    RyeOsDialogVm, RyeOsDockTileVm, RyeOsInputVm, RyeOsLayoutNodeVm, RyeOsNoticeVm,
    RyeOsSplitAxisVm, RyeOsStatusBarVm, RyeOsTileIntentVm, RyeOsTopBarVm, RyeOsViewModel,
    RyeOsViewSetVm,
};
use ryeos_client_base::ui::{RyeOsUiEvent, RyeOsUiIntent};

use crate::theme::{self, NAV_WIDTH, Palette, STATUS_HEIGHT, STRIP_HEIGHT, SYSTEM_HEIGHT};
use crate::views;
use crate::{App, Local};

/// The page before the core exists: starting, or why it cannot.
pub fn boot_screen(failure: Option<&str>) -> El {
    let p = Palette::get();
    let (kicker, headline, color) = match failure {
        None => ("Opening presentation", "RyeOS", p.text),
        Some(_) => ("Cannot continue", "RyeOS stopped", p.red),
    };
    let mut copy = col()
        .gap(12.0)
        .max_w(544.0)
        .child(
            text(kicker.to_uppercase())
                .font(theme::ui(11.0).tracking(1.8))
                .color(p.accent_bright),
        )
        .child(
            text(headline)
                .font(rune::tok::TextStyle::new(40.0, 600.0).tracking(-1.6))
                .color(color),
        )
        .child(el().w_full().h(2.0).fill(p.line));
    copy = match failure {
        None => copy.child(
            text("Reading the session and claiming the seat.")
                .font(theme::ui(13.0))
                .color(p.yellow),
        ),
        Some(message) => copy.child(
            text(message.to_owned())
                .font(theme::ui(13.0))
                .color(p.text)
                .wrap_anywhere()
                .w_full()
                .padding(12.0)
                .border_sides([0.0, 0.0, 0.0, 2.0], p.red),
        ),
    };
    let mark = el()
        .size(112.0, 112.0)
        .border(2.0, p.accent)
        .fill(p.accent.alpha(0.14));
    row()
        .w_full()
        .h_full()
        .center()
        .gap(64.0)
        .padding(32.0)
        .fill(p.bg)
        .child(mark)
        .child(copy)
}

/// The whole presentation.
pub fn shell(vm: &RyeOsViewModel, local: &Local) -> El {
    let p = Palette::get();
    let chrome = &vm.presentation.chrome;
    let mut page = col()
        .w_full()
        .h_full()
        .fill(p.bg)
        .child(system_bar(vm))
        .child(view_set_strip(&chrome.top_bar))
        .child(body(vm, local))
        .child(status_bar(&chrome.status_bar));
    if !vm.notices.is_empty() {
        page = page.child(notices(&vm.notices));
    }
    for dialog in &vm.dialogs {
        page = page.child(dialog_layer(dialog, local));
    }
    // An open actions menu draws last, anchored to its control.
    if let Some(key) = &local.menu
        && let Some(intents) = vm
            .view_set
            .as_ref()
            .and_then(|set| set.root.as_ref())
            .and_then(|root| menu_intents(root, key))
    {
        page = page.child(views::actions_menu(key, intents));
    }
    page
}

fn system_bar(vm: &RyeOsViewModel) -> El {
    let p = Palette::get();
    let session = &vm.session;
    let mut context = row().center_y().gap(10.0).min_w(0.0).grow(1.0);
    if let Some(ordinal) = session.surface_ordinal {
        context = context.child(
            text(format!("{ordinal:02}"))
                .font(theme::data(10.0).tracking(0.4))
                .color(p.accent),
        );
    }
    context = context.child(
        text(session.title.clone())
            .font(theme::data(10.0).tracking(0.4))
            .color(p.muted)
            .truncate(),
    );
    let state = row()
        .center_y()
        .gap(10.0)
        .child(
            text(vm.chrome.health_label.to_uppercase())
                .font(theme::data(10.0).tracking(0.4))
                .color(p.muted),
        )
        .child(
            text(freshness_label(vm.transport.freshness))
                .font(theme::data(10.0).tracking(0.4))
                .color(p.muted),
        );
    row()
        .w_full()
        .h(SYSTEM_HEIGHT)
        .px(12.0)
        .gap(14.0)
        .center_y()
        .fill(p.bar)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(context)
        .child(state)
}

/// The transport freshness as its wire name, uppercased.
fn freshness_label(freshness: RyeOsTransportFreshness) -> String {
    serde_json::to_value(freshness)
        .ok()
        .and_then(|value| value.as_str().map(str::to_uppercase))
        .unwrap_or_default()
}

fn view_set_strip(top_bar: &RyeOsTopBarVm) -> El {
    let p = Palette::get();
    if !top_bar.visible {
        return el().h(0.0);
    }
    let mut strip = row()
        .w_full()
        .h(STRIP_HEIGHT)
        .px(18.0)
        .gap(8.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line);
    for tab in &top_bar.tabs {
        let (ink, ordinal_ink, rule) = if tab.active {
            (p.text, p.yellow, p.accent_bright)
        } else {
            (p.muted, p.faint, p.bar.alpha(0.0))
        };
        let view_set_id = tab.view_set_id;
        strip = strip.child(
            row()
                .key(format!("view-set:{}", tab.view_set_id))
                .role("tab")
                .label(tab.title.clone())
                .min_w(140.0)
                .h_full()
                .pl(18.0)
                .pr(12.0)
                .pt(4.0)
                .gap(17.0)
                .center_y()
                .border_sides([0.0, 0.0, 2.0, 0.0], rule)
                .hover(p.plane_selected.alpha(0.4))
                .child(
                    text(format!("{:02}", tab.number))
                        .font(theme::data(9.0))
                        .color(ordinal_ink),
                )
                .child(text(tab.title.clone()).font(theme::ui(13.0)).color(ink))
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::Activate {
                        intent: RyeOsUiIntent::SelectViewSet { view_set_id },
                    });
                }),
        );
    }
    strip
}

fn body(vm: &RyeOsViewModel, local: &Local) -> El {
    let mut body = row()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .pt(14.0)
        .pb(14.0)
        .px(18.0)
        .gap(12.0);
    if !vm.navigation.items.is_empty() {
        body = body.child(navigation(vm));
    }
    body.child(view_set(vm, local))
}

fn navigation(vm: &RyeOsViewModel) -> El {
    let p = Palette::get();
    let mut items = col().w_full().py(10.0);
    for item in &vm.navigation.items {
        let intent = item.intent.clone();
        let (ink, rule, fill) = if item.selected {
            (p.text, p.accent_bright, p.plane_selected)
        } else {
            (p.muted, p.plane.alpha(0.0), p.plane.alpha(0.0))
        };
        items = items.child(
            row()
                .key(format!("nav:{}", item.id))
                .w_full()
                .min_h(42.0)
                .pl(15.0)
                .pr(12.0)
                .center_y()
                .fill(fill)
                .border_sides([0.0, 0.0, 0.0, 2.0], rule)
                .hover(p.plane_selected.alpha(0.5))
                .child(
                    el().w(20.0)
                        .child(text("◇").font(theme::ui(13.0)).color(p.yellow)),
                )
                .child(text(item.label.clone()).font(theme::ui(13.0)).color(ink))
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::Activate {
                        intent: intent.clone(),
                    });
                }),
        );
    }
    col()
        .w(NAV_WIDTH)
        .h_full()
        .min_h(0.0)
        .fill(p.plane)
        .border(1.0, p.line)
        .child(
            row()
                .w_full()
                .h(40.0)
                .px(14.0)
                .center_y()
                .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
                .child(
                    text("EXPLORER")
                        .font(theme::data(9.0).tracking(1.26))
                        .color(p.muted)
                        .grow(1.0),
                )
                .child(
                    text(format!("{:02}", vm.navigation.items.len()))
                        .font(theme::data(9.0).tracking(1.26))
                        .color(p.muted),
                ),
        )
        .child(items)
}

fn view_set(vm: &RyeOsViewModel, local: &Local) -> El {
    let maximized = matches!(
        vm.view_set.as_ref().and_then(|set| set.root.as_ref()),
        Some(RyeOsLayoutNodeVm::Tile {
            maximized: true,
            ..
        })
    );
    let docks = &vm.docks;
    let gap = if maximized { 0.0 } else { 12.0 };
    let mut middle = row().w_full().grow(1.0).min_h(0.0).min_w(0.0).gap(gap);
    if !maximized && let Some(left) = &docks.left {
        middle = middle.child(dock(left, true, local));
    }
    middle = middle.child(center(vm.view_set.as_ref(), local));
    if !maximized && let Some(right) = &docks.right {
        middle = middle.child(dock(right, true, local));
    }
    let mut set = col().grow(1.0).h_full().min_w(0.0).min_h(0.0).gap(gap);
    if !maximized && let Some(top) = &docks.top {
        set = set.child(dock(top, false, local));
    }
    set = set.child(middle);
    if !maximized && let Some(bottom) = &docks.bottom {
        set = set.child(dock(bottom, false, local));
    }
    set
}

fn dock(tile: &RyeOsDockTileVm, side: bool, local: &Local) -> El {
    let supplement = tile.supplement.as_ref();
    let edge = tile.edge;
    let focused = tile.focused;
    let frame = views::frame(
        views::FrameSpec {
            key: format!("dock:{:?}", tile.edge),
            instance: &tile.instance_key,
            label: supplement
                .map(|s| s.frame_label.as_str())
                .filter(|label| !label.is_empty())
                .unwrap_or(&tile.title),
            detail: supplement
                .map(|s| s.frame_detail.as_str())
                .filter(|detail| !detail.is_empty()),
            attachment: tile.attachment_label.as_deref(),
            focused,
            transparent: false,
            chrome_hidden: false,
            tile: None,
            dock_edge: Some(format!("{:?}", tile.edge).to_lowercase()),
            heading: tile.heading.as_ref(),
            supplement,
            view: &tile.view,
            input: tile.input.as_ref(),
        },
        local,
    );
    let frame = if focused {
        frame
    } else {
        frame.on_press(move |app: &mut App| app.ui(RyeOsUiEvent::FocusDock { edge }))
    };
    let size = f32::from(tile.size);
    if side {
        col().w(size).h_full().min_h(0.0).child(frame)
    } else {
        col().w_full().h(size).min_w(0.0).child(frame)
    }
}

/// The layout, or the bare backdrop when the set has none.
fn center(view_set: Option<&RyeOsViewSetVm>, local: &Local) -> El {
    let content = match view_set.and_then(|set| set.root.as_ref()) {
        Some(root) => layout(root, local),
        None => el(),
    };
    col()
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .child(content)
}

fn layout(node: &RyeOsLayoutNodeVm, local: &Local) -> El {
    let p = Palette::get();
    match node {
        RyeOsLayoutNodeVm::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = ratio.clamp(0.0, 1.0);
            let first = col()
                .grow(ratio)
                .min_w(0.0)
                .min_h(0.0)
                .child(layout(first, local));
            let second = col()
                .grow(1.0 - ratio)
                .min_w(0.0)
                .min_h(0.0)
                .child(layout(second, local));
            match axis {
                RyeOsSplitAxisVm::Horizontal => row()
                    .w_full()
                    .h_full()
                    .min_w(0.0)
                    .min_h(0.0)
                    .gap(8.0)
                    .child(first)
                    .child(el().w(1.0).h_full().fill(p.line).role("separator"))
                    .child(second),
                RyeOsSplitAxisVm::Vertical => col()
                    .w_full()
                    .h_full()
                    .min_w(0.0)
                    .min_h(0.0)
                    .gap(8.0)
                    .child(first)
                    .child(el().w_full().h(1.0).fill(p.line).role("separator"))
                    .child(second),
            }
        }
        RyeOsLayoutNodeVm::Tile {
            group_label,
            tabs,
            tile_id,
            instance_key,
            focused,
            maximized,
            title,
            attachment_label,
            heading,
            supplement,
            intents,
            view,
            chrome_hidden,
            background_transparent,
            input,
            ..
        } => {
            let supplement = supplement.as_ref();
            let label = supplement
                .map(|s| s.frame_label.as_str())
                .filter(|label| !label.is_empty())
                .or(group_label.as_deref())
                .unwrap_or(title);
            let frame = views::frame(
                views::FrameSpec {
                    key: format!("tile:{tile_id}"),
                    instance: instance_key,
                    label,
                    detail: supplement
                        .map(|s| s.frame_detail.as_str())
                        .filter(|detail| !detail.is_empty()),
                    attachment: attachment_label.as_deref(),
                    focused: *focused,
                    transparent: *background_transparent,
                    chrome_hidden: *chrome_hidden,
                    tile: Some(views::TileChrome {
                        tile_id,
                        maximized: *maximized,
                        intents,
                        tabs,
                        grouped: group_label.is_some(),
                    }),
                    dock_edge: None,
                    heading: heading.as_ref(),
                    supplement,
                    view,
                    input: input.as_ref(),
                },
                local,
            );
            if *focused {
                frame
            } else {
                let target = tile_id.clone();
                frame.on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::FocusChanged {
                        target: Some(target.clone()),
                    });
                })
            }
        }
    }
}

/// The intents of the mounted tile whose actions menu key is `key`.
fn menu_intents<'a>(node: &'a RyeOsLayoutNodeVm, key: &str) -> Option<&'a [RyeOsTileIntentVm]> {
    match node {
        RyeOsLayoutNodeVm::Split { first, second, .. } => {
            menu_intents(first, key).or_else(|| menu_intents(second, key))
        }
        RyeOsLayoutNodeVm::Tile {
            instance_key,
            intents,
            ..
        } => (views::menu_key(instance_key) == key).then_some(intents.as_slice()),
    }
}

fn status_bar(model: &RyeOsStatusBarVm) -> El {
    let p = Palette::get();
    if !model.visible {
        return el().h(0.0);
    }
    let mut bar = row()
        .w_full()
        .h(STATUS_HEIGHT)
        .px(18.0)
        .gap(14.0)
        .center_y()
        .fill(p.bar)
        .border_sides([1.0, 0.0, 0.0, 0.0], p.line);
    for segment in &model.segments {
        let label = match segment.label.as_deref().filter(|label| !label.is_empty()) {
            Some(label) => format!("{label} {}", segment.value),
            None => segment.value.clone(),
        };
        let mut part = text(label.to_uppercase())
            .font(theme::data(9.0))
            .color(p.tone(segment.tone, p.muted));
        if segment.grow {
            part = part.grow(1.0);
        }
        bar = bar.child(part);
    }
    bar.child(el().grow(1.0))
        .child(
            text(model.key_hint.to_uppercase())
                .font(theme::data(9.0))
                .color(p.faint),
        )
        .child(
            row()
                .key("shell:launch")
                .role("button")
                .label("Open the launcher")
                .h_full()
                .px(10.0)
                .center_y()
                .fill(p.accent_bright)
                .child(text("RYEOS").font(theme::data(9.0)).color(p.on_accent))
                .on_press(|app: &mut App| app.ui(RyeOsUiEvent::OpenLauncher)),
        )
}

fn notices(notices: &[RyeOsNoticeVm]) -> El {
    let p = Palette::get();
    let mut stack = col()
        .pin(None, None, Some(20.0), Some(STATUS_HEIGHT + 18.0))
        .w(390.0)
        .gap(8.0);
    for notice in notices {
        let id = notice.id.clone();
        stack = stack.child(
            row()
                .key(format!("notice:{}", notice.id))
                .w_full()
                .padding(12.0)
                .gap(12.0)
                .fill(p.plane_raised)
                .border(1.0, p.line_strong)
                .border_sides([1.0, 1.0, 1.0, 3.0], p.notice_rule(notice.tone))
                .shadow(9.0, 30.0, rune::Color::BLACK.alpha(0.45))
                .child(
                    text(notice.message.clone())
                        .font(theme::ui(12.0))
                        .color(p.text)
                        .grow(1.0),
                )
                .child(
                    text("×")
                        .font(theme::ui(17.0))
                        .color(p.muted)
                        .key(format!("notice:{}:dismiss", notice.id))
                        .role("button")
                        .label("Dismiss")
                        .on_press(move |app: &mut App| {
                            app.ui(RyeOsUiEvent::DismissNotice { id: id.clone() });
                        }),
                ),
        );
    }
    stack
}

fn dialog_layer(dialog: &RyeOsDialogVm, local: &Local) -> El {
    let p = Palette::get();
    let close = dialog.instance_key.clone();
    let close_button = close.clone();
    let mut content = col().w_full().shrink().min_h(0.0).padding(12.0).gap(10.0);
    let filter = dialog.input.as_ref().filter(|input| input.live_filter);
    if let Some(input) = filter {
        content = content.child(composer(input, local));
    }
    content = content.child(
        col()
            .w_full()
            .shrink()
            .min_h(0.0)
            .child(views::body(&dialog.view, &dialog.instance_key)),
    );
    if let Some(input) = dialog.input.as_ref().filter(|input| !input.live_filter) {
        content = content.child(composer(input, local));
    }
    let header = row()
        .w_full()
        .h(34.0)
        .px(12.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(
            text("■")
                .font(theme::data(8.0))
                .color(p.accent_bright)
                .pr(10.0),
        )
        .child(
            text(dialog.title.to_uppercase())
                .font(theme::data(10.0).tracking(0.8))
                .color(p.text)
                .truncate()
                .grow(1.0),
        )
        .child(
            text("×")
                .font(theme::ui(18.0))
                .color(p.muted)
                .px(6.0)
                .hover(p.plane_raised)
                .key(format!("dialog:{:?}:close", dialog.instance_key))
                .role("button")
                .label("Close")
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::CloseDialog {
                        instance_key: close_button.clone(),
                    });
                }),
        );
    let panel = col()
        .key(format!("dialog:{:?}", dialog.instance_key))
        .w_full()
        .max_w(720.0)
        .max_h(640.0)
        .shrink()
        .min_h(0.0)
        .fill(p.plane)
        .border(1.0, p.line_strong)
        .border_sides([1.0, 1.0, 1.0, 1.0], p.line_strong)
        .child(el().w_full().h(1.0).fill(p.accent_bright))
        .child(header)
        .child(content);
    col()
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .items_center()
        .pt(96.0)
        .px(18.0)
        .pb(18.0)
        .fill(p.bg.alpha(0.72))
        .modal(true, move |app: &mut App| {
            app.ui(RyeOsUiEvent::CloseDialog {
                instance_key: close.clone(),
            });
        })
        .child(panel)
}

/// The input composer: route strip, the text field bound to the core's
/// buffer, and the submit control. A live filter is the compact form above
/// its view: Up and Down move the selection, Enter activates it.
pub fn composer(input: &RyeOsInputVm, local: &Local) -> El {
    let p = Palette::get();
    let key = crate::input_key(&input.address);
    let edit_key = key.clone();
    let change_address = input.address.clone();
    let key_address = input.address.clone();
    let button_address = input.address.clone();
    let live = input.live_filter;
    let submit_enabled = input.submit_enabled;
    let focused = input.focused;
    let field_font = if live {
        theme::data(11.0)
    } else {
        theme::ui(13.0)
    };
    let mut field = text_input(&local.edits.edit(&key), field_font)
        .key(format!("composer:{key}"))
        .placeholder(input.placeholder.clone(), p.faint)
        .color(p.text)
        .w_full()
        .on_edit(move |app: &mut App| app.edit_mut(&edit_key))
        .on_input_change(move |app: &mut App, _tx| {
            let Some((text, cursor)) = app.edit_state(&crate::input_key(&change_address)) else {
                return;
            };
            if !focused {
                app.ui(RyeOsUiEvent::InputAt {
                    address: change_address.clone(),
                    action: RyeOsInputAction::Focus,
                });
            }
            app.ui(RyeOsUiEvent::InputAt {
                address: change_address.clone(),
                action: RyeOsInputAction::SetText { text, cursor },
            });
        })
        .on_input_key(move |app: &mut App, event: &KeyEvent, _tx| {
            let action = match &event.key {
                Key::Up if live => RyeOsInputAction::NavigateSelection { delta: -1 },
                Key::Down if live => RyeOsInputAction::NavigateSelection { delta: 1 },
                Key::Enter if live && !event.mods.shift => RyeOsInputAction::ActivateSelection,
                // Shift+Enter is a newline; Alt+Enter submits as an interrupt.
                Key::Enter if !event.mods.shift => {
                    if !submit_enabled {
                        return true;
                    }
                    RyeOsInputAction::Submit {
                        interrupt: event.mods.alt,
                    }
                }
                _ => return false,
            };
            if !focused {
                app.ui(RyeOsUiEvent::InputAt {
                    address: key_address.clone(),
                    action: RyeOsInputAction::Focus,
                });
            }
            app.ui(RyeOsUiEvent::InputAt {
                address: key_address.clone(),
                action,
            });
            true
        });
    field = if live {
        field.h(38.0).px(14.0).py(10.0)
    } else {
        field.min_h(54.0).padding(14.0)
    };
    let mut composer = col();
    composer = if live {
        composer
            .w_full()
            .fill(p.plane.alpha(0.97))
            .border_sides([0.0, 0.0, 1.0, 0.0], p.line_strong)
    } else {
        composer
            .fill(p.plane)
            .border(1.0, p.line_strong)
            .mt(8.0)
            .mx(20.0)
            .mb(18.0)
    };
    if !live {
        composer = composer.child(
            row()
                .w_full()
                .h(30.0)
                .px(12.0)
                .gap(8.0)
                .center_y()
                .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
                .child(text("●").font(theme::data(9.0)).color(p.green))
                .child(
                    text(input.route_label.clone())
                        .font(theme::data(9.0))
                        .color(p.muted)
                        .truncate()
                        .grow(1.0),
                )
                .child(text("DRAFT").font(theme::data(9.0)).color(p.faint)),
        );
    }
    composer = composer.child(field);
    let mut actions = row().w_full().center_y().child(
        text(input.hint.clone())
            .font(theme::ui(10.0))
            .color(p.faint)
            .grow(1.0),
    );
    actions = if live {
        actions.min_h(24.0).px(14.0).pb(6.0)
    } else {
        actions.min_h(34.0).pl(14.0).pr(10.0).pb(7.0)
    };
    if !live {
        let mut send = el()
            .key(format!("composer:{key}:submit"))
            .role("button")
            .label("Send")
            .size(27.0, 27.0)
            .center()
            .fill(p.accent)
            .disabled(!submit_enabled)
            .child(text("↑").font(theme::ui(13.0)).color(p.text));
        send = if submit_enabled {
            send.on_press(move |app: &mut App| {
                app.ui(RyeOsUiEvent::InputAt {
                    address: button_address.clone(),
                    action: RyeOsInputAction::Submit { interrupt: false },
                });
            })
        } else {
            send.opacity(0.5)
        };
        actions = actions.child(send);
    }
    composer.child(actions)
}
