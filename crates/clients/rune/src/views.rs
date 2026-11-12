//! View bodies, tile frames and key translation.
//!
//! Every view kind the core projects is matched here without a wildcard, so a
//! new kind fails to compile until this renderer decides how to draw it.

use rune::{Align, El, Key, KeyEvent, ScrollSpec, Side, col, el, row, text};
use ryeos_client_base::ids::RyeOsViewInstanceKey;
use ryeos_client_base::ui::timeline::RyeOsTimelineEntryVm;

use ryeos_client_base::ui::view_model::{
    RyeOsInputVm, RyeOsRowDetailVm, RyeOsRowVm, RyeOsSectionVm, RyeOsSourceStatusVm,
    RyeOsTableCoverageVm, RyeOsTableHierarchyVm, RyeOsTableRowVm, RyeOsTextLineVm,
    RyeOsTileIntentVm, RyeOsTone, RyeOsViewHeadingVm, RyeOsViewSupplementVm, RyeOsViewTabVm,
    RyeOsViewVm,
};
use ryeos_client_base::ui::{
    RyeOsKey, RyeOsKeyEvent, RyeOsKeyModifiers, RyeOsUiEvent, RyeOsUiIntent,
};

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

/// What a tile adds to a frame's header beyond a dock's.
pub struct TileChrome<'a> {
    pub tile_id: &'a str,
    pub maximized: bool,
    pub intents: &'a [RyeOsTileIntentVm],
    pub tabs: &'a [RyeOsViewTabVm],
    /// A grouped tile puts its tabs on their own row.
    pub grouped: bool,
}

/// One mounted view in its frame: a layout tile or a dock.
pub struct FrameSpec<'a> {
    pub key: String,
    pub instance: &'a RyeOsViewInstanceKey,
    pub label: &'a str,
    pub detail: Option<&'a str>,
    pub attachment: Option<&'a str>,
    pub focused: bool,
    pub transparent: bool,
    pub chrome_hidden: bool,
    /// `None` for a dock, whose header names its edge instead of tools.
    pub tile: Option<TileChrome<'a>>,
    pub dock_edge: Option<String>,
    pub heading: Option<&'a RyeOsViewHeadingVm>,
    pub supplement: Option<&'a RyeOsViewSupplementVm>,
    pub view: &'a RyeOsViewVm,
    pub input: Option<&'a RyeOsInputVm>,
}

/// The key of a tile's actions menu, and of the control that opens it.
pub fn menu_key(instance: &RyeOsViewInstanceKey) -> String {
    format!("menu:{instance:?}")
}

/// A bordered frame: header, heading, the view body, supplement footer and
/// the input composer, in the order the browser shell draws them.
pub fn frame(spec: FrameSpec<'_>, local: &crate::Local) -> El {
    let p = Palette::get();
    let mut frame = col()
        .key(spec.key.clone())
        .w_full()
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .fill(p.plane.alpha(if spec.transparent { 0.78 } else { 0.94 }))
        .border(1.0, if spec.focused { p.accent } else { p.line })
        // Content taller than its slot stays inside the frame.
        .clip_rounded(0.0);
    if !spec.chrome_hidden {
        frame = frame.child(frame_header(&spec, local));
    }
    if let Some(heading) = spec.heading {
        frame = frame.child(content_heading(heading));
    }
    let filter = spec.input.filter(|input| input.live_filter);
    if let Some(input) = filter {
        frame = frame.child(crate::shell::composer(input, local));
    }
    frame = frame.child(col().w_full().grow(1.0).min_h(0.0).child(body(
        spec.view,
        spec.instance,
        local,
    )));
    if let Some(supplement) = spec.supplement {
        frame = supplement_footer(frame, supplement);
    }
    if let Some(input) = spec.input.filter(|input| !input.live_filter) {
        frame = frame.child(crate::shell::composer(input, local));
    }
    frame
}

fn frame_header(spec: &FrameSpec<'_>, local: &crate::Local) -> El {
    let p = Palette::get();
    let signal = el()
        .size(if spec.focused { 13.0 } else { 5.0 }, 5.0)
        .fill(p.accent_bright);
    let mut identity = row()
        .center_y()
        .gap(10.0)
        .min_w(0.0)
        .child(signal)
        .child(header_label(spec.label, p.text));
    if let Some(detail) = spec.detail.filter(|detail| !detail.is_empty()) {
        identity = identity.child(frame_detail(detail));
    }
    let mut title_row = row()
        .w_full()
        .min_h(HEADER_HEIGHT)
        .px(12.0)
        .center_y()
        .gap(10.0)
        .child(identity.grow(1.0));
    let Some(tile) = &spec.tile else {
        // A dock names its attachment, its detail, or else its edge.
        let aside = spec
            .attachment
            .or(spec.detail)
            .map(str::to_owned)
            .or_else(|| spec.dock_edge.clone())
            .unwrap_or_default();
        return title_row
            .child(header_label(&aside, p.muted))
            .border_sides([0.0, 0.0, 1.0, 0.0], p.line);
    };
    let tabs = (tile.tabs.len() > 1).then(|| view_tabs(tile.tabs, tile.grouped));
    if !tile.grouped
        && let Some(tabs) = tabs.as_ref().cloned()
    {
        title_row = title_row.child(tabs);
    }
    let mut tools = row().center_y().gap(8.0);
    if let Some(attachment) = spec.attachment {
        tools = tools.child(frame_detail(attachment).label("Selection attachment"));
    }
    let tile_id = tile.tile_id.to_owned();
    let maximized = tile.maximized;
    tools = tools.child(
        text(if maximized { "↙" } else { "↗" })
            .font(theme::ui(13.0))
            .color(p.muted)
            .padding(2.0)
            .role("button")
            .label(if maximized {
                "Restore view"
            } else {
                "Maximize view"
            })
            .on_press(move |app: &mut App| {
                app.ui(RyeOsUiEvent::Activate {
                    intent: RyeOsUiIntent::ToggleTileMaximized {
                        tile_id: tile_id.clone(),
                    },
                });
            }),
    );
    if !tile.intents.is_empty() {
        let key = menu_key(spec.instance);
        let open = local.menu.as_deref() == Some(key.as_str());
        let toggle = key.clone();
        tools = tools.child(
            col()
                .key(format!("{key}:open"))
                .child(crate::icons::glyph(
                    &crate::icons::MORE,
                    14.0,
                    if open { p.accent_bright } else { p.muted },
                ))
                .padding(5.0)
                .role("button")
                .label(format!("Actions for {}", spec.label))
                .expanded(open)
                .on_press(move |app: &mut App| app.toggle_menu(toggle.clone())),
        );
    }
    title_row = title_row.child(tools);
    let mut header = col()
        .w_full()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(title_row);
    if tile.grouped
        && let Some(tabs) = tabs
    {
        header = header.child(
            row()
                .w_full()
                .min_h(40.0)
                .px(6.0)
                .border_sides([1.0, 0.0, 0.0, 0.0], p.line)
                .child(tabs),
        );
    }
    header
}

fn frame_detail(detail: &str) -> El {
    let p = Palette::get();
    text(detail.to_owned())
        .font(theme::data(9.0).tracking(0.4))
        .color(p.muted)
        .truncate()
}

fn view_tabs(tabs: &[RyeOsViewTabVm], grouped: bool) -> El {
    let p = Palette::get();
    let mut strip = row().h_full().role("tablist");
    if !grouped {
        strip = strip.ml(18.0);
    }
    if grouped {
        strip = strip.scroll_x();
    }
    for tab in tabs {
        let target = tab.tile_id.clone();
        strip = strip.child(
            row()
                .h_full()
                .min_h(HEADER_HEIGHT)
                .px(13.0)
                .center_y()
                .border_sides(
                    [0.0, 0.0, 1.0, 0.0],
                    if tab.active {
                        p.yellow
                    } else {
                        p.plane.alpha(0.0)
                    },
                )
                .role("tab")
                .focus_selected(tab.active)
                .child(
                    text(tab.title.clone())
                        .font(theme::data(9.0))
                        .color(if tab.active { p.text } else { p.muted }),
                )
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::FocusChanged {
                        target: Some(target.clone()),
                    });
                }),
        );
    }
    strip
}

/// The authored heading above a view's content.
fn content_heading(heading: &RyeOsViewHeadingVm) -> El {
    let p = Palette::get();
    let mut block = col()
        .w_full()
        .pt(28.0)
        .px(34.0)
        .pb(22.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(
            text(heading.eyebrow.to_uppercase())
                .font(theme::data(9.0).tracking(1.3))
                .color(p.muted),
        )
        .child(
            text(heading.title.clone())
                .font(theme::ui(38.0).tracking(-1.5))
                .color(p.text)
                .wrap()
                .max_w(720.0)
                .mt(14.0)
                .mb(10.0),
        );
    if !heading.summary.is_empty() {
        block = block.child(
            text(heading.summary.clone())
                .font(theme::ui(13.0))
                .color(p.text)
                .wrap()
                .max_w(620.0),
        );
    }
    if !heading.metadata.is_empty() {
        block = block.child(
            text(heading.metadata.join("  /  ").to_uppercase())
                .font(theme::data(9.0).tracking(1.3))
                .color(p.muted)
                .wrap()
                .mt(17.0),
        );
    }
    block
}

/// The supplement's excerpt and footer under a view.
fn supplement_footer(mut frame: El, supplement: &RyeOsViewSupplementVm) -> El {
    let p = Palette::get();
    let caption = |label: &str| {
        text(label.to_uppercase())
            .font(theme::data(9.0).tracking(0.7))
            .color(p.muted)
            .px(12.0)
            .py(8.0)
    };
    if !supplement.excerpt.is_empty() {
        let mut code = col().w_full();
        for line in &supplement.excerpt {
            let ink = line.tone.map_or(p.muted, |tone| p.tone(tone, p.muted));
            let mut entry = row()
                .w_full()
                .min_h(20.0)
                .px(12.0)
                .py(2.0)
                .child(
                    text(line.field.clone())
                        .font(theme::data(10.0))
                        .color(p.faint)
                        .w(42.0),
                )
                .child(
                    text(line.value.clone())
                        .font(theme::data(10.0))
                        .color(ink)
                        .wrap_anywhere()
                        .grow(1.0)
                        .min_w(0.0),
                );
            if line.tone == Some(RyeOsTone::Good) {
                entry = entry.fill(p.green.alpha(0.09));
            }
            code = code.child(entry);
        }
        let mut section = col()
            .w_full()
            .max_h(240.0)
            .border_sides([1.0, 0.0, 0.0, 0.0], p.line)
            .label(if supplement.excerpt_title.is_empty() {
                "Excerpt".to_owned()
            } else {
                supplement.excerpt_title.clone()
            })
            .scroll(ScrollSpec {
                stick_bottom: false,
            });
        if !supplement.excerpt_title.is_empty() {
            section = section.child(caption(&supplement.excerpt_title));
        }
        frame = frame.child(section.child(code));
    }
    if !supplement.footer.is_empty() || !supplement.footer_rows.is_empty() {
        let mut footer = col()
            .w_full()
            .py(4.0)
            .border_sides([1.0, 0.0, 0.0, 0.0], p.line);
        if !supplement.footer.is_empty() {
            footer = footer.child(caption(&supplement.footer));
        }
        for line in &supplement.footer_rows {
            let ink = line.tone.map_or(p.muted, |tone| p.tone(tone, p.muted));
            footer = footer.child(
                row()
                    .w_full()
                    .px(14.0)
                    .py(5.0)
                    .gap(12.0)
                    .child(
                        text(line.field.clone())
                            .font(theme::ui(10.0))
                            .color(ink)
                            .grow(1.0)
                            .min_w(0.0)
                            .truncate(),
                    )
                    .child(
                        text(line.value.clone())
                            .font(theme::ui(10.0))
                            .color(p.muted),
                    ),
            );
        }
        frame = frame.child(footer);
    }
    frame
}

/// A tile's open actions menu, anchored under its control and drawn at the
/// root so no frame clips it.
pub fn actions_menu(key: &str, intents: &[RyeOsTileIntentVm]) -> El {
    let p = Palette::get();
    let mut list = col()
        .key(key.to_owned())
        .anchored_fit(format!("{key}:open"), Side::Bottom, Align::End, 2.0)
        .min_w(180.0)
        .max_w(360.0)
        .padding(4.0)
        .fill(p.plane_raised)
        .border(1.0, p.line_strong)
        .role("menu")
        .modal(true, |app: &mut App| app.close_menu());
    for action in intents {
        let intent = action.intent.clone();
        list = list.child(
            text(action.label.clone())
                .font(theme::ui(12.0))
                .color(p.text)
                .truncate()
                .w_full()
                .px(10.0)
                .py(9.0)
                .hover(p.plane_selected)
                .role("menuitem")
                .label(action.title.clone())
                .on_press(move |app: &mut App| {
                    app.close_menu();
                    app.ui(RyeOsUiEvent::Activate {
                        intent: intent.clone(),
                    });
                }),
        );
    }
    list
}

/// The body of one mounted view instance.
pub fn body(view: &RyeOsViewVm, instance: &RyeOsViewInstanceKey, local: &crate::Local) -> El {
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
        RyeOsViewVm::Text { title, lines, .. } => text_view(instance, title, lines),
        RyeOsViewVm::Document {
            title,
            source_status,
            path,
            content,
            truncated,
            ..
        } => document_view(
            instance,
            title,
            source_status.as_ref(),
            path,
            content,
            *truncated,
        ),
        RyeOsViewVm::Table {
            title,
            source_status,
            columns,
            total_rows,
            coverage,
            rows,
            ..
        } => table_view(
            instance,
            title,
            source_status.as_ref(),
            columns,
            *total_rows,
            coverage.as_ref(),
            rows,
        ),
        RyeOsViewVm::Timeline {
            title,
            source_status,
            entries,
            entry_ids,
            entry_indents,
            selected,
            entry_expandable,
            entry_expanded,
            entry_details,
            ..
        } => timeline_view(
            instance,
            title,
            source_status.as_ref(),
            Timeline {
                entries,
                ids: entry_ids,
                indents: entry_indents,
                selected: *selected,
                expandable: entry_expandable,
                expanded: entry_expanded,
                details: entry_details,
            },
        ),
        RyeOsViewVm::Field { field } => crate::field::view(field, instance, local),
    }
}

/// A scrolling list body with the list views' inset.
fn list(key: String, label: &str) -> El {
    col()
        .key(key)
        .w_full()
        .shrink()
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
    let fold = if section.collapsed {
        &crate::icons::COLLAPSED
    } else {
        &crate::icons::EXPANDED
    };
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
        .child(crate::icons::glyph(fold, 12.0, ink))
        .child(
            text(section.title.clone())
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
                .child(disclosure_icon(expanded, 13.0))
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

fn text_view(instance: &RyeOsViewInstanceKey, title: &str, lines: &[RyeOsTextLineVm]) -> El {
    let p = Palette::get();
    let mut body = col()
        .key(format!("text:{instance:?}"))
        .w_full()
        .shrink()
        .min_h(0.0)
        .px(28.0)
        .py(22.0)
        .gap(8.0)
        .label(title.to_owned())
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    for line in lines {
        body = body.child(
            text(line.text.clone())
                .font(theme::data(12.0))
                .color(p.tone(line.tone, p.text))
                .wrap_anywhere()
                .w_full(),
        );
    }
    body
}

fn document_view(
    instance: &RyeOsViewInstanceKey,
    title: &str,
    status: Option<&RyeOsSourceStatusVm>,
    path: &str,
    content: &str,
    truncated: bool,
) -> El {
    let p = Palette::get();
    let mut header = row()
        .w_full()
        .px(16.0)
        .py(9.0)
        .gap(16.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(
            text(path.to_owned())
                .font(theme::data(9.0))
                .color(p.muted)
                .truncate_head()
                .grow(1.0)
                .min_w(0.0),
        );
    if truncated {
        header = header.child(
            text("BOUNDED PREVIEW")
                .font(theme::data(9.0))
                .color(p.yellow),
        );
    }
    let mut page = col().w_full().shrink().min_h(0.0).label(title.to_owned());
    if let Some(status) = status {
        page = page.child(source_status(status));
    }
    page.child(header).child(
        col()
            .key(format!("document:{instance:?}"))
            .w_full()
            .shrink()
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(
                text(content.to_owned())
                    .font(theme::data(11.0))
                    .color(p.text)
                    .wrap_anywhere()
                    .w_full()
                    .pt(18.0)
                    .px(20.0)
                    .pb(32.0),
            ),
    )
}

/// The detail column every table carries at its right edge.
const TABLE_ACTION_WIDTH: f32 = 50.0;

fn table_view(
    instance: &RyeOsViewInstanceKey,
    title: &str,
    status: Option<&RyeOsSourceStatusVm>,
    columns: &[String],
    total_rows: usize,
    coverage: Option<&RyeOsTableCoverageVm>,
    rows: &[RyeOsTableRowVm],
) -> El {
    let p = Palette::get();
    let mut body = col()
        .key(format!("table:{instance:?}"))
        .w_full()
        .shrink()
        .min_h(0.0)
        .px(20.0)
        .py(16.0)
        .role("table")
        .label(title.to_owned())
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    if let Some(status) = status {
        body = body.child(source_status(status));
    }
    if let Some(coverage) = coverage {
        body = body.child(
            text(format!("{} / {}", coverage.state, coverage.reason).to_uppercase())
                .font(theme::data(10.0).tracking(0.4))
                .color(p.muted)
                .wrap_anywhere()
                .px(9.0)
                .pb(10.0)
                .role("status"),
        );
    }
    let mut head = row()
        .w_full()
        .min_h(38.0)
        .center_y()
        .role("row")
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line);
    for column in columns {
        head = head.child(
            text(column.to_uppercase())
                .font(theme::data(9.0))
                .color(p.muted)
                .truncate()
                .grow(1.0)
                .min_w(0.0)
                .px(9.0)
                .role("columnheader"),
        );
    }
    head = head.child(
        col()
            .w(TABLE_ACTION_WIDTH)
            .center()
            .role("columnheader")
            .child(text("DETAIL").font(theme::data(9.0)).color(p.muted)),
    );
    body = body.child(head);
    if total_rows == 0 {
        body = body.child(
            text("No rows in this page.")
                .font(theme::data(10.0))
                .color(p.muted)
                .px(9.0)
                .py(15.0)
                .role("status"),
        );
    }
    for item in rows {
        body = body.child(table_record(instance, item));
    }
    body
}

fn table_record(instance: &RyeOsViewInstanceKey, item: &RyeOsTableRowVm) -> El {
    let p = Palette::get();
    let row_ink = p.tone(item.tone, p.text);
    let mut line = row().w_full().min_w(0.0).min_h(38.0).center_y().role("row");
    if item.selected {
        line = line.fill(p.plane_selected);
    }
    for (index, cell) in item.cells.iter().enumerate() {
        let ink = item
            .cell_tones
            .get(index)
            .copied()
            .flatten()
            .map_or(row_ink, |tone| p.tone(tone, row_ink));
        let choose_key = instance.clone();
        let item_id = item.id.clone();
        let mut slot = row().grow(1.0).min_w(0.0).h_full().center_y().role("cell");
        if index == 0
            && let Some(hierarchy) = &item.hierarchy
        {
            slot = slot.child(tree_prefix(instance, &item.id, cell, hierarchy));
        }
        slot = slot.child(
            text(cell.clone())
                .font(theme::ui(13.0))
                .color(ink)
                .truncate()
                .grow(1.0)
                .min_w(0.0)
                .px(9.0)
                .py(11.0)
                .role("button")
                .label(format!("Select row: {cell}"))
                .focus_selected(index == 0 && item.selected)
                .hover(p.plane_raised)
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::ChooseViewItem {
                        instance_key: choose_key.clone(),
                        item_id: item_id.clone(),
                        activate: true,
                    });
                }),
        );
        line = line.child(slot);
    }
    let mut action = col().w(TABLE_ACTION_WIDTH).h_full().role("cell");
    if item.expandable {
        action = action.child(disclosure(
            instance,
            &item.id,
            item.expanded,
            "row".to_owned(),
            TABLE_ACTION_WIDTH,
        ));
    }
    line = line.child(action);
    let mut record = col()
        .key(format!("item:{instance:?}:{}", item.id))
        .w_full()
        .min_w(0.0)
        .role("rowgroup")
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(line);
    if item.expanded {
        record = record.child(details(&item.detail));
    }
    record
}

/// The tree drawing before a hierarchical table row's first cell. The
/// structure comes from the core's retained tree projection, never inferred.
fn tree_prefix(
    instance: &RyeOsViewInstanceKey,
    item_id: &str,
    label: &str,
    hierarchy: &RyeOsTableHierarchyVm,
) -> El {
    let p = Palette::get();
    let mut guides: String = hierarchy
        .ancestor_continues
        .iter()
        .map(|continues| if *continues { '│' } else { '\u{a0}' })
        .collect();
    guides.push(if hierarchy.is_last { '└' } else { '├' });
    let mut prefix = row().center_y().pl(9.0).child(
        text(guides)
            .font(theme::data(13.0))
            .color(p.muted)
            .a11y_hidden(),
    );
    if hierarchy.has_children {
        let fold_key = instance.clone();
        let item_id = item_id.to_owned();
        let collapsed = hierarchy.collapsed;
        prefix = prefix.child(
            col()
                .center()
                .child(crate::icons::glyph(
                    if collapsed {
                        &crate::icons::COLLAPSED
                    } else {
                        &crate::icons::EXPANDED
                    },
                    12.0,
                    p.muted,
                ))
                .min_w(20.0)
                .padding(4.0)
                .hover(p.plane_selected)
                .role("button")
                .label(format!(
                    "{} descendants of {label}",
                    if collapsed { "Expand" } else { "Collapse" }
                ))
                .expanded(!collapsed)
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::SetViewTreeItemCollapsed {
                        instance_key: fold_key.clone(),
                        item_id: item_id.clone(),
                        collapsed: !collapsed,
                    });
                }),
        );
    } else {
        prefix = prefix.child(
            text("─")
                .font(theme::data(13.0))
                .color(p.muted)
                .a11y_hidden(),
        );
    }
    prefix
}

/// The expand/collapse control at the right edge of a record.
/// The open/closed triangle of an expandable record.
fn disclosure_icon(expanded: bool, size: f32) -> El {
    let p = Palette::get();
    if expanded {
        crate::icons::glyph(&crate::icons::EXPANDED, size, p.yellow)
    } else {
        crate::icons::glyph(&crate::icons::COLLAPSED, size, p.muted)
    }
}

/// The rows and timeline disclosure column.
const DISCLOSURE_WIDTH: f32 = 34.0;

/// An expand/collapse control `width` points wide, ruled off on its left.
fn disclosure(
    instance: &RyeOsViewInstanceKey,
    item_id: &str,
    expanded: bool,
    what: String,
    width: f32,
) -> El {
    let p = Palette::get();
    let expand_key = instance.clone();
    let item_id = item_id.to_owned();
    col()
        .w(width)
        .h_full()
        .min_h(34.0)
        .center()
        .border_sides([0.0, 0.0, 0.0, 1.0], p.line)
        .fill(if expanded { p.plane_selected } else { p.plane })
        .hover(p.plane_selected)
        .role("button")
        .label(format!(
            "{} {what}",
            if expanded { "Collapse" } else { "Expand" }
        ))
        .expanded(expanded)
        .child(disclosure_icon(expanded, 13.0))
        .on_press(move |app: &mut App| {
            app.ui(RyeOsUiEvent::ToggleViewItemExpansion {
                instance_key: expand_key.clone(),
                item_id: item_id.clone(),
                expand: !expanded,
            });
        })
}

/// The timeline's parallel per-entry arrays, borrowed together.
struct Timeline<'a> {
    entries: &'a [RyeOsTimelineEntryVm],
    ids: &'a [String],
    indents: &'a [u8],
    selected: Option<usize>,
    expandable: &'a [bool],
    expanded: &'a [bool],
    details: &'a [Vec<RyeOsRowDetailVm>],
}

/// Indent per call-tree level.
const TIMELINE_INDENT: f32 = 18.0;

fn timeline_view(
    instance: &RyeOsViewInstanceKey,
    title: &str,
    status: Option<&RyeOsSourceStatusVm>,
    timeline: Timeline<'_>,
) -> El {
    let p = Palette::get();
    // A feed: it opens at the newest entry and follows new ones while there.
    let mut body = col()
        .key(format!("timeline:{instance:?}"))
        .w_full()
        .shrink()
        .min_h(0.0)
        .px(16.0)
        .py(12.0)
        .label(title.to_owned())
        .scroll(ScrollSpec { stick_bottom: true });
    if let Some(status) = status {
        body = body.child(source_status(status));
    }
    for (index, entry) in timeline.entries.iter().enumerate() {
        // The renderer never trusts the parallel arrays to be complete.
        let id = timeline.ids.get(index);
        let indent = f32::from(timeline.indents.get(index).copied().unwrap_or(0));
        let selected = timeline.selected == Some(index);
        let expandable = timeline.expandable.get(index).copied().unwrap_or(false);
        let expanded = timeline.expanded.get(index).copied().unwrap_or(false);
        let content = match entry {
            RyeOsTimelineEntryVm::Block { text: body, tone } => text(body.clone())
                .font(theme::ui(13.0))
                .color(p.tone(*tone, p.text))
                .wrap()
                .grow(1.0)
                .min_w(0.0),
            RyeOsTimelineEntryVm::Line {
                primary,
                meta,
                tone,
                ..
            } => entry_line(primary, meta.as_deref(), p.tone(*tone, p.text), 500.0),
            RyeOsTimelineEntryVm::Pair {
                summary,
                meta,
                tone,
                ..
            } => entry_line(summary, meta.as_deref(), p.tone(*tone, p.text), 400.0),
            RyeOsTimelineEntryVm::Separator { label } => text(label.to_uppercase())
                .font(theme::data(9.0))
                .color(p.faint),
        };
        let mut button = row()
            .grow(1.0)
            .min_w(0.0)
            .ml(indent * TIMELINE_INDENT)
            .px(15.0)
            .py(12.0)
            .center_y()
            .border_sides(
                [0.0, 0.0, 0.0, 1.0],
                if selected { p.accent_bright } else { p.line },
            )
            .role("button")
            .focus_selected(selected)
            .child(content);
        if selected {
            button = button.fill(p.plane_selected);
        }
        if let Some(id) = id {
            let choose_key = instance.clone();
            let item_id = id.clone();
            button = button.hover(p.plane_raised).on_press(move |app: &mut App| {
                app.ui(RyeOsUiEvent::ChooseViewItem {
                    instance_key: choose_key.clone(),
                    item_id: item_id.clone(),
                    activate: true,
                });
            });
        }
        let mut head = row().w_full().min_w(0.0).child(button);
        if expandable && let Some(id) = id {
            head = head.child(disclosure(
                instance,
                id,
                expanded,
                "timeline entry".to_owned(),
                DISCLOSURE_WIDTH,
            ));
        }
        let mut record = col()
            .key(format!(
                "entry:{instance:?}:{}",
                id.map_or_else(|| index.to_string(), Clone::clone)
            ))
            .w_full()
            .min_w(0.0)
            .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
            .child(head);
        if expanded && let Some(rows) = timeline.details.get(index) {
            record = record.child(details(rows));
        }
        body = body.child(record);
    }
    body
}

/// A timeline line: the primary text with its meta beside it.
fn entry_line(primary: &str, meta: Option<&str>, ink: rune::Color, weight: f32) -> El {
    let p = Palette::get();
    let mut line = row().grow(1.0).min_w(0.0).gap(12.0).center_y().child(
        text(primary.to_owned())
            .font(rune::tok::TextStyle::new(13.0, weight).family(theme::UI))
            .color(ink)
            .wrap()
            .grow(1.0)
            .min_w(0.0),
    );
    if let Some(meta) = meta {
        line = line.child(text(meta.to_owned()).font(theme::ui(10.0)).color(p.muted));
    }
    line
}

/// How a block of field/value pairs sits: indented under an expanded record,
/// or flush inside a narrow floating panel (the field's detail).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Pairs {
    Record,
    Panel,
}

/// Field/value pairs under an expanded record.
pub(crate) fn details(details: &[RyeOsRowDetailVm]) -> El {
    details_as(details, Pairs::Record)
}

pub(crate) fn details_as(details: &[RyeOsRowDetailVm], style: Pairs) -> El {
    let p = Palette::get();
    let pairs: Vec<(&str, String, rune::Color)> = details
        .iter()
        .map(|detail| {
            let field = if detail.field.is_empty() {
                "detail"
            } else {
                detail.field.as_str()
            };
            let ink = detail.tone.map_or(p.muted, |tone| p.tone(tone, p.muted));
            (field, detail.value.clone(), ink)
        })
        .collect();
    pair_block(&pairs, style)
}

/// Untoned field/value pairs.
pub(crate) fn detail_pairs(pairs: &[(&str, String)], style: Pairs) -> El {
    let p = Palette::get();
    let pairs: Vec<(&str, String, rune::Color)> = pairs
        .iter()
        .map(|(field, value)| (*field, value.clone(), p.muted))
        .collect();
    pair_block(&pairs, style)
}

fn pair_block(pairs: &[(&str, String, rune::Color)], style: Pairs) -> El {
    let p = Palette::get();
    let (label_width, gap) = match style {
        Pairs::Record => (110.0, 12.0),
        Pairs::Panel => (76.0, 8.0),
    };
    let mut block = col().w_full();
    block = match style {
        Pairs::Record => block
            .pt(7.0)
            .pb(9.0)
            .pl(34.0)
            .pr(12.0)
            .fill(p.plane_raised.alpha(0.45)),
        Pairs::Panel => block.py(5.0),
    };
    for (field, value, ink) in pairs {
        block = block.child(
            row()
                .w_full()
                .min_w(0.0)
                .gap(gap)
                .py(2.0)
                .child(
                    text((*field).to_owned())
                        .font(theme::data(9.0))
                        .color(p.faint)
                        .wrap_anywhere()
                        .w(label_width),
                )
                .child(
                    text(value.clone())
                        .font(theme::data(9.0))
                        .color(*ink)
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
                .wrap()
                .min_w(0.0)
                .max_w(360.0),
        )
}
