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

/// The launcher chord, K with exactly one of Ctrl and Alt: it reaches the
/// core even from inside a text field, as in the browser shell.
pub fn is_launcher_chord(event: &KeyEvent) -> bool {
    let k = match &event.key {
        Key::Text(typed) => typed.eq_ignore_ascii_case("k"),
        Key::Command(ch) => ch.eq_ignore_ascii_case(&'k'),
        _ => false,
    };
    let mods = &event.mods;
    k && !mods.shift && !mods.logo && (mods.ctrl != mods.alt)
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
    if spec.focused {
        // The focused frame's inner dashed rule, drawn over its content.
        frame = frame.child(
            el().pin(Some(3.0), Some(3.0), Some(3.0), Some(3.0))
                .border(1.0, p.accent)
                .border_dash(3.0, 3.0)
                .a11y_hidden(),
        );
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
        RyeOsViewVm::RequiredSubject {
            title,
            input,
            facets,
            actions,
        } => required_subject(title, input, facets, actions),
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
            ..
        } => match Timeline::of(view) {
            Some(timeline) => timeline_view(instance, title, source_status.as_ref(), timeline),
            None => el(),
        },
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
    if rows.len() > VIRTUAL_ROWS_FROM {
        let head = status.map(source_status);
        let keys = rows
            .iter()
            .map(|item| format!("item:{instance:?}:{}", item.id))
            .collect();
        return virtual_list(
            format!("rows:{instance:?}"),
            title,
            "list",
            head,
            instance,
            keys,
            ROW_ESTIMATE,
            false,
            |view, instance, index| match view {
                RyeOsViewVm::Rows { rows, .. } => rows
                    .get(index)
                    .map(|item| record(instance, item, RowStyle::Rows)),
                _ => None,
            },
        );
    }
    let mut body = list(format!("rows:{instance:?}"), title).role("list");
    if let Some(status) = status {
        body = body.child(source_status(status));
    }
    for item in rows {
        body = body.child(record(instance, item, RowStyle::Rows));
    }
    body
}

/// Lists longer than this build only the rows in view (`El::virtual_rows`);
/// shorter ones build every row.
const VIRTUAL_ROWS_FROM: usize = 64;
/// Estimated heights for rows not yet measured.
const ROW_ESTIMATE: f32 = 46.0;
const TABLE_ROW_ESTIMATE: f32 = 38.0;

/// A long list: `head` stays above a scroll viewport whose rows are built by
/// index from the current model at layout time, so a commit costs a key per
/// row plus the rows in view. `keys` are the records' own keys, so selection
/// reveal reaches any row. A feed (`follow`) opens at its newest row and keeps
/// following while the reader is there.
#[allow(clippy::too_many_arguments)]
fn virtual_list(
    key: String,
    label: &str,
    role: &'static str,
    head: Option<El>,
    instance: &RyeOsViewInstanceKey,
    keys: Vec<String>,
    estimate: f32,
    follow: bool,
    build: fn(&RyeOsViewVm, &RyeOsViewInstanceKey, usize) -> Option<El>,
) -> El {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    keys.hash(&mut hasher);
    let version = hasher.finish();
    let count = keys.len();
    let keys = std::rc::Rc::new(keys);
    let build_instance = instance.clone();
    let rows = col()
        .key(format!("{key}:rows"))
        .w_full()
        .px(16.0)
        .py(12.0)
        .growth(rune::GrowthSpec {
            spring: rune::GROWTH,
            edge: rune::edge::NO_MASK,
        })
        .virtual_rows(
            count,
            estimate,
            move |_: &App, index| std::borrow::Cow::Owned(keys.get(index).cloned().unwrap_or_default()),
            move |app: &App, index| {
                app.with_view(&build_instance, |view| build(view, &build_instance, index))
                    .flatten()
                    .unwrap_or_else(el)
            },
        )
        .virtual_rows_version(version);
    // The viewport fills the tile: virtual rows give it no content height
    // to size from.
    let mut body = col()
        .key(key.clone())
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .role(role)
        .label(label.to_owned());
    if let Some(head) = head {
        body = body.child(col().w_full().px(16.0).pt(12.0).child(head));
    }
    body.child(
        col()
            .key(format!("{key}:scroll"))
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: follow,
            })
            .child(rows),
    )
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
                        text(
                            item.glyph
                                .as_deref()
                                .map_or("◇".into(), |glyph| {
                                    theme::drawable(glyph, theme::Face::Ui)
                                })
                                .into_owned(),
                        )
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
                .selectable(true)
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
                    .selectable(true)
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
    if rows.len() > VIRTUAL_ROWS_FROM {
        let head = col()
            .w_full()
            .children(table_head(status, coverage, columns));
        let keys = rows
            .iter()
            .map(|item| format!("item:{instance:?}:{}", item.id))
            .collect();
        return virtual_list(
            format!("table:{instance:?}"),
            title,
            "table",
            Some(head),
            instance,
            keys,
            TABLE_ROW_ESTIMATE,
            false,
            |view, instance, index| match view {
                RyeOsViewVm::Table { rows, .. } => {
                    rows.get(index).map(|item| table_record(instance, item))
                }
                _ => None,
            },
        );
    }
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
        })
        .children(table_head(status, coverage, columns));
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

/// What a table shows above its rows: source status, coverage and the
/// column headings.
fn table_head(
    status: Option<&RyeOsSourceStatusVm>,
    coverage: Option<&RyeOsTableCoverageVm>,
    columns: &[String],
) -> Vec<El> {
    let p = Palette::get();
    let mut head = Vec::new();
    if let Some(status) = status {
        head.push(source_status(status));
    }
    if let Some(coverage) = coverage {
        head.push(
            text(format!("{} / {}", coverage.state, coverage.reason).to_uppercase())
                .font(theme::data(10.0).tracking(0.4))
                .color(p.muted)
                .wrap_anywhere()
                .px(9.0)
                .pb(10.0)
                .role("status"),
        );
    }
    let mut columns_row = row()
        .w_full()
        .min_h(38.0)
        .center_y()
        .role("row")
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line);
    for column in columns {
        columns_row = columns_row.child(
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
    columns_row = columns_row.child(
        col()
            .w(TABLE_ACTION_WIDTH)
            .center()
            .role("columnheader")
            .child(text("DETAIL").font(theme::data(9.0)).color(p.muted)),
    );
    head.push(columns_row);
    head
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

impl<'a> Timeline<'a> {
    /// The timeline a view shows, if it is one.
    fn of(view: &'a RyeOsViewVm) -> Option<Self> {
        match view {
            RyeOsViewVm::Timeline {
                entries,
                entry_ids,
                entry_indents,
                selected,
                entry_expandable,
                entry_expanded,
                entry_details,
                ..
            } => Some(Timeline {
                entries,
                ids: entry_ids,
                indents: entry_indents,
                selected: *selected,
                expandable: entry_expandable,
                expanded: entry_expanded,
                details: entry_details,
            }),
            _ => None,
        }
    }
}

/// Indent per call-tree level.
const TIMELINE_INDENT: f32 = 18.0;

fn timeline_view(
    instance: &RyeOsViewInstanceKey,
    title: &str,
    status: Option<&RyeOsSourceStatusVm>,
    timeline: Timeline<'_>,
) -> El {
    if timeline.entries.len() > VIRTUAL_ROWS_FROM {
        let keys = (0..timeline.entries.len())
            .map(|index| timeline_entry_key(instance, &timeline, index))
            .collect();
        return virtual_list(
            format!("timeline:{instance:?}"),
            title,
            "list",
            status.map(source_status),
            instance,
            keys,
            TIMELINE_ENTRY_ESTIMATE,
            true,
            |view, instance, index| {
                Timeline::of(view)
                    .filter(|timeline| index < timeline.entries.len())
                    .map(|timeline| timeline_entry(instance, &timeline, index))
            },
        );
    }
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
    for index in 0..timeline.entries.len() {
        body = body.child(timeline_entry(instance, &timeline, index));
    }
    body
}

/// Estimated height of a timeline entry not yet measured.
const TIMELINE_ENTRY_ESTIMATE: f32 = 44.0;

/// A timeline entry's key: its id, or its place when it has none.
fn timeline_entry_key(instance: &RyeOsViewInstanceKey, timeline: &Timeline<'_>, index: usize) -> String {
    format!(
        "entry:{instance:?}:{}",
        timeline
            .ids
            .get(index)
            .map_or_else(|| index.to_string(), Clone::clone)
    )
}

/// One timeline entry with its disclosure and, when expanded, its details.
fn timeline_entry(instance: &RyeOsViewInstanceKey, timeline: &Timeline<'_>, index: usize) -> El {
    let p = Palette::get();
    let entry = &timeline.entries[index];
    // The renderer never trusts the parallel arrays to be complete.
    let id = timeline.ids.get(index);
    let indent = f32::from(timeline.indents.get(index).copied().unwrap_or(0));
    let selected = timeline.selected == Some(index);
    let expandable = timeline.expandable.get(index).copied().unwrap_or(false);
    let expanded = timeline.expanded.get(index).copied().unwrap_or(false);
    let content = match entry {
        RyeOsTimelineEntryVm::Block { text: body, tone } => {
            text(theme::drawable(body, theme::Face::Ui).into_owned())
                .selectable(true)
                .font(theme::ui(13.0))
                .color(p.tone(*tone, p.text))
                .wrap()
                .grow(1.0)
                .min_w(0.0)
        }
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
        RyeOsTimelineEntryVm::Separator { label } => {
            text(theme::drawable(label, theme::Face::Data).to_uppercase())
                .font(theme::data(9.0))
                .color(p.faint)
        }
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
        .key(timeline_entry_key(instance, timeline, index))
        .w_full()
        .min_w(0.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(head);
    if expanded && let Some(rows) = timeline.details.get(index) {
        record = record.child(details(rows));
    }
    record
}

/// A timeline line: the primary text with its meta beside it.
fn entry_line(primary: &str, meta: Option<&str>, ink: rune::Color, weight: f32) -> El {
    let p = Palette::get();
    let mut line = row().grow(1.0).min_w(0.0).gap(12.0).center_y().child(
        text(theme::drawable(primary, theme::Face::Ui).into_owned())
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

/// A view waiting for a subject: what it needs, and the open selections that
/// can supply it.
fn required_subject(
    title: &str,
    input: &str,
    facets: &[String],
    actions: &[RyeOsTileIntentVm],
) -> El {
    let p = Palette::get();
    let mut block = col()
        .w_full()
        .grow(1.0)
        .justify_center()
        .gap(12.0)
        .padding(48.0)
        .role("status")
        .label(title.to_owned())
        .child(
            text("UNRESOLVED VIEW")
                .font(theme::data(9.0).tracking(1.2))
                .color(p.accent_bright),
        )
        .child(
            text(title.to_owned())
                .font(theme::ui(32.0))
                .color(p.text)
                .wrap()
                .w_full(),
        )
        .child(
            text(format!("{input} needs {}.", facets.join(", ")))
                .font(theme::ui(13.0))
                .color(p.muted)
                .wrap()
                .max_w(560.0),
        );
    if actions.is_empty() {
        return block.child(
            text("Select a compatible subject in another open set.")
                .font(theme::data(10.0))
                .color(p.faint),
        );
    }
    let mut choices = row()
        .flex_wrap()
        .gap(8.0)
        .mt(8.0)
        .role("group")
        .label("Compatible current selections");
    for action in actions {
        let intent = action.intent.clone();
        choices = choices.child(
            text(action.label.clone())
                .font(theme::ui(13.0))
                .color(p.text)
                .px(12.0)
                .py(9.0)
                .border(1.0, p.line_strong)
                .fill(p.plane)
                .hover(p.plane_selected)
                .role("button")
                .label(action.title.clone())
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::Activate {
                        intent: intent.clone(),
                    });
                }),
        );
    }
    block = block.child(choices);
    block
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

#[cfg(test)]
mod tests {
    use rune::{Key, KeyEvent, Mods};

    use super::is_launcher_chord;

    fn press(key: Key, mods: Mods) -> KeyEvent {
        KeyEvent { key, mods }
    }

    /// K with exactly one of Ctrl and Alt opens the launcher from anywhere;
    /// Shift, the Super key, both modifiers or neither leave it to typing.
    #[test]
    fn only_the_admitted_launcher_chord_escapes_typing() {
        let with = |ctrl, alt, shift, logo| Mods {
            ctrl,
            alt,
            shift,
            logo,
            ..Mods::default()
        };
        let k = || Key::Text("k".to_owned());
        assert!(is_launcher_chord(&press(k(), with(true, false, false, false))));
        assert!(is_launcher_chord(&press(k(), with(false, true, false, false))));
        assert!(is_launcher_chord(&press(
            Key::Command('k'),
            with(true, false, false, false)
        )));
        assert!(is_launcher_chord(&press(
            Key::Text("K".to_owned()),
            with(false, true, false, false)
        )));
        for mods in [
            with(false, false, false, false),
            with(true, true, false, false),
            with(true, false, true, false),
            with(false, true, false, true),
        ] {
            assert!(!is_launcher_chord(&press(k(), mods)), "{mods:?}");
        }
        assert!(!is_launcher_chord(&press(
            Key::Text("j".to_owned()),
            with(true, false, false, false)
        )));
    }

    // ---- Rows and table rendering ----

    use ryeos_client_base::ids::{PresentationId, RyeOsViewInstanceKey};
    use ryeos_client_base::ui::view_model::RyeOsViewVm;
    use serde_json::{Value, json};

    use super::body;
    use crate::App;
    use crate::probe::{runtime, says, texts};

    fn instance() -> RyeOsViewInstanceKey {
        let presentation =
            PresentationId::try_new("11111111-1111-4111-8111-111111111111").unwrap();
        RyeOsViewInstanceKey::shared_dock(&presentation, "left").unwrap()
    }

    fn table_row(id: &str, cells: [&str; 2], extra: Value) -> Value {
        let mut row = json!({
            "id": id, "cells": cells, "tone": "neutral", "selected": false,
            "expandable": false, "expanded": false, "detail": []
        });
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            row[key] = value;
        }
        row
    }

    fn table(rows: Vec<Value>, total_rows: usize, coverage: Value) -> RyeOsViewVm {
        serde_json::from_value(json!({
            "type": "table", "title": "Threads", "columns": ["name", "kind"],
            "total_rows": total_rows, "coverage": coverage, "rows": rows
        }))
        .unwrap()
    }

    fn list_row(id: &str, primary: &str, kind: Option<&str>, extra: Value) -> Value {
        let mut row = json!({
            "id": id, "primary": primary, "secondary": null, "meta": null, "kind": kind,
            "intent": null, "tone": "neutral", "selected": false
        });
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            row[key] = value;
        }
        row
    }

    fn rows(rows: Vec<Value>) -> RyeOsViewVm {
        let total = rows.len();
        serde_json::from_value(json!({
            "type": "rows", "title": "Items", "columns": [], "total_rows": total, "rows": rows
        }))
        .unwrap()
    }

    fn drawn(view: RyeOsViewVm) -> rune::Runtime<App> {
        let instance = instance();
        let local = std::rc::Rc::new(crate::Local::default());
        runtime((900.0, 700.0), move || body(&view, &instance, &local))
    }

    /// An element's path, its parent's path, and whether it takes focus.
    fn find(rt: &rune::Runtime<App>, label: &str) -> (String, bool) {
        rt.elements()
            .into_iter()
            .find(|element| element.label.as_deref() == Some(label))
            .map(|element| (element.path, element.focusable))
            .unwrap_or_else(|| panic!("no element labelled {label:?}"))
    }

    fn parent(path: &str) -> &str {
        path.rsplit_once('/').map_or("", |(parent, _)| parent)
    }

    fn labelled_containing(rt: &rune::Runtime<App>, part: &str) -> Vec<String> {
        rt.elements()
            .into_iter()
            .filter_map(|element| element.label)
            .filter(|label| label.contains(part))
            .collect()
    }

    fn hierarchical() -> RyeOsViewVm {
        table(
            vec![
                table_row(
                    "t1",
                    ["Alpha", "beta"],
                    json!({"hierarchy": {
                        "ancestor_continues": [true], "is_last": false,
                        "has_children": true, "collapsed": false
                    }}),
                ),
                table_row(
                    "t2",
                    ["Leaf", "gamma"],
                    json!({"hierarchy": {
                        "ancestor_continues": [], "is_last": true,
                        "has_children": false, "collapsed": false
                    }}),
                ),
            ],
            2,
            Value::Null,
        )
    }

    #[test]
    fn hierarchy_is_drawn_only_in_the_first_column() {
        let rt = drawn(hierarchical());
        // One guide for the branch row ("│├") and one for the leaf ("└"):
        // the second column of each row carries none.
        let shown = texts(&rt);
        assert_eq!(shown.iter().filter(|text| *text == "│├").count(), 1);
        assert_eq!(shown.iter().filter(|text| *text == "└").count(), 1);
        assert_eq!(shown.iter().filter(|text| *text == "─").count(), 1);
        let (alpha, _) = find(&rt, "Select row: Alpha");
        let (beta, _) = find(&rt, "Select row: beta");
        assert_ne!(parent(&alpha), parent(&beta), "each cell has its own slot");
        let toggle = labelled_containing(&rt, "descendants of");
        assert_eq!(toggle, vec!["Collapse descendants of Alpha".to_owned()]);
        let (toggle_path, _) = find(&rt, "Collapse descendants of Alpha");
        assert!(
            toggle_path.starts_with(&format!("{}/", parent(&alpha))),
            "the tree toggle sits in the first column"
        );
        assert!(
            !toggle_path.starts_with(&format!("{}/", parent(&beta))),
            "and not in the second"
        );
    }

    #[test]
    fn a_collapsed_branch_offers_to_expand_its_descendants() {
        let mut view = hierarchical();
        let RyeOsViewVm::Table { rows, .. } = &mut view else {
            unreachable!()
        };
        rows[0].hierarchy.as_mut().unwrap().collapsed = true;
        let rt = drawn(view);
        assert_eq!(
            labelled_containing(&rt, "descendants of"),
            vec!["Expand descendants of Alpha".to_owned()]
        );
    }

    #[test]
    fn the_tree_toggle_is_not_part_of_the_row_press() {
        let rt = drawn(hierarchical());
        let (select, _) = find(&rt, "Select row: Alpha");
        let (toggle, _) = find(&rt, "Collapse descendants of Alpha");
        assert!(!select.starts_with(&format!("{toggle}/")) && !toggle.starts_with(&format!("{select}/")));
        // A press at the centre of each lands on its own control.
        let centre = |path: &str| {
            let element = rt
                .elements()
                .into_iter()
                .find(|element| element.path == path)
                .unwrap();
            let [x, y, w, h] = element.rect;
            (x + w / 2.0, y + h / 2.0)
        };
        let (x, y) = centre(&toggle);
        let hit = rt.element_at(x, y).expect("the toggle can be pressed");
        assert!(hit.path().starts_with(&toggle), "{}", hit.path());
        let (x, y) = centre(&select);
        let hit = rt.element_at(x, y).expect("the cell can be pressed");
        assert!(hit.path().starts_with(&select), "{}", hit.path());
        assert!(!hit.path().starts_with(&toggle));
    }

    #[test]
    fn a_flat_table_row_draws_no_tree_prefix() {
        let rt = drawn(table(
            vec![table_row("t1", ["Alpha", "beta"], json!({}))],
            1,
            Value::Null,
        ));
        assert!(labelled_containing(&rt, "descendants of").is_empty());
        assert!(!says(&rt, "─"));
    }

    /// Rows never take the renderer's focus: the keyboard moves the core's
    /// selection. An unavailable row is drawn dimmed.
    #[test]
    fn unavailable_rows_are_dimmed_and_no_row_takes_renderer_focus() {
        let rt = drawn(rows(vec![
            list_row("r1", "Blocked", Some("unavailable"), json!({})),
            list_row("r2", "Fine", None, json!({})),
        ]));
        let row = |label: &str| {
            rt.elements()
                .into_iter()
                .find(|element| element.label.as_deref() == Some(label))
                .unwrap_or_else(|| panic!("no element labelled {label:?}"))
        };
        let (blocked, fine) = (row("Blocked"), row("Fine"));
        assert!(!blocked.focusable && !fine.focusable);
        assert!(blocked.opacity < fine.opacity, "{} vs {}", blocked.opacity, fine.opacity);
    }

    #[test]
    fn a_detail_record_is_only_its_details() {
        let rt = drawn(rows(vec![
            list_row("r1", "Heading", None, json!({})),
            list_row(
                "r2",
                "Hidden primary",
                Some("detail"),
                json!({"detail": [{"field": "Path", "value": "/srv/data"}]}),
            ),
        ]));
        assert!(says(&rt, "Path"));
        assert!(says(&rt, "/srv/data"));
        assert!(!says(&rt, "Hidden primary"), "a detail record has no title line");
        assert!(labelled_containing(&rt, "Hidden primary").is_empty());
    }

    #[test]
    fn a_rows_detail_shows_only_when_expanded() {
        let detail = json!([{"field": "Owner", "value": "ops-team"}]);
        let closed = drawn(rows(vec![list_row(
            "r1",
            "Item",
            None,
            json!({"expandable": true, "expanded": false, "detail": detail}),
        )]));
        assert!(!says(&closed, "ops-team"));
        assert_eq!(
            labelled_containing(&closed, "Expand Item"),
            vec!["Expand Item".to_owned()]
        );
        let open = drawn(rows(vec![list_row(
            "r1",
            "Item",
            None,
            json!({"expandable": true, "expanded": true, "detail": detail}),
        )]));
        assert!(says(&open, "ops-team"));
        assert_eq!(
            labelled_containing(&open, "Collapse Item"),
            vec!["Collapse Item".to_owned()]
        );
    }

    #[test]
    fn a_table_detail_shows_only_when_expanded() {
        let detail = json!([{"field": "Owner", "value": "ops-team"}]);
        let closed = drawn(table(
            vec![table_row(
                "t1",
                ["Alpha", "beta"],
                json!({"expandable": true, "expanded": false, "detail": detail}),
            )],
            1,
            Value::Null,
        ));
        assert!(!says(&closed, "ops-team"));
        assert_eq!(
            labelled_containing(&closed, "Expand row"),
            vec!["Expand row".to_owned()]
        );
        let open = drawn(table(
            vec![table_row(
                "t1",
                ["Alpha", "beta"],
                json!({"expandable": true, "expanded": true, "detail": detail}),
            )],
            1,
            Value::Null,
        ));
        assert!(says(&open, "ops-team"));
    }

    #[test]
    fn an_empty_table_page_says_so_and_a_populated_one_does_not() {
        let empty = drawn(table(Vec::new(), 0, Value::Null));
        assert!(says(&empty, "No rows in this page."));
        let populated = drawn(table(
            vec![table_row("t1", ["Alpha", "beta"], json!({}))],
            1,
            Value::Null,
        ));
        assert!(!says(&populated, "No rows in this page."));
        // The page window can be empty while the collection is not.
        let windowed = drawn(table(Vec::new(), 5, Value::Null));
        assert!(!says(&windowed, "No rows in this page."));
    }

    #[test]
    fn the_coverage_line_is_shown_whether_or_not_there_are_rows() {
        let coverage = json!({"state": "partial", "reason": "page_limit"});
        let empty = drawn(table(Vec::new(), 0, coverage.clone()));
        assert!(says(&empty, "PARTIAL / PAGE_LIMIT"));
        assert!(says(&empty, "No rows in this page."));
        let populated = drawn(table(
            vec![table_row("t1", ["Alpha", "beta"], json!({}))],
            1,
            coverage,
        ));
        assert!(says(&populated, "PARTIAL / PAGE_LIMIT"));
        let unreported = drawn(table(
            vec![table_row("t1", ["Alpha", "beta"], json!({}))],
            1,
            Value::Null,
        ));
        assert!(!texts(&unreported).iter().any(|text| text.contains(" / ")));
    }
}
