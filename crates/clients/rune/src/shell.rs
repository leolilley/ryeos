//! The presentation shell: system bar, view-set strip, the view
//! set, status bar, notices and dialogs. Layout and metrics follow the
//! browser shell's stylesheet.

use rune::{El, Key, KeyEvent, ScrollSpec, col, el, row, text, text_input};
use ryeos_client_base::layout::SplitBranch;
use ryeos_client_base::ui::event::{RyeOsInputAction, RyeOsTransportFreshness};
use ryeos_client_base::ui::view_model::{
    RyeOsChipMarkVm, RyeOsDialogVm, RyeOsEdgeVm, RyeOsInputVm, RyeOsLayoutNodeVm, RyeOsNoticeVm,
    RyeOsSplitAxisVm, RyeOsSurfaceChoiceVm, RyeOsTileIntentVm, RyeOsTone, RyeOsTopBarVm,
    RyeOsViewModel, RyeOsViewSetVm,
};
use ryeos_client_base::ui::{RyeOsUiEvent, RyeOsUiIntent};

use ryeos_client_runtime::documents::{DocumentOffer, DocumentState};

use crate::theme::{self, Palette, Tokens};
use crate::views;
use crate::{App, Local, RenameTarget};

/// The page before the core exists: starting, or why it cannot.
pub fn boot_screen(failure: Option<&str>) -> El {
    let p = Palette::get();
    match failure {
        None => quiet_frame(
            "Opening presentation",
            "Reading the session and claiming the seat.",
            p.accent,
        ),
        Some(message) => {
            let frame = frame_column("RyeOS stopped", p.red, p.red);
            centered(
                frame.child(
                    text(message.to_owned())
                        .font(theme::data(12.0))
                        .color(p.text)
                        .wrap_anywhere()
                        .w_full(),
                ),
            )
        }
    }
}

/// A full-page wait in the boot screen's frame: the wordmark, one headline,
/// a hairline with `accent` on its leading edge, and a muted detail.
pub fn quiet_frame(headline: &str, detail: &str, accent: rune::Color) -> El {
    let p = Palette::get();
    centered(
        frame_column(headline, p.text, accent).child(
            text(detail.to_owned())
                .font(theme::ui(12.0))
                .color(p.muted)
                .role("status"),
        ),
    )
}

pub(crate) fn frame_column(headline: &str, ink: rune::Color, accent: rune::Color) -> El {
    let p = Palette::get();
    let mark = row()
        .gap(10.0)
        .center_y()
        .child(el().size(12.0, 12.0).border(1.0, accent))
        .child(
            text("RYEOS")
                .font(theme::data(10.0).tracking(3.0))
                .color(p.muted),
        );
    let rule = row()
        .w_full()
        .h(1.0)
        .fill(p.line)
        .child(el().w(48.0).h(1.0).fill(accent));
    col()
        .gap(16.0)
        .w_full()
        .max_w(440.0)
        .child(mark)
        .child(
            text(headline.to_owned())
                .font(rune::tok::TextStyle::new(22.0, 400.0).tracking(-0.4))
                .color(ink),
        )
        .child(rule)
}

pub(crate) fn centered(content: El) -> El {
    let p = Palette::get();
    row()
        .w_full()
        .h_full()
        .center()
        .padding(32.0)
        .fill(p.bg)
        .child(content)
}

/// The whole presentation.
pub fn shell(
    vm: &RyeOsViewModel,
    local: &Local,
    documents: &[(DocumentOffer, Option<String>)],
) -> El {
    let p = Palette::get();
    let chrome = &vm.presentation.chrome;
    // The body keeps its own 18 side padding; the 44 here brings the tiles to
    // 62 from the left edge, clear of the Surfaces switcher.
    let renaming_surface = matches!(
        &local.rename,
        Some(crate::Rename {
            target: RenameTarget::Surface(_),
            ..
        })
    );
    // While a Surface is renamed the switcher widens to hold the form; the body
    // moves over for that moment rather than sit under it.
    let room = if renaming_surface {
        theme::SWITCHER_INSET + theme::SWITCHER_RENAME_WIDTH
    } else {
        theme::SWITCHER_ROOM
    };
    let edges = Edges::of(vm);
    let content = col()
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .child(body(vm, local, edges));
    // A switcher at the side takes its room on that side; along the top it
    // takes its own band, and hidden it takes nothing.
    let mut main = row().w_full().grow(1.0).min_h(0.0);
    main = match edges.switcher {
        RyeOsEdgeVm::Left => main.child(el().w(room).h_full()).child(content),
        RyeOsEdgeVm::Right => main.child(content).child(el().w(room).h_full()),
        RyeOsEdgeVm::Top | RyeOsEdgeVm::Bottom | RyeOsEdgeVm::Hidden => main.child(content),
    };
    let mut page = col()
        .w_full()
        .h_full()
        .fill(p.bg)
        .on_layout(|app: &mut App, rect: [f32; 4]| app.set_viewport(rect[2], rect[3]));
    // The side switcher is pinned, so its place in the child list does not
    // move it on screen; first here, keyboard focus starts at the open
    // Surfaces.
    if edges.status_top {
        page = page.child(status_bar(vm, edges, local.narrow()));
    }
    page = match edges.switcher {
        RyeOsEdgeVm::Left | RyeOsEdgeVm::Right => page.child(system_bar(vm, local, edges)),
        RyeOsEdgeVm::Top => page.child(switcher_band(vm, local)),
        RyeOsEdgeVm::Bottom | RyeOsEdgeVm::Hidden => page,
    };
    page = page
        .child(view_set_strip(&chrome.top_bar, local, edges))
        .child(main);
    if !edges.status_top {
        page = page.child(status_bar(vm, edges, local.narrow()));
    }
    if let Some(dot) = hidden_health_dot(vm) {
        page = page.child(dot);
    }
    if !vm.notices.is_empty() || !documents.is_empty() {
        page = page.child(notices(&vm.notices, documents, edges));
    }
    for dialog in &vm.dialogs {
        page = page.child(dialog_layer(dialog, local));
    }
    // The overlay draws over everything but an actions menu.
    if vm.overlay.open {
        page = page.child(crate::overlay::layer(vm, local));
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

/// Where the switcher and the status bar sit, as this frame draws them.
#[derive(Clone, Copy)]
struct Edges {
    /// The switcher's edge; `Bottom` is not a switcher edge and draws as
    /// hidden.
    switcher: RyeOsEdgeVm,
    /// The status bar is drawn, at the top.
    status_top: bool,
    /// The status bar is drawn, at the bottom.
    status_bottom: bool,
}

impl Edges {
    fn of(vm: &RyeOsViewModel) -> Self {
        let bar = &vm.presentation.chrome.status_bar;
        let shown = bar.visible && bar.edge != RyeOsEdgeVm::Hidden;
        Edges {
            switcher: vm.presentation.chrome.switcher.edge,
            status_top: shown && bar.edge == RyeOsEdgeVm::Top,
            status_bottom: shown && bar.edge != RyeOsEdgeVm::Top,
        }
    }

    /// Room the side switcher leaves at the left and the right of a band
    /// that runs under it.
    fn side_room(self) -> (f32, f32) {
        match self.switcher {
            RyeOsEdgeVm::Left => (theme::SWITCHER_ROOM, 0.0),
            RyeOsEdgeVm::Right => (0.0, theme::SWITCHER_ROOM),
            RyeOsEdgeVm::Top | RyeOsEdgeVm::Bottom | RyeOsEdgeVm::Hidden => (0.0, 0.0),
        }
    }
}

/// The Surfaces switcher at a side: a column down the left or right edge, one
/// chip per open Surface and a plus beneath. It floats over the frame's
/// corner.
fn system_bar(vm: &RyeOsViewModel, local: &Local, edges: Edges) -> El {
    let t = Tokens::get();
    let session = &vm.session;
    // A rename needs the room for its field; the column widens for it.
    let renaming = matches!(
        &local.rename,
        Some(crate::Rename {
            target: RenameTarget::Surface(_),
            ..
        })
    );
    // look: the switcher's open state with the Surfaces' names (spec section
    // 11, "Bar, open") is not built; the bar stays the narrow chip column.
    // look: the switcher and the sheet carry no shadow; no shadow call is used
    // in this crate.
    let mut chips = col()
        .w_full()
        .gap(6.0)
        .min_h(0.0)
        .role("tablist")
        .label("Open Surfaces")
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    for surface in &session.surfaces {
        chips = chips.child(surface_entry(surface, local));
    }
    // With no open Surface the session's initial stands in, as the old bar
    // showed the session's title.
    if session.surfaces.is_empty() {
        let initial = crate::marks::initial_of(&session.title);
        chips = chips.child(
            col()
                .size(theme::SWITCHER_CHIP, theme::SWITCHER_CHIP)
                .center()
                .radius(theme::RADIUS_ROW)
                .border(1.0, t.ink_at(0.14))
                .role("tab")
                .label(session.title.clone())
                .child(
                    text(initial)
                        .font(theme::medium(theme::TYPE_MICRO))
                        .color(t.ink_at(0.55)),
                ),
        );
    }
    // The column is bounded by a pin on top and bottom (clear of the status
    // bar), so a long list scrolls instead of growing over the body.
    let switcher = col()
        .w_full()
        .shrink()
        .min_h(0.0)
        .px(6.0)
        .py(3.0)
        .gap(6.0)
        .radius(theme::RADIUS_SWITCHER)
        .fill(t.bg_at(theme::BAR_FILL))
        .border(1.0, t.ink_at(theme::BAR_BORDER))
        .child(chips);
    // Clear of a status bar on either edge; with none at the bottom, clear of
    // the lone health dot in that corner.
    let top = theme::SWITCHER_INSET
        + if edges.status_top {
            theme::STATUS_BAR_HEIGHT
        } else {
            0.0
        };
    let bottom = theme::SWITCHER_INSET
        + if edges.status_bottom {
            theme::STATUS_BAR_HEIGHT
        } else {
            24.0
        };
    let (left, right) = if edges.switcher == RyeOsEdgeVm::Right {
        (None, Some(theme::SWITCHER_INSET))
    } else {
        (Some(theme::SWITCHER_INSET), None)
    };
    col()
        .pin(left, Some(top), right, Some(bottom))
        .w(if renaming {
            theme::SWITCHER_RENAME_WIDTH
        } else {
            theme::SWITCHER_WIDTH
        })
        .min_h(0.0)
        .child(
            switcher.child(
                col()
                    .key("switcher:add")
                    .size(theme::SWITCHER_CHIP, 30.0)
                    .center()
                    .radius(theme::RADIUS_ROW)
                    .role("button")
                    .label("Open a Surface")
                    .hover(t.select_fill())
                    .child(crate::icons::glyph(
                        &crate::icons::PLUS,
                        14.0,
                        t.ink_at(0.40),
                    ))
                    .on_press(|app: &mut App| app.ui(RyeOsUiEvent::OpenLauncher)),
            ),
        )
}

/// One open Surface as a chip in the switcher. The current one has the strong
/// edge; any other selects on press. Pressing the current one twice renames it.
fn surface_entry(surface: &RyeOsSurfaceChoiceVm, local: &Local) -> El {
    let t = Tokens::get();
    let key = format!("surface:{:?}", surface.surface_instance_id);
    let renaming = matches!(
        &local.rename,
        Some(crate::Rename { target: RenameTarget::Surface(RyeOsUiIntent::RenameSurface { surface_instance_id, .. }), .. })
            if *surface_instance_id == surface.surface_instance_id
    );
    if renaming {
        return row()
            .key(key.clone())
            .w_full()
            .h(34.0)
            .center_y()
            .child(rename_form(local, key, "Surface name", ("Save", "Cancel")));
    }
    let (edge, edge_ink) = if surface.active {
        (1.5, t.ink_at(0.95))
    } else {
        (1.0, t.ink_at(0.14))
    };
    let select = surface.select_intent.clone();
    let mut chip = col()
        .size(theme::SWITCHER_CHIP, theme::SWITCHER_CHIP)
        .center()
        .radius(theme::RADIUS_ROW)
        .fill(t.bg)
        .border(edge, edge_ink)
        .child(chip_mark(surface));
    let dot = crate::marks::WorkDot::of(
        surface.needs_you.is_some_and(|count| count > 0),
        surface.running.is_some_and(|count| count > 0),
    );
    if let Some(dot) = dot {
        chip = chip.child(crate::marks::chip_dot(dot, format!("{}:{key}", dot.word())));
    }
    let mut entry = col()
        .key(key)
        .items_center()
        .role("tab")
        .label(surface.title.clone())
        .focus_selected(surface.active)
        .child(chip);
    if surface.active {
        if let Some(intent) = surface.rename_intent.clone() {
            let title = surface.title.clone();
            entry = entry.on_double_press(move |app: &mut App| {
                app.begin_rename(RenameTarget::Surface(intent.clone()), &title)
            });
        }
    } else {
        entry = entry.hover(t.select_fill()).on_press(move |app: &mut App| {
            app.ui(RyeOsUiEvent::Activate {
                intent: select.clone(),
            });
        });
    }
    entry
}

/// The Surfaces switcher along the top: one 40 high band holding an island of
/// 30, a pill per open Surface with its number and name, and the plus after
/// the last.
fn switcher_band(vm: &RyeOsViewModel, local: &Local) -> El {
    let t = Tokens::get();
    let mut tags = row()
        .h_full()
        .min_w(0.0)
        .shrink()
        .gap(2.0)
        .center_y()
        .role("tablist")
        .label("Open Surfaces")
        .scroll_x();
    for surface in &vm.session.surfaces {
        tags = tags.child(surface_tag(surface, local));
    }
    row()
        .w_full()
        .h(theme::SWITCHER_BAND_HEIGHT)
        .px(theme::SWITCHER_INSET)
        .pt(theme::SWITCHER_INSET)
        .role("navigation")
        .label("Surfaces")
        .child(
            row()
                .h(theme::ISLAND_HEIGHT)
                .min_w(0.0)
                .shrink()
                .px(3.0)
                .gap(2.0)
                .center_y()
                .radius(theme::radius_pill(theme::ISLAND_HEIGHT))
                .fill(t.bg_at(theme::BAR_FILL))
                .border(1.0, t.ink_at(theme::BAR_BORDER))
                .child(tags)
                .child(
                    col()
                        .key("switcher:add")
                        .h(TAG_HEIGHT)
                        .px(9.0)
                        .center()
                        .radius(theme::radius_pill(TAG_HEIGHT))
                        .role("button")
                        .label("Open a Surface")
                        .hover(t.select_fill())
                        .child(text("+").font(theme::ui(13.0)).color(t.ink_at(0.40)))
                        .on_press(|app: &mut App| app.ui(RyeOsUiEvent::OpenLauncher)),
                ),
        )
}

/// A tag in the top switcher.
const TAG_HEIGHT: f32 = 24.0;

/// One open Surface as a tag in the top switcher: number, name and its work
/// dot. The current one is filled; pressing it twice renames it.
fn surface_tag(surface: &RyeOsSurfaceChoiceVm, local: &Local) -> El {
    let t = Tokens::get();
    let key = format!("surface:{:?}", surface.surface_instance_id);
    let renaming = matches!(
        &local.rename,
        Some(crate::Rename { target: RenameTarget::Surface(RyeOsUiIntent::RenameSurface { surface_instance_id, .. }), .. })
            if *surface_instance_id == surface.surface_instance_id
    );
    if renaming {
        return row().key(key.clone()).h(28.0).center_y().child(rename_form(
            local,
            key,
            "Surface name",
            ("Save", "Cancel"),
        ));
    }
    let (number, name) = if surface.active {
        (t.ink_at(0.55), t.ink)
    } else {
        (t.ink_at(0.30), t.ink_at(0.62))
    };
    let mut tag = row()
        .key(key.clone())
        .h(TAG_HEIGHT)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .radius(theme::radius_pill(TAG_HEIGHT))
        .role("tab")
        .label(surface.title.clone())
        .focus_selected(surface.active)
        .child(
            text(format!("{}", surface.ordinal))
                .font(theme::ui(10.5))
                .color(number),
        )
        .child(
            text(surface.title.clone())
                .font(theme::medium(theme::TYPE_CELL))
                .color(name),
        );
    let dot = crate::marks::WorkDot::of(
        surface.needs_you.is_some_and(|count| count > 0),
        surface.running.is_some_and(|count| count > 0),
    );
    if let Some(dot) = dot {
        tag = tag.child(crate::marks::tab_dot(dot, format!("{}:{key}", dot.word())));
    }
    if surface.active {
        tag = tag.fill(t.ink_at(0.14));
        if let Some(intent) = surface.rename_intent.clone() {
            let title = surface.title.clone();
            tag = tag.on_double_press(move |app: &mut App| {
                app.begin_rename(RenameTarget::Surface(intent.clone()), &title)
            });
        }
        tag
    } else {
        let select = surface.select_intent.clone();
        tag.hover(t.select_fill()).on_press(move |app: &mut App| {
            app.ui(RyeOsUiEvent::Activate {
                intent: select.clone(),
            });
        })
    }
}

/// What a Surface's chip shows: Home's grid, a node's key figure, or the
/// title's first letter. The current Surface's mark is lit.
fn chip_mark(surface: &RyeOsSurfaceChoiceVm) -> El {
    let t = Tokens::get();
    let (figure, initial) = if surface.active {
        (t.ink_at(0.95), t.ink_at(0.95))
    } else {
        (t.ink_at(0.60), t.ink_at(0.55))
    };
    match surface.chip.mark {
        RyeOsChipMarkVm::Grid => crate::marks::grid_mark(figure),
        // A node whose key is not known yet shows its initial.
        RyeOsChipMarkVm::Key => match surface.chip.fingerprint.as_deref() {
            Some(fingerprint) => crate::marks::key_mark(fingerprint, 14.0, figure),
            None => crate::marks::initial_mark(&surface.title, initial),
        },
        RyeOsChipMarkVm::Initial => crate::marks::initial_mark(&surface.title, initial),
    }
}

/// The transport freshness as its wire name, uppercased.
fn freshness_label(freshness: RyeOsTransportFreshness) -> String {
    serde_json::to_value(freshness)
        .ok()
        .and_then(|value| value.as_str().map(str::to_uppercase))
        .unwrap_or_default()
}

/// The tabs band: bare words centred as a group, the current one set heavier
/// with a line under it, and a plus after the last. Pressing the current tab
/// twice renames it.
fn view_set_strip(top_bar: &RyeOsTopBarVm, local: &Local, edges: Edges) -> El {
    let t = Tokens::get();
    if !top_bar.visible {
        return el().h(0.0);
    }
    // The group scrolls when it overflows, fading where tabs run out of view.
    let mut tabs = row()
        .h_full()
        .min_w(0.0)
        .shrink()
        .gap(8.0)
        .center_y()
        .scroll_x()
        .scroll_edge_fade(TABS_FADE);
    for tab in &top_bar.tabs {
        let view_set_id = tab.view_set_id;
        let renaming = tab.active
            && matches!(
                &local.rename,
                Some(crate::Rename { target: RenameTarget::ViewSet(id), .. }) if *id == view_set_id
            );
        let (word, mark) = if tab.active {
            (theme::medium(theme::TYPE_TITLE), t.ink_at(0.90))
        } else {
            (theme::ui(13.5), t.ink.alpha(0.0))
        };
        let ink = if tab.active { t.ink } else { t.ink_at(0.55) };
        let mut slot = row()
            .key(format!("view-set:{view_set_id}"))
            .h_full()
            .gap(8.0)
            .center_y();
        slot = if renaming {
            slot.child(rename_form(
                local,
                format!("view-set:{view_set_id}"),
                "View set name",
                ("✓", "×"),
            ))
        } else {
            // The line under the word is the word's width; a work dot sits
            // after it, outside the line.
            let mut tab_row = row()
                .h_full()
                .px(10.0)
                .gap(6.0)
                .center_y()
                .role("tab")
                .label(tab.title.clone())
                .focus_selected(tab.active)
                .child(
                    row()
                        .pt(7.0)
                        .pb(6.0)
                        .border_sides([0.0, 0.0, 1.0, 0.0], mark)
                        .child(text(tab.title.clone()).font(word).color(ink)),
                );
            let dot = crate::marks::WorkDot::of(
                tab.needs_you.unwrap_or(false),
                tab.running.unwrap_or(false),
            );
            if let Some(dot) = dot {
                tab_row = tab_row.child(crate::marks::tab_dot(
                    dot,
                    format!("{}:view-set:{view_set_id}", dot.word()),
                ));
            }
            if tab.active {
                let title = tab.title.clone();
                tab_row = tab_row.on_double_press(move |app: &mut App| {
                    app.begin_rename(RenameTarget::ViewSet(view_set_id), &title)
                });
            }
            slot.child(tab_row.on_press(move |app: &mut App| {
                app.ui(RyeOsUiEvent::Activate {
                    intent: RyeOsUiIntent::SelectViewSet { view_set_id },
                });
            }))
        };
        tabs = tabs.child(slot);
    }
    // Under a top switcher the band is the tabs' 44 alone; otherwise it is 50
    // with the words dropped 6 so they sit level with the switcher's chips.
    let (height, top) = if edges.switcher == RyeOsEdgeVm::Top {
        (theme::TABS_HEIGHT - theme::TABS_TOP, 0.0)
    } else {
        (theme::TABS_HEIGHT, theme::TABS_TOP)
    };
    row()
        .w_full()
        .h(height)
        .pt(top)
        // The switcher's room plus the band's own 18, so tabs clear the switcher.
        .pl(edges.side_room().0 + 18.0)
        .pr(edges.side_room().1 + 18.0)
        .gap(20.0)
        .center_y()
        .role("navigation")
        .label("View sets")
        // Equal spacers either side keep the group centred; when it overflows they
        // collapse and the group scrolls between the switcher and the far edge.
        .child(el().grow(1.0))
        .child(tabs)
        .child(if top_bar.add_tab {
            col()
                .key("shell:new-view-set")
                .size(28.0, 28.0)
                .center()
                .role("button")
                .label("New view set")
                .hover(t.select_fill())
                .child(crate::icons::glyph(
                    &crate::icons::PLUS,
                    15.0,
                    t.ink_at(0.40),
                ))
                .on_press(|app: &mut App| {
                    app.ui(RyeOsUiEvent::Activate {
                        intent: RyeOsUiIntent::NewViewSet,
                    });
                })
        } else {
            // The Surface offers no new tab (the website's): no plus.
            el().w(0.0)
        })
        .child(el().grow(1.0))
}

/// How far the tab group fades at an edge where tabs run out of view.
const TABS_FADE: f32 = 24.0;

/// The inline name field with save and cancel; Escape cancels.
/// An inline rename; when it closes, focus returns to the `trigger` slot's first control.
fn rename_form(local: &Local, trigger: String, label: &str, (save, cancel): (&str, &str)) -> El {
    let p = Palette::get();
    let edit = local
        .rename
        .as_ref()
        .map(|rename| rename.edit.clone())
        .unwrap_or_else(|| rune::TextEdit::new(""));
    row()
        .h_full()
        .center_y()
        .gap(4.0)
        .px(8.0)
        .focus_return(trigger)
        .child(
            text_input(&edit, theme::ui(13.0))
                .key("rename:field")
                .label(label.to_owned())
                .color(p.text)
                .w(150.0)
                .padding(6.0)
                .fill(p.plane)
                .border(1.0, p.accent)
                .on_edit(|app: &mut App| app.rename_edit())
                .on_submit(|app: &mut App, _tx| app.finish_rename())
                .on_input_key(|app: &mut App, event: &KeyEvent, _tx| {
                    if matches!(event.key, Key::Escape) {
                        app.cancel_rename();
                        return true;
                    }
                    false
                }),
        )
        .child(
            text(save.to_owned())
                .font(theme::ui(13.0))
                .color(p.text)
                .padding(6.0)
                .role("button")
                .label("Save name")
                .on_press(|app: &mut App| app.finish_rename()),
        )
        .child(
            text(cancel.to_owned())
                .font(theme::ui(13.0))
                .color(p.text)
                .padding(6.0)
                .role("button")
                .label("Cancel rename")
                .on_press(|app: &mut App| app.cancel_rename()),
        )
}

/// The body: the tiles, 16 from the frame's edges (a side switcher's room
/// comes on top, on its side). Under the tabs band the tiles start 2 below it;
/// above the status bar they stop 12 above it.
fn body(vm: &RyeOsViewModel, local: &Local, edges: Edges) -> El {
    let chrome = &vm.presentation.chrome;
    let top = if chrome.top_bar.visible { 2.0 } else { 14.0 };
    let bottom = if edges.status_bottom { 12.0 } else { 14.0 };
    let side = |on_side: bool| if on_side { 18.0 } else { 16.0 };
    row()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .pt(top)
        .pb(bottom)
        .pl(side(edges.switcher == RyeOsEdgeVm::Left))
        .pr(side(edges.switcher == RyeOsEdgeVm::Right))
        .child(view_set(vm, local))
}

fn view_set(vm: &RyeOsViewModel, local: &Local) -> El {
    let maximized = matches!(
        vm.view_set.as_ref().and_then(|set| set.root.as_ref()),
        Some(RyeOsLayoutNodeVm::Tile {
            maximized: true,
            ..
        })
    );
    let gap = if maximized {
        0.0
    } else if local.width() <= crate::COMPACT {
        8.0
    } else {
        12.0
    };
    let middle = row()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .min_w(0.0)
        .gap(gap)
        .child(center(vm.view_set.as_ref(), local));
    col()
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .gap(gap)
        .child(middle)
}

/// The layout, or the bare backdrop when the set has none.
fn center(view_set: Option<&RyeOsViewSetVm>, local: &Local) -> El {
    let content = match view_set.and_then(|set| set.root.as_ref().map(|root| (set, root))) {
        Some((set, root)) => {
            let splits = Splits {
                guard: &set.layout_guard,
                min: set.split_min_ratio,
                max: set.split_max_ratio,
                sole: local.narrow() || tile_count(root) == 1,
            };
            layout(
                root,
                local,
                &splits,
                &mut Vec::new(),
                local.width() - SIDE_RESERVE,
            )
        }
        None => el(),
    };
    col()
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .clip_rounded(0.0)
        .child(content)
}

/// What every split in one view set's layout shares.
struct Splits<'a> {
    guard: &'a str,
    min: f32,
    max: f32,
    /// Whether the set shows one tile only: it has no other tile to be
    /// focused against.
    sole: bool,
}

/// The room a window gives the tiles' row before the tiles: both sides'
/// margins, and so an estimate of a tile's width. Tables use the estimate to
/// decide which columns fit.
const SIDE_RESERVE: f32 = 32.0;

/// The least width a pane keeps beside another: a tile is read whole, and
/// one the window has no room for is clipped rather than squeezed.
const MIN_PANE: f32 = 400.0;

/// The least width a layout node keeps: side by side, panes add up with the
/// divider; stacked, the widest counts.
fn pane_floor(node: &RyeOsLayoutNodeVm) -> f32 {
    match node {
        RyeOsLayoutNodeVm::Split {
            axis,
            first,
            second,
            ..
        } => {
            if *axis == RyeOsSplitAxisVm::Horizontal {
                pane_floor(first) + DIVIDER + pane_floor(second)
            } else {
                pane_floor(first).max(pane_floor(second))
            }
        }
        RyeOsLayoutNodeVm::Tile { .. } => MIN_PANE,
    }
}

/// How many tiles the layout holds.
fn tile_count(node: &RyeOsLayoutNodeVm) -> usize {
    match node {
        RyeOsLayoutNodeVm::Split { first, second, .. } => tile_count(first) + tile_count(second),
        RyeOsLayoutNodeVm::Tile { .. } => 1,
    }
}

/// One split's stable key: its branch path from the root.
fn split_key(path: &[SplitBranch]) -> String {
    let mut key = String::from("split:");
    for branch in path {
        key.push(match branch {
            SplitBranch::First => '1',
            SplitBranch::Second => '2',
        });
    }
    key
}

/// The keyboard step of a focused divider.
const SPLIT_STEP: f32 = 0.05;

/// The drag and keyboard handle between a split's panes. A drag previews
/// the ratio and commits once on release; keys commit each step.
fn divider(
    axis: RyeOsSplitAxisVm,
    key: &str,
    ratio: f32,
    path: &[SplitBranch],
    splits: &Splits<'_>,
) -> El {
    let p = Palette::get();
    let horizontal = axis == RyeOsSplitAxisVm::Horizontal;
    let (min, max) = (splits.min, splits.max);
    let commit = {
        let guard = splits.guard.to_owned();
        let path = path.to_vec();
        move |app: &mut App, ratio: f32| {
            app.ui(RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ResizeSplit {
                    layout_guard: guard.clone(),
                    path: path.clone(),
                    ratio: ratio.clamp(min, max),
                },
            });
        }
    };
    let drag_commit = commit.clone();
    let drag_key = key.to_owned();
    let line = if horizontal {
        el().w(1.0).h_full()
    } else {
        el().w_full().h(1.0)
    };
    let handle = if horizontal {
        col().w(DIVIDER).h_full().items_center()
    } else {
        row().w_full().h(DIVIDER).center_y()
    };
    handle
        .key(format!("{key}:divider"))
        .role("separator")
        .label(if horizontal {
            "Resize columns"
        } else {
            "Resize rows"
        })
        .focusable(true)
        .cursor(if horizontal {
            rune::Cursor::ResizeColumn
        } else {
            rune::Cursor::ResizeRow
        })
        .hover(p.accent_bright.alpha(0.35))
        .child(line)
        .on_drag(
            move |app: &mut App, drag: &mut rune::Drag, _tx| match drag.phase {
                rune::Phase::Down => drag.data = ratio,
                rune::Phase::Move => {
                    let Some(extent) = app.split_extent(&drag_key).filter(|extent| *extent > 0.0)
                    else {
                        return;
                    };
                    let travel = if horizontal { drag.dx } else { drag.dy };
                    let next = (drag.data + travel / extent).clamp(min, max);
                    app.set_split_preview(Some((drag_key.clone(), next)));
                }
                rune::Phase::Up => {
                    let preview = app.local.split_preview.take();
                    if drag.cancelled() {
                        return;
                    }
                    if let Some((_, next)) = preview.filter(|(key, _)| *key == drag_key) {
                        drag_commit(app, next);
                    }
                }
            },
        )
        .on_key(move |app: &mut App, event: &KeyEvent, _tx| {
            let next = match (&event.key, horizontal) {
                (Key::Left, true) | (Key::Up, false) => ratio - SPLIT_STEP,
                (Key::Right, true) | (Key::Down, false) => ratio + SPLIT_STEP,
                (Key::Home, _) => min,
                (Key::End, _) => max,
                _ => return false,
            };
            commit(app, next);
            true
        })
}

/// A split divider's thickness, which is the gap between tiles, and the gap
/// either side of it.
const DIVIDER: f32 = theme::GAP_TILE;
const SPLIT_GAP: f32 = 0.0;

/// Whether the set's current tile is under `node`.
fn holds_focus(node: &RyeOsLayoutNodeVm) -> bool {
    match node {
        RyeOsLayoutNodeVm::Split { first, second, .. } => holds_focus(first) || holds_focus(second),
        RyeOsLayoutNodeVm::Tile { current, .. } => *current,
    }
}

fn layout(
    node: &RyeOsLayoutNodeVm,
    local: &Local,
    splits: &Splits<'_>,
    path: &mut Vec<SplitBranch>,
    width: f32,
) -> El {
    match node {
        RyeOsLayoutNodeVm::Split { first, second, .. } if local.narrow() => {
            // One tile at a time: the focused leaf's branch, the authored tree
            // untouched.
            let into_first = holds_focus(first) || !holds_focus(second);
            path.push(if into_first {
                SplitBranch::First
            } else {
                SplitBranch::Second
            });
            let shown = layout(
                if into_first { first } else { second },
                local,
                splits,
                path,
                width,
            );
            path.pop();
            shown
        }
        RyeOsLayoutNodeVm::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let key = split_key(path);
            let ratio = match &local.split_preview {
                Some((preview_key, preview)) if *preview_key == key => *preview,
                _ => ratio.clamp(splits.min, splits.max),
            };
            let horizontal = *axis == RyeOsSplitAxisVm::Horizontal;
            // Side by side, the panes share the width less the divider.
            let (floor_first, floor_second) = if horizontal {
                (pane_floor(first), pane_floor(second))
            } else {
                (0.0, 0.0)
            };
            let (first_width, second_width) = if horizontal {
                let shared = (width - DIVIDER).max(0.0);
                let first_width = (shared * ratio).min(shared - floor_second).max(floor_first);
                (first_width, (shared - first_width).max(floor_second))
            } else {
                (width, width)
            };
            path.push(SplitBranch::First);
            let first = col()
                .grow(ratio)
                .min_w(floor_first)
                .min_h(0.0)
                .child(layout(first, local, splits, path, first_width));
            path.pop();
            path.push(SplitBranch::Second);
            let second = col()
                .grow(1.0 - ratio)
                .min_w(floor_second)
                .min_h(0.0)
                .child(layout(second, local, splits, path, second_width));
            path.pop();
            let handle = divider(*axis, &key, ratio, path, splits);
            let extent_key = key.clone();
            let container = if horizontal { row() } else { col() };
            container
                .key(key)
                .w_full()
                .h_full()
                .min_w(0.0)
                .min_h(0.0)
                .gap(SPLIT_GAP)
                .on_layout(move |app: &mut App, rect: [f32; 4]| {
                    // The ratio divides the two panes' length: the gaps and
                    // the divider are not part of it.
                    let length = if horizontal { rect[2] } else { rect[3] };
                    app.set_split_extent(extent_key.clone(), length - 2.0 * SPLIT_GAP - DIVIDER);
                })
                .child(first)
                .child(handle)
                .child(second)
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
            authorship,
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
                    sole: splits.sole,
                    width,
                    transparent: *background_transparent,
                    chrome_hidden: *chrome_hidden,
                    tile: Some(views::TileChrome {
                        tile_id,
                        maximized: *maximized,
                        intents,
                        tabs,
                        grouped: group_label.is_some(),
                    }),
                    heading: heading.as_ref(),
                    supplement,
                    view,
                    input: input.as_ref(),
                    authorship: authorship.as_ref(),
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
pub(crate) fn menu_intents<'a>(
    node: &'a RyeOsLayoutNodeVm,
    key: &str,
) -> Option<&'a [RyeOsTileIntentVm]> {
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

/// The health dot's colour: a feed that is not current tints it warn
/// whatever the health says.
fn health_color(vm: &RyeOsViewModel) -> rune::Color {
    let t = Tokens::get();
    // A node the client cannot reach (the feed lost, or the session expired or
    // revoked) is danger; a feed still connecting or catching up is warn.
    match vm.transport.freshness {
        RyeOsTransportFreshness::Current => t.tone(vm.chrome.health_tone).unwrap_or(t.ink_dim()),
        RyeOsTransportFreshness::Reconnecting | RyeOsTransportFreshness::ExpiredOrRevoked => {
            t.danger
        }
        RyeOsTransportFreshness::Connecting | RyeOsTransportFreshness::GapResnapshotRequired => {
            t.warn
        }
    }
}

/// With the status bar hidden, a lone dot at the bottom-right keeps the
/// node's health visible. No text, key, handler or focus.
fn hidden_health_dot(vm: &RyeOsViewModel) -> Option<El> {
    let edges = Edges::of(vm);
    if edges.status_top || edges.status_bottom {
        return None;
    }
    // The dot is the node's health while the bar is hidden: it carries the
    // same key, role and label as the bar's health, so assistive tech and
    // tests still find it (Rune leaves zero-size elements out of the tree).
    Some(
        el().size(6.0, 6.0)
            .radius(3.0)
            .fill(health_color(vm))
            .pin(None, None, Some(14.0), Some(14.0))
            .key("shell:health")
            .role("status")
            .label(format!(
                "{} · {}",
                vm.chrome.health_label,
                freshness_label(vm.transport.freshness)
            )),
    )
}

/// The floating status bar: two islands, the key hints at the left, the
/// core's status segments and the node's health at the right.
fn status_bar(vm: &RyeOsViewModel, edges: Edges, narrow: bool) -> El {
    let t = Tokens::get();
    let model = &vm.presentation.chrome.status_bar;
    if !edges.status_top && !edges.status_bottom {
        // Hidden: the health dot drawn by `hidden_health_dot` stands in.
        return el().h(0.0);
    }
    let island = || {
        row()
            .h(theme::ISLAND_HEIGHT)
            .px(12.0)
            .gap(12.0)
            .center_y()
            .radius(theme::radius_pill(theme::ISLAND_HEIGHT))
            .fill(t.bg_at(theme::BAR_FILL))
            .border(1.0, t.ink_at(theme::BAR_BORDER))
    };
    let dot = |color: rune::Color| el().size(6.0, 6.0).radius(3.0).fill(color);
    let rule = || el().w(1.0).h(12.0).fill(t.ink_at(0.14));
    // The hints run in priority order; the row is bounded by the island, so
    // the last ones are the ones a narrow window loses, and it keeps four.
    let shown = if narrow {
        NARROW_HINTS
    } else {
        model.key_hints.len()
    };
    let mut hints = row().min_w(0.0).shrink().gap(14.0).center_y().scroll_x();
    for hint in model.key_hints.iter().take(shown) {
        hints = hints.child(crate::marks::key_hint(
            &hint.keys,
            &hint.word,
            crate::marks::HINT_WORD_STATUS,
        ));
    }
    let keys = if model.key_hints.is_empty() {
        el()
    } else {
        island()
            .key("shell:keys")
            .min_w(0.0)
            .role("group")
            .label("Keys")
            .child(hints)
    };
    let mut status = island();
    // Attention tints only a dot.
    if let Some(color) = model.attention.and_then(|tone| t.tone(tone)) {
        status = status.child(dot(color));
    }
    for segment in &model.segments {
        let mut part = row().gap(6.0).center_y();
        if let Some(color) = t.tone(segment.tone) {
            part = part.child(dot(color));
        }
        if let Some(label) = segment.label.as_deref().filter(|label| !label.is_empty()) {
            part = part.child(
                text(label.to_owned())
                    .font(theme::TYPE_SMALL)
                    .color(t.ink_at(0.45)),
            );
        }
        part = part.child(
            text(segment.value.clone())
                .font(theme::medium(theme::TYPE_SMALL))
                .color(t.ink_at(0.90)),
        );
        status = status.child(part);
        status = status.child(rule());
    }
    // The node's health, moved here from the top of the frame. A feed that is
    // not current tints the dot whatever the health says.
    let health = health_color(vm);
    status = status.child(
        row()
            .gap(6.0)
            .center_y()
            .key("shell:health")
            .role("status")
            .label(format!(
                "{} · {}",
                vm.chrome.health_label,
                freshness_label(vm.transport.freshness)
            ))
            .child(dot(health))
            .child(
                text(vm.chrome.health_label.clone())
                    .font(theme::medium(theme::TYPE_SMALL))
                    .color(t.ink_at(0.90)),
            ),
    );
    let bar = row()
        .w_full()
        .h(theme::STATUS_BAR_HEIGHT)
        .px(theme::SWITCHER_INSET);
    // The inset is on the frame's side, so the islands float off its edge.
    let bar = if edges.status_top {
        bar.pt(theme::SWITCHER_INSET)
    } else {
        bar.pb(theme::SWITCHER_INSET)
    };
    bar.gap(12.0)
        .center_y()
        .child(keys)
        .child(el().grow(1.0))
        .child(status)
}

/// How many key hints a narrow window keeps.
const NARROW_HINTS: usize = 4;

fn notices(
    notices: &[RyeOsNoticeVm],
    documents: &[(DocumentOffer, Option<String>)],
    edges: Edges,
) -> El {
    let p = Palette::get();
    let above = if edges.status_bottom {
        theme::STATUS_BAR_HEIGHT
    } else {
        0.0
    };
    let beside = if edges.switcher == RyeOsEdgeVm::Right {
        theme::SWITCHER_ROOM
    } else {
        0.0
    };
    let mut stack = col()
        .pin(None, None, Some(20.0 + beside), Some(above + 18.0))
        .w(390.0)
        .gap(8.0);
    for (offer, link) in documents {
        stack = stack.child(document_offer(offer, link.as_deref()));
    }
    for notice in notices {
        let id = notice.id.clone();
        stack = stack.child(
            row()
                .key(format!("notice:{}", notice.id))
                .role(if notice.tone == RyeOsTone::Danger {
                    "alert"
                } else {
                    "status"
                })
                .w_full()
                .padding(12.0)
                .gap(12.0)
                .fill(p.plane_raised)
                .border(1.0, p.line_strong)
                .border_sides([0.0, 0.0, 0.0, 3.0], p.notice_rule(notice.tone))
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

/// A new presentation in its own document, offered as a link the operator
/// opens when they choose. Nothing opens by itself, and dismissing the offer
/// leaves whatever the node prepared in place.
fn document_offer(offer: &DocumentOffer, link: Option<&str>) -> El {
    let p = Palette::get();
    let id = offer.id;
    let (message, tone) = match &offer.state {
        DocumentState::Preparing => (
            "Preparing an independent presentation. No producer is claimed.".to_owned(),
            RyeOsTone::Neutral,
        ),
        DocumentState::Ready { .. } if link.is_some() => (
            "Ready. Open the prepared Surface when you choose.".to_owned(),
            RyeOsTone::Good,
        ),
        DocumentState::Ready { .. } => (
            "The prepared Surface has no address on this page.".to_owned(),
            RyeOsTone::Warn,
        ),
        DocumentState::Unconfirmed { reason } => (
            format!("Preparation is not confirmed: {reason}. Retry repeats the same request."),
            RyeOsTone::Warn,
        ),
    };
    let mut actions = row().gap(10.0).center_y();
    if let Some(url) = link {
        actions = actions.child(
            text("Open Surface")
                .font(theme::ui(12.0))
                .color(p.accent_bright)
                .key(format!("document:{id}:open"))
                .href(url.to_owned()),
        );
    }
    if offer.retryable {
        actions = actions.child(
            text("Retry")
                .font(theme::ui(12.0))
                .color(p.accent_bright)
                .key(format!("document:{id}:retry"))
                .role("button")
                .on_press(move |app: &mut App| app.retry_document(id)),
        );
    }
    row()
        .key(format!("document:{id}"))
        .role("status")
        .label("Prepared presentation")
        .w_full()
        .padding(12.0)
        .gap(12.0)
        .fill(p.plane_raised)
        .border(1.0, p.line_strong)
        .border_sides([1.0, 1.0, 1.0, 3.0], p.notice_rule(tone))
        .shadow(9.0, 30.0, rune::Color::BLACK.alpha(0.45))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(8.0)
                .child(text(message).font(theme::ui(12.0)).color(p.text).wrap())
                .child(actions),
        )
        .child(
            text("×")
                .font(theme::ui(17.0))
                .color(p.muted)
                .key(format!("document:{id}:dismiss"))
                .role("button")
                .label("Dismiss link")
                .on_press(move |app: &mut App| app.dismiss_document(id)),
        )
}

fn dialog_layer(dialog: &RyeOsDialogVm, local: &Local) -> El {
    if dialog.launcher {
        return crate::launcher::layer(dialog, local);
    }
    let p = Palette::get();
    let t = Tokens::get();
    let close = dialog.instance_key.clone();
    let close_button = close.clone();
    let mut content = col().w_full().shrink().min_h(0.0).padding(12.0).gap(10.0);
    let filter = dialog.input.as_ref().filter(|input| input.live_filter);
    if let Some(input) = filter {
        content = content.child(composer(input, local));
    }
    content = content.child(col().w_full().shrink().min_h(0.0).child(views::body(
        &dialog.view,
        &dialog.instance_key,
        local,
    )));
    if let Some(foot) = views::table_foot(&dialog.view) {
        content = content.child(foot);
    }
    if let Some(input) = dialog.input.as_ref().filter(|input| !input.live_filter) {
        content = content.child(composer(input, local));
    }
    let header = row()
        .w_full()
        .h(34.0)
        .px(12.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(col().pr(10.0).child(crate::icons::glyph(
            &crate::icons::SQUARE,
            9.0,
            p.accent_bright,
        )))
        .child(
            // look: the dialog title keeps its upper case and mark for now; the browser smoke test finds the launcher dialog by this label.
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
        .max_w(920.0)
        .max_h(640.0)
        .shrink()
        .min_h(0.0)
        .radius(theme::RADIUS_SHEET)
        .clip_rounded(theme::RADIUS_SHEET)
        .fill(t.sheet_solid())
        .border(1.0, t.ink_at(0.14))
        .child(header)
        .child(content);
    col()
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .items_center()
        // 120, not the spec's 150: 150 plus the 640 max height and the 18 bottom overflows an 800-high window.
        .pt(if local.narrow() { 36.0 } else { 120.0 })
        .px(if local.narrow() { 8.0 } else { 18.0 })
        .pb(if local.narrow() { 8.0 } else { 18.0 })
        .fill(t.bg_at(theme::SCRIM_FILL))
        .modal(true, move |app: &mut App| {
            app.ui(RyeOsUiEvent::CloseDialog {
                instance_key: close.clone(),
            });
        })
        .child(panel)
}

/// The text field bound to an input's buffer. With `place_keys` the keys that
/// choose where a launcher view lands reach the core instead of the field.
pub(crate) fn input_field(
    input: &RyeOsInputVm,
    local: &Local,
    field_font: rune::tok::TextStyle,
    place_keys: bool,
) -> El {
    let key = crate::input_key(&input.address);
    let edit_key = key.clone();
    let change_address = input.address.clone();
    let key_address = input.address.clone();
    let live = input.live_filter;
    let submit_enabled = input.submit_enabled;
    let focused = input.focused;
    let t = Tokens::get();
    text_input(&local.edits.edit(&key), field_font)
        .key(format!("composer:{key}"))
        .placeholder(input.placeholder.clone(), t.ink_faint())
        .color(t.ink_body())
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
            if views::is_launcher_chord(event) {
                return app.core_key(event);
            }
            let mods = &event.mods;
            // With the launcher's places showing, T and S choose one.
            if place_keys && !(mods.ctrl || mods.alt || mods.logo) && is_place_key(event) {
                return app.core_key(event);
            }
            let bare = !(mods.shift || mods.alt || mods.ctrl || mods.logo);
            let action = match &event.key {
                Key::Up if live && bare => RyeOsInputAction::NavigateSelection { delta: -1 },
                Key::Down if live && bare => RyeOsInputAction::NavigateSelection { delta: 1 },
                Key::Enter if live => RyeOsInputAction::ActivateSelection,
                // Alt+Enter submits as an interrupt.
                Key::Enter if !mods.shift && submit_enabled => RyeOsInputAction::Submit {
                    interrupt: mods.alt,
                },
                // With nothing to submit, Return writes a line break, as
                // Shift+Return does.
                Key::Enter if !mods.shift => {
                    let edit_key = crate::input_key(&key_address);
                    app.edit_mut(&edit_key).insert("\n");
                    let Some((text, cursor)) = app.edit_state(&edit_key) else {
                        return true;
                    };
                    RyeOsInputAction::SetText { text, cursor }
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
        })
}

/// T or S: the keys that choose where a launcher view lands.
fn is_place_key(event: &KeyEvent) -> bool {
    match &event.key {
        Key::Text(typed) => typed.eq_ignore_ascii_case("t") || typed.eq_ignore_ascii_case("s"),
        _ => false,
    }
}

/// The input composer: route strip, the text field bound to the core's
/// buffer, and the submit control. A live filter is the compact form above
/// its view: Up and Down move the selection, Enter activates it.
pub fn composer(input: &RyeOsInputVm, local: &Local) -> El {
    let key = crate::input_key(&input.address);
    let button_address = input.address.clone();
    let live = input.live_filter;
    let submit_enabled = input.submit_enabled;
    let t = Tokens::get();
    let field_font = if live {
        theme::TYPE_SMALL
    } else {
        theme::TYPE_PROSE
    };
    let field = input_field(input, local, field_font, false);
    if live {
        // The filter line: a bordered box, the field centred in it and the
        // key that reaches it as a cap at the right.
        return col().w_full().px(12.0).pt(12.0).pb(8.0).child(
            row()
                .w_full()
                .h(FILTER_HEIGHT)
                .px(12.0)
                .gap(8.0)
                .center_y()
                .radius(theme::RADIUS_BUTTON)
                .border(1.0, t.control_border())
                .child(field.grow(1.0).min_w(0.0))
                .child(crate::marks::key_chip("/")),
        );
    }
    let field = field.w_full();
    // The composer: the input, then a strip of the route, the hint and send.
    let chip = |label: String, border: rune::Color| {
        row()
            .h(CHIP_HEIGHT)
            .px(10.0)
            .gap(6.0)
            .center_y()
            .radius(theme::radius_pill(CHIP_HEIGHT))
            .border(1.0, border)
            .min_w(0.0)
            .child(
                text(label)
                    .font(theme::TYPE_MICRO)
                    .color(t.ink_soft())
                    .truncate(),
            )
    };
    // A draft is pending until it is sent: the accent border and a dot.
    let pending = row()
        .h(CHIP_HEIGHT)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .radius(theme::radius_pill(CHIP_HEIGHT))
        .border(1.0, t.accent.alpha(PENDING_BORDER))
        .child(
            el().size(PENDING_DOT, PENDING_DOT)
                .radius(PENDING_DOT / 2.0)
                .fill(t.accent),
        )
        .child(text("DRAFT").font(theme::TYPE_MICRO).color(t.ink_soft()));
    let mut send = el()
        .key(format!("composer:{key}:submit"))
        .role("button")
        .label("Send")
        .size(SEND_SIZE, SEND_SIZE)
        .center()
        .radius(SEND_SIZE / 2.0)
        .fill(t.ink)
        .disabled(!submit_enabled)
        .child(text("↑").font(theme::TYPE_LINE).color(t.bg));
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
    // The send button and the DRAFT chip keep their size. The hint gives way
    // first and is dropped in a narrow window; then the route chip's label
    // is cut.
    let mut strip = row()
        .w_full()
        .center_y()
        .gap(8.0)
        .child(chip(input.route_label.clone(), t.control_border()).shrink());
    if !input.text.is_empty() {
        strip = strip.child(pending);
    }
    strip = strip.child(el().grow(1.0));
    if !local.narrow() {
        strip = strip.child(chip(input.hint.clone(), t.control_border()).shrink());
    }
    // The conversation's usage is quiet text, not a chip.
    if let Some(usage) = &input.usage {
        strip = strip.child(
            text(usage.clone())
                .font(theme::TYPE_MICRO)
                .color(t.ink_dim())
                .truncate()
                .min_w(0.0),
        );
    }
    strip = strip.child(send);
    let boxed = col()
        .w_full()
        .max_w(COMPOSER_COLUMN)
        .padding(COMPOSER_PAD)
        .gap(COMPOSER_GAP)
        .radius(theme::RADIUS_TILE)
        .fill(t.bg_at(COMPOSER_FILL))
        .border(1.0, t.control_border())
        .child(field)
        .child(strip);
    col()
        .w_full()
        .px(theme::PAD_TILE + 6.0)
        .pt(COMPOSER_GAP)
        .pb(18.0)
        .items_center()
        .child(boxed)
}

/// The composer's box: its widest, its padding, the gap between its input and
/// its strip, and its fill as an opacity of `bg`.
const COMPOSER_COLUMN: f32 = 720.0;
const COMPOSER_PAD: f32 = 14.0;
const COMPOSER_GAP: f32 = 16.0;
const COMPOSER_FILL: f32 = 0.55;
/// A chip in the strip, and the send button's diameter.
const CHIP_HEIGHT: f32 = 24.0;
const SEND_SIZE: f32 = 30.0;
/// A pending draft's chip: its border as an opacity of `accent`, and its dot.
const PENDING_BORDER: f32 = 0.6;
const PENDING_DOT: f32 = 5.0;
/// The filter line's height.
const FILTER_HEIGHT: f32 = 30.0;
