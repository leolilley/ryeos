//! View bodies, tile frames and key translation.
//!
//! Every view kind the core projects is matched here without a wildcard, so a
//! new kind fails to compile until this renderer decides how to draw it.

use rune::{Align, Color, El, Key, KeyEvent, ScrollSpec, Side, col, el, row, text};
use ryeos_client_base::ids::RyeOsViewInstanceKey;
use ryeos_client_base::ui::content::ColumnRole;
use ryeos_client_base::ui::timeline::RyeOsTimelineEntryVm;

use ryeos_client_base::ui::view_model::{
    RyeOsInputVm, RyeOsRowDetailVm, RyeOsRowVm, RyeOsSectionVm, RyeOsSourceStatusVm,
    RyeOsTableHierarchyVm, RyeOsTableRowVm, RyeOsTextLineVm, RyeOsTileIntentVm,
    RyeOsTimelineDockActionVm, RyeOsTimelineDockVm, RyeOsTimelineLaneVm, RyeOsTone,
    RyeOsViewHeadingVm, RyeOsViewSupplementVm, RyeOsViewTabVm, RyeOsViewVm,
};
use ryeos_client_base::ui::{
    RyeOsKey, RyeOsKeyEvent, RyeOsKeyModifiers, RyeOsUiEvent, RyeOsUiIntent,
};

use crate::App;
use crate::theme::{self, Palette, Tokens};

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

/// A tile's title: `type.title` at `ink.strong`.
fn tile_title(label: &str) -> El {
    let t = Tokens::get();
    text(label.to_owned())
        .font(theme::TYPE_TITLE)
        .color(t.ink_strong())
        .truncate()
}

/// Height of a tile's title row.
const TITLE_ROW_HEIGHT: f32 = 44.0;

/// What a tile adds to a frame's header beyond its title.
pub struct TileChrome<'a> {
    pub tile_id: &'a str,
    pub maximized: bool,
    pub intents: &'a [RyeOsTileIntentVm],
    pub tabs: &'a [RyeOsViewTabVm],
    /// A grouped tile puts its tabs on their own row.
    pub grouped: bool,
}

/// One mounted view in its frame: a layout tile.
pub struct FrameSpec<'a> {
    pub key: String,
    pub instance: &'a RyeOsViewInstanceKey,
    pub label: &'a str,
    pub detail: Option<&'a str>,
    pub attachment: Option<&'a str>,
    pub focused: bool,
    /// The set shows this tile alone: there is no other tile for focus to
    /// tell it from, so it draws no focus edge and keeps its tools at rest.
    pub sole: bool,
    /// An estimate of the tile's width, which tables fit their columns to.
    pub width: f32,
    pub transparent: bool,
    pub chrome_hidden: bool,
    /// `None` for a frame without tile tools.
    pub tile: Option<TileChrome<'a>>,
    pub heading: Option<&'a RyeOsViewHeadingVm>,
    pub supplement: Option<&'a RyeOsViewSupplementVm>,
    pub view: &'a RyeOsViewVm,
    pub input: Option<&'a RyeOsInputVm>,
    /// Set when the view is project-authored; drawn beside the label.
    pub authorship: Option<&'a ryeos_client_base::ui::content::ItemAuthorship>,
}

/// The key of a tile's actions menu, and of the control that opens it.
pub fn menu_key(instance: &RyeOsViewInstanceKey) -> String {
    format!("menu:{instance:?}")
}

/// How much of the tile fill a tile with a transparent background keeps.
const TRANSPARENT_TILE_FILL: f32 = 0.8;

/// A bordered frame: header, heading, the view body, supplement footer and
/// the input composer, in the order the browser shell draws them.
pub fn frame(spec: FrameSpec<'_>, local: &crate::Local) -> El {
    let t = Tokens::get();
    // A tile: `sheet.fill` and a 1 px edge, `sheet.focus` when it holds focus.
    let mut fill = t.sheet_fill();
    if spec.transparent {
        fill = fill.alpha(fill.a * TRANSPARENT_TILE_FILL);
    }
    let mut frame = col()
        .key(spec.key.clone())
        .w_full()
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .fill(fill)
        .radius(theme::RADIUS_TILE)
        .border(
            1.0,
            if spec.focused && !spec.sole {
                t.sheet_focus()
            } else {
                t.sheet_border()
            },
        )
        // Content taller than its slot stays inside the frame.
        .clip_rounded(theme::RADIUS_TILE);
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
    frame = frame.child(
        col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(body_in(spec.view, spec.instance, local, spec.width)),
    );
    // The status lane and the pending asks close the feed, directly above the
    // composer in the same tile.
    if let RyeOsViewVm::Timeline { lane, dock, .. } = spec.view {
        frame = frame.child(timeline_foot(spec.instance, lane.as_ref(), dock));
    }
    if let Some(foot) = table_foot(spec.view) {
        frame = frame.child(foot);
    }
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
    let t = Tokens::get();
    let identity = row().center_y().min_w(0.0).child(tile_title(spec.label));
    let mut title_row = row()
        .w_full()
        .min_h(TITLE_ROW_HEIGHT)
        .px(theme::PAD_TILE)
        .center_y()
        .gap(10.0)
        .child(identity.grow(1.0));
    // The mark for a project-authored view: always drawn when present, at
    // the right of the title row, in ink (never a warning tone). The title
    // truncates before the mark does.
    if let Some(authorship) = spec.authorship {
        title_row = title_row.child(authorship_chip(authorship));
    }
    let Some(tile) = &spec.tile else {
        // A frame without tile tools names its attachment or its detail, as
        // the right note.
        let aside = spec
            .attachment
            .or(spec.detail)
            .map(str::to_owned)
            .unwrap_or_default();
        return title_row.child(frame_detail(&aside));
    };
    let tabs = (tile.tabs.len() > 1).then(|| view_tabs(tile.tabs, tile.grouped));
    if !tile.grouped
        && let Some(tabs) = tabs.as_ref().cloned()
    {
        title_row = title_row.child(tabs);
    }
    // The right note: what the view says about itself, else the thread a
    // timeline follows.
    let note = spec
        .detail
        .filter(|detail| !detail.is_empty())
        .or(match spec.view {
            RyeOsViewVm::Timeline { thread_id, .. } => thread_id.as_deref(),
            _ => None,
        });
    if let Some(detail) = note {
        title_row = title_row.child(frame_detail(detail));
    }
    let mut tools = row().center_y().gap(8.0);
    if let Some(attachment) = spec.attachment {
        tools = tools.child(frame_detail(attachment).label("Selection attachment"));
    }
    let tile_id = tile.tile_id.to_owned();
    let maximized = tile.maximized;
    // The tools are drawn only on a tile that holds focus among others; at
    // rest they keep their place and stay pressable, and a hover shows their
    // pad.
    let tool_ink = if spec.focused && !spec.sole {
        t.ink_dim()
    } else {
        Color::TRANSPARENT
    };
    tools = tools.child(
        col()
            .child(
                text(if maximized { "↙" } else { "↗" })
                    .font(theme::TYPE_LINE)
                    .color(tool_ink),
            )
            .padding(2.0)
            .radius(theme::RADIUS_ROW)
            .hover(t.select_fill())
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
                    if open { t.accent } else { tool_ink },
                ))
                .padding(5.0)
                .radius(theme::RADIUS_ROW)
                .hover(t.select_fill())
                .role("button")
                .label(format!("Actions for {}", spec.label))
                .expanded(open)
                .on_press(move |app: &mut App| app.toggle_menu(toggle.clone())),
        );
    }
    title_row = title_row.child(tools);
    let mut header = col().w_full().child(title_row);
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

/// The chip that marks a project-authored view: the word `project` (plain
/// key; no vocabulary reaches the frame), a separator, then the signer's name
/// (or fingerprint prefix), or the trust word when there is no signer. It is
/// ink on ink, never a tone: a project-authored view is not a fault.
fn authorship_chip(authorship: &ryeos_client_base::ui::content::ItemAuthorship) -> El {
    let t = Tokens::get();
    row()
        .h(22.0)
        .px(9.0)
        .gap(7.0)
        .center_y()
        .radius(theme::radius_pill(22.0))
        .border(1.0, t.ink_at(0.22))
        .label(format!("Project view, {}", authorship.mark_tail()))
        .child(text("project").font(theme::TYPE_NOTE).color(t.ink_soft()))
        .child(el().size(1.0, 10.0).fill(t.ink_at(0.18)))
        .child(
            text(authorship.mark_tail())
                .font(theme::medium(theme::TYPE_NOTE))
                .color(t.ink_at(0.85))
                .truncate(),
        )
}

/// A tile's right note: `type.small` at `ink.dim`.
fn frame_detail(detail: &str) -> El {
    let t = Tokens::get();
    text(detail.to_owned())
        .font(theme::TYPE_SMALL)
        .color(t.ink_dim())
        .truncate()
}

fn view_tabs(tabs: &[RyeOsViewTabVm], grouped: bool) -> El {
    let t = Tokens::get();
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
                .min_h(TITLE_ROW_HEIGHT)
                .px(13.0)
                .center_y()
                .border_sides(
                    [0.0, 0.0, 1.0, 0.0],
                    if tab.active {
                        t.ink_strong()
                    } else {
                        Color::TRANSPARENT
                    },
                )
                .role("tab")
                .focus_selected(tab.active)
                .child(
                    text(tab.title.clone())
                        .font(theme::TYPE_SMALL)
                        .color(if tab.active {
                            t.ink_strong()
                        } else {
                            t.ink_dim()
                        }),
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
    body_in(view, instance, local, f32::MAX)
}

/// The view's body in a tile about `width` wide: a table fits its columns to
/// the width.
fn body_in(
    view: &RyeOsViewVm,
    instance: &RyeOsViewInstanceKey,
    local: &crate::Local,
    width: f32,
) -> El {
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
            column_roles,
            total_rows,
            rows,
            ..
        } => table_view(
            instance,
            title,
            source_status.as_ref(),
            TableShape {
                columns,
                roles: column_roles,
                width,
            },
            *total_rows,
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

/// The inset and spacing of a scrolling list body.
#[derive(Clone, Copy)]
struct ListPad {
    x: f32,
    top: f32,
    bottom: f32,
    /// Between rows.
    gap: f32,
    /// Rows sit in a centred column no wider than this.
    column: Option<f32>,
}

/// The list views' inset.
const LIST_PAD: ListPad = ListPad {
    x: 16.0,
    top: 12.0,
    bottom: 12.0,
    gap: 0.0,
    column: None,
};

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
            LIST_PAD,
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
    pad: ListPad,
    build: impl Fn(&RyeOsViewVm, &RyeOsViewInstanceKey, usize) -> Option<El> + 'static,
) -> El {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    keys.hash(&mut hasher);
    let version = hasher.finish();
    let count = keys.len();
    let keys = std::rc::Rc::new(keys);
    let build_instance = instance.clone();
    let mut rows = col()
        .key(format!("{key}:rows"))
        .w_full()
        .pt(pad.top)
        .pb(pad.bottom)
        .gap(pad.gap);
    rows = match pad.column {
        Some(width) => rows.max_w(width),
        None => rows.px(pad.x),
    };
    let rows = rows
        .growth(rune::GrowthSpec {
            spring: rune::GROWTH,
            edge: rune::edge::NO_MASK,
        })
        .virtual_rows(
            count,
            estimate,
            move |_: &App, index| {
                std::borrow::Cow::Owned(keys.get(index).cloned().unwrap_or_default())
            },
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
        body = body.child(col().w_full().px(pad.x).pt(pad.top).child(head));
    }
    let mut scroll = col()
        .key(format!("{key}:scroll"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: follow,
        });
    if pad.column.is_some() {
        // A feed's first visible line is usually cut by the viewport: fade it.
        scroll = scroll
            .px(pad.x)
            .items_center()
            .scroll_edge_fade(TIMELINE_FADE);
    }
    body.child(scroll.child(rows))
}

/// Above a section's title, and below it.
const SECTION_TITLE_ABOVE: f32 = 14.0;
const SECTION_TITLE_BELOW: f32 = 4.0;
/// A section's field row.
const FIELD_ROW_HEIGHT: f32 = 25.0;
const FIELD_ROW_GAP: f32 = 10.0;
/// The fixed width of a field's name.
const FIELD_NAME_WIDTH: f32 = 120.0;
/// A field row sits this far in from the tile's padding, so its selection
/// fill has a margin.
const FIELD_INSET: f32 = 8.0;
/// A tone's dot beside its words.
const TONE_DOT: f32 = 6.0;
/// Between a status dot and its word.
const TONE_DOT_GAP: f32 = 7.0;

/// A tone's dot: the tone's colour, or nothing at all.
fn tone_dot(tone: Option<Color>, size: f32) -> Option<El> {
    tone.map(|color| el().size(size, size).radius(size / 2.0).fill(color))
}

fn sections_view(instance: &RyeOsViewInstanceKey, title: &str, sections: &[RyeOsSectionVm]) -> El {
    let mut body = col()
        .key(format!("sections:{instance:?}"))
        .w_full()
        .shrink()
        .min_h(0.0)
        .px(theme::PAD_TILE - FIELD_INSET)
        .py(8.0)
        .label(title.to_owned())
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    for section in sections {
        body = body.child(section_block(instance, section));
    }
    body
}

fn section_block(instance: &RyeOsViewInstanceKey, section: &RyeOsSectionVm) -> El {
    let t = Tokens::get();
    let ink = if section.header_selected {
        t.ink_strong()
    } else {
        t.ink_dim()
    };
    let fold = if section.collapsed {
        &crate::icons::COLLAPSED
    } else {
        &crate::icons::EXPANDED
    };
    let toggle_key = instance.clone();
    let section_id = section.id.clone();
    let mut header = row()
        .key(format!("section:{instance:?}:{}", section.id))
        .w_full()
        .mt(SECTION_TITLE_ABOVE)
        .mb(SECTION_TITLE_BELOW)
        .center_y()
        .gap(8.0)
        .hover(t.select_fill())
        .role("button")
        .expanded(!section.collapsed)
        .child(
            text(section.title.to_uppercase())
                .font(theme::TYPE_LABEL)
                .color(ink)
                .truncate()
                .grow(1.0),
        )
        .on_press(move |app: &mut App| {
            app.ui(RyeOsUiEvent::ToggleViewSection {
                instance_key: toggle_key.clone(),
                section_id: section_id.clone(),
            });
        });
    if section.header_selected {
        header = header.fill(t.select_fill()).radius(theme::RADIUS_ROW);
    }
    // The fold mark shows only where the fold is not the usual state: a
    // folded section, or the one the point is on.
    if section.collapsed || section.header_selected {
        header = header.child(crate::icons::glyph(fold, 10.0, t.ink_mute()));
    }
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
    let t = Tokens::get();
    let mut wrapper = col()
        .key(format!("item:{instance:?}:{}", item.id))
        .w_full()
        .min_w(0.0)
        .role("listitem");
    if style == RowStyle::Rows {
        wrapper = wrapper.border_sides([0.0, 0.0, 1.0, 0.0], p.line);
    }
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
        line = match style {
            RowStyle::Rows => line.fill(p.plane_selected),
            RowStyle::Section => line.fill(t.select_fill()).radius(theme::RADIUS_ROW),
        };
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
            // A field row: its name, fixed width, then its value; a toned
            // row puts the tone's dot before the value's words.
            let aside = item.meta.as_ref().or(item.secondary.as_ref());
            let mut value = row().grow(1.0).min_w(0.0).gap(TONE_DOT_GAP).center_y();
            if let Some(dot) = tone_dot(t.tone(item.tone), TONE_DOT) {
                value = value.child(dot);
            }
            // A value is read whole: it wraps rather than truncates.
            let words = |what: &String| {
                text(what.clone())
                    .font(theme::TYPE_CELL)
                    .color(t.ink_body())
                    .wrap_anywhere()
                    .grow(1.0)
                    .min_w(0.0)
            };
            let line = line
                .min_h(FIELD_ROW_HEIGHT)
                .px(FIELD_INSET)
                .gap(FIELD_ROW_GAP);
            match aside {
                Some(aside) => line
                    .child(
                        text(item.primary.clone())
                            .font(theme::TYPE_SMALL)
                            .color(if item.selected {
                                t.ink_strong()
                            } else {
                                t.ink_dim()
                            })
                            .truncate()
                            .w(FIELD_NAME_WIDTH),
                    )
                    .child(value.child(words(aside))),
                None => line.child(value.child(words(&item.primary))),
            }
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
    // A field row shows its disclosure only while it is the one the point is
    // on, or open, and as a bare mark rather than a button.
    let rows_style = style == RowStyle::Rows;
    if item.expandable && (rows_style || item.selected || item.expanded) {
        let expand_key = instance.clone();
        let item_id = item.id.clone();
        let expanded = item.expanded;
        head = head.child(
            col()
                .w(34.0)
                .center()
                .border_sides(
                    [0.0, 0.0, 0.0, if rows_style { 1.0 } else { 0.0 }],
                    p.line,
                )
                .fill(if !rows_style {
                    Color::TRANSPARENT
                } else if expanded {
                    p.plane_selected
                } else {
                    p.plane
                })
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

/// The disclosure column of a table whose rows do not open on activation.
const TABLE_ACTION_WIDTH: f32 = 24.0;

/// A table's header row, and the ink of its labels.
const TABLE_HEADER_HEIGHT: f32 = 28.0;
const TABLE_HEADER_INK: f32 = 0.40;
/// A table row: its height, its padding either side, and the gap between its
/// cells. Rows sit 1 apart, inset from the tile so a selection fill has a
/// margin.
const TABLE_ROW_HEIGHT: f32 = 28.0;
/// A hierarchy table's row.
const TABLE_ROW_HEIGHT_TREE: f32 = 26.0;
const TABLE_ROW_PAD: f32 = 10.0;
const TABLE_CELL_GAP: f32 = 12.0;
const TABLE_ROW_GAP: f32 = 1.0;
const TABLE_INSET: f32 = 8.0;
/// Indent per level of a hierarchical table.
const TABLE_INDENT: f32 = 16.0;
/// The width of the fold mark's place, so rows without children line up.
const FOLD_MARK_WIDTH: f32 = 16.0;

/// The table's padding: rows inset from the tile.
const TABLE_PAD: ListPad = ListPad {
    x: TABLE_INSET,
    top: 8.0,
    bottom: 8.0,
    gap: TABLE_ROW_GAP,
    column: None,
};

/// Whether a source's status says it could not be read.
fn unreadable(status: Option<&RyeOsSourceStatusVm>) -> bool {
    status.is_some_and(|status| is_unreadable(&status.state))
}

fn is_unreadable(state: &str) -> bool {
    matches!(state, "failed" | "unavailable" | "unresolved")
}

/// What a table is drawn from besides its rows: the headings, what each
/// column holds, and about how wide the tile is.
#[derive(Clone, Copy)]
struct TableShape<'a> {
    columns: &'a [String],
    roles: &'a [ColumnRole],
    width: f32,
}

fn table_view(
    instance: &RyeOsViewInstanceKey,
    title: &str,
    status: Option<&RyeOsSourceStatusVm>,
    shape: TableShape<'_>,
    total_rows: usize,
    rows: &[RyeOsTableRowVm],
) -> El {
    let t = Tokens::get();
    let action = table_has_action(rows);
    let plan = table_plan(shape, action);
    if rows.len() > VIRTUAL_ROWS_FROM {
        let head = col().w_full().children(table_head(status, shape, &plan, action));
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
            TABLE_PAD,
            move |view, instance, index| match view {
                RyeOsViewVm::Table {
                    rows, column_roles, ..
                } => rows
                    .get(index)
                    .map(|item| table_record(instance, item, column_roles, &plan, action)),
                _ => None,
            },
        );
    }
    let mut body = col()
        .key(format!("table:{instance:?}"))
        .w_full()
        .shrink()
        .min_h(0.0)
        .px(TABLE_PAD.x)
        .pt(TABLE_PAD.top)
        .pb(TABLE_PAD.bottom)
        .gap(TABLE_ROW_GAP)
        .role("table")
        .label(title.to_owned())
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .children(table_head(status, shape, &plan, action));
    // A source that could not be read is never drawn as an empty page.
    if total_rows == 0 && !unreadable(status) {
        body = body.child(
            text("Nothing observed yet.")
                .font(theme::TYPE_SMALL)
                .color(t.ink_dim())
                .px(TABLE_ROW_PAD)
                .py(TABLE_CELL_GAP)
                .role("status"),
        );
    }
    for item in rows {
        body = body.child(table_record(instance, item, shape.roles, &plan, action));
    }
    body
}

/// Whether a table draws its disclosure column. Rows that open on activation
/// are their own affordance (the key that expands the selected row still
/// works); a table whose rows only expand keeps the column.
fn table_has_action(rows: &[RyeOsTableRowVm]) -> bool {
    rows.iter().any(|row| row.expandable) && rows.iter().all(|row| row.intent.is_none())
}

/// The coverage sentence at the foot of a table's tile, above a rule.
pub fn table_foot(view: &RyeOsViewVm) -> Option<El> {
    let RyeOsViewVm::Table {
        coverage: Some(coverage),
        ..
    } = view
    else {
        return None;
    };
    let t = Tokens::get();
    Some(
        col()
            .w_full()
            .border_sides([1.0, 0.0, 0.0, 0.0], t.rule())
            .child(
                text(coverage.label.clone())
                    .font(theme::TYPE_NOTE)
                    .color(t.tone(coverage.tone).unwrap_or_else(|| t.ink_faint()))
                    .truncate()
                    .w_full()
                    .px(theme::PAD_TILE)
                    .py(11.0)
                    .role("status"),
            ),
    )
}

/// What a table shows above its rows: source status and the column headings.
fn table_head(
    status: Option<&RyeOsSourceStatusVm>,
    shape: TableShape<'_>,
    plan: &[Option<ColumnWidth>],
    action: bool,
) -> Vec<El> {
    let t = Tokens::get();
    let mut head = Vec::new();
    if let Some(status) = status {
        head.push(source_status(status));
    }
    let mut columns_row = row()
        .w_full()
        .min_h(TABLE_HEADER_HEIGHT)
        .px(TABLE_ROW_PAD)
        .gap(TABLE_CELL_GAP)
        .center_y()
        .role("row");
    for (index, column) in shape.columns.iter().enumerate() {
        let Some(width) = plan.get(index).copied().flatten() else {
            continue;
        };
        let role = role_at(shape.roles, index);
        let label = text(column.to_uppercase())
            .font(theme::TYPE_LABEL)
            .color(t.ink_at(TABLE_HEADER_INK))
            .truncate()
            .min_w(0.0);
        // A right-aligned column's heading sits over its figures.
        let cell = if role == ColumnRole::Count {
            row().justify_end().child(label)
        } else {
            row().child(label)
        };
        columns_row = columns_row.child(sized(cell, width).min_w(0.0).role("columnheader"));
    }
    if action {
        columns_row = columns_row.child(col().w(TABLE_ACTION_WIDTH).role("columnheader"));
    }
    head.push(columns_row);
    head
}

/// The role of column `index`; a column with no role entry is text.
fn role_at(roles: &[ColumnRole], index: usize) -> ColumnRole {
    roles.get(index).copied().unwrap_or_default()
}

/// How wide a column is: a fixed width, or a share of the room left over.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ColumnWidth {
    Fixed(f32),
    Grow {
        min: f32,
        weight: f32,
        max: Option<f32>,
    },
}

impl ColumnWidth {
    /// The least room the column needs.
    fn least(self) -> f32 {
        match self {
            Self::Fixed(width) => width,
            Self::Grow { min, .. } => min,
        }
    }
}

/// A text column's least width, and the widest it grows beside a ref or path
/// column, which takes the room instead.
const TEXT_LEAST: f32 = 80.0;
const TEXT_MOST: f32 = 160.0;
/// A ref or path column's least width.
const REF_LEAST: f32 = 150.0;

/// The width a role asks for. Ids, kinds, statuses, times, counts and hashes
/// are fixed so rows line up; refs and paths take the room left over, with
/// text columns beside them held to a bounded share. With no ref or path
/// column, text columns share the room equally.
fn role_width(role: ColumnRole, has_ref: bool) -> ColumnWidth {
    match role {
        ColumnRole::Id => ColumnWidth::Fixed(112.0),
        ColumnRole::Kind => ColumnWidth::Fixed(78.0),
        ColumnRole::Status => ColumnWidth::Fixed(104.0),
        ColumnRole::Time => ColumnWidth::Fixed(100.0),
        ColumnRole::Hash => ColumnWidth::Fixed(84.0),
        ColumnRole::Count => ColumnWidth::Fixed(56.0),
        ColumnRole::Action => ColumnWidth::Fixed(TABLE_ACTION_WIDTH),
        ColumnRole::Ref | ColumnRole::Path => ColumnWidth::Grow {
            min: REF_LEAST,
            weight: 2.0,
            max: None,
        },
        ColumnRole::Text => ColumnWidth::Grow {
            min: TEXT_LEAST,
            weight: 1.0,
            max: has_ref.then_some(TEXT_MOST),
        },
    }
}

/// When a tile is too narrow for every column, columns go in this order,
/// lowest first. `None` never goes: an id says which row it is and a status
/// says what happened to it.
fn drop_rank(role: ColumnRole) -> Option<u8> {
    match role {
        ColumnRole::Hash => Some(0),
        ColumnRole::Time => Some(1),
        ColumnRole::Count => Some(2),
        ColumnRole::Text => Some(3),
        ColumnRole::Kind => Some(4),
        ColumnRole::Path => Some(5),
        ColumnRole::Ref => Some(6),
        ColumnRole::Id | ColumnRole::Status | ColumnRole::Action => None,
    }
}

/// Each column's width, or `None` for one the tile has no room for. Columns
/// leave by rank, the later of two equals first, until the rest fit the width
/// (less the table's inset, the rows' padding and gaps, and the disclosure
/// column).
fn table_plan(shape: TableShape<'_>, action: bool) -> Vec<Option<ColumnWidth>> {
    let has_ref = shape
        .roles
        .iter()
        .any(|role| matches!(role, ColumnRole::Ref | ColumnRole::Path));
    let mut plan: Vec<Option<ColumnWidth>> = (0..shape.columns.len())
        .map(|index| Some(role_width(role_at(shape.roles, index), has_ref)))
        .collect();
    let disclosure = if action {
        TABLE_ACTION_WIDTH + TABLE_CELL_GAP
    } else {
        0.0
    };
    let room = shape.width - 2.0 * (TABLE_INSET + TABLE_ROW_PAD) - disclosure;
    let needed = |plan: &[Option<ColumnWidth>]| {
        let shown: Vec<f32> = plan.iter().flatten().map(|width| width.least()).collect();
        shown.iter().sum::<f32>() + shown.len().saturating_sub(1) as f32 * TABLE_CELL_GAP
    };
    while needed(&plan) > room {
        let rank = |index: usize| drop_rank(role_at(shape.roles, index));
        let leaving = (0..plan.len())
            .filter(|index| plan[*index].is_some())
            .filter_map(|index| rank(index).map(|rank| (rank, index)))
            .min_by_key(|(rank, index)| (*rank, std::cmp::Reverse(*index)));
        match leaving {
            Some((_, index)) => plan[index] = None,
            None => break,
        }
    }
    plan
}

/// Give a cell its column's width.
fn sized(cell: El, width: ColumnWidth) -> El {
    match width {
        ColumnWidth::Fixed(width) => cell.w(width),
        ColumnWidth::Grow { min, weight, max } => {
            let cell = cell.grow(weight).min_w(min);
            match max {
                Some(max) => cell.max_w(max),
                None => cell,
            }
        }
    }
}

/// Roles drawn in the monospace face.
fn role_is_mono(role: ColumnRole) -> bool {
    matches!(
        role,
        ColumnRole::Id | ColumnRole::Ref | ColumnRole::Path | ColumnRole::Hash
    )
}

/// The characters of an id that fit its column: longer ids keep their head
/// and tail around an ellipsis.
const ID_CHARS: usize = 13;

/// The characters of a hash that are shown.
const HASH_CHARS: usize = 8;

/// `text` cut to at most `max` characters by replacing the middle with an
/// ellipsis, so both ends of an id stay readable.
fn middle_ellipsis(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max || max < 3 {
        return text.to_owned();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut shortened: String = chars[..head].iter().collect();
    shortened.push('…');
    shortened.extend(chars[chars.len() - tail..].iter());
    shortened
}

/// What a cell shows for its role: a hash is its first eight characters and
/// an id is cut in the middle. The graphical toolkit truncates `ref` and
/// `path` cells at the end, since it offers no middle or left cut.
fn role_text(role: ColumnRole, cell: &str) -> String {
    match role {
        ColumnRole::Hash => cell.chars().take(HASH_CHARS).collect(),
        ColumnRole::Id => middle_ellipsis(cell, ID_CHARS),
        _ => cell.to_owned(),
    }
}

fn table_record(
    instance: &RyeOsViewInstanceKey,
    item: &RyeOsTableRowVm,
    roles: &[ColumnRole],
    plan: &[Option<ColumnWidth>],
    action: bool,
) -> El {
    let p = Palette::get();
    let t = Tokens::get();
    let mut line = row()
        .w_full()
        .min_w(0.0)
        .min_h(if item.hierarchy.is_some() {
            TABLE_ROW_HEIGHT_TREE
        } else {
            TABLE_ROW_HEIGHT
        })
        .px(TABLE_ROW_PAD)
        .gap(TABLE_CELL_GAP)
        .center_y()
        .role("row");
    if item.selected {
        line = line.fill(t.select_fill()).radius(theme::RADIUS_ROW);
    }
    // Only a status dot takes the tone; the row's text keeps its colour. A
    // table with a status column draws the dot in that cell. One with none
    // and no per-cell tones puts the row's tone before its first cell.
    let has_status = roles.contains(&ColumnRole::Status);
    let row_tone = if item.cell_tones.is_empty() && !has_status {
        t.tone(item.tone)
    } else {
        None
    };
    for (index, cell) in item.cells.iter().enumerate() {
        let Some(width) = plan.get(index).copied().flatten() else {
            continue;
        };
        let choose_key = instance.clone();
        let item_id = item.id.clone();
        let cell_tone = item.cell_tones.get(index).copied().flatten();
        let name = index == 0 && item.selected;
        let role = role_at(roles, index);
        let mut slot = sized(row(), width)
            .min_w(0.0)
            .h_full()
            .center_y()
            .role("cell");
        if index == 0
            && let Some(hierarchy) = &item.hierarchy
        {
            slot = slot.child(tree_prefix(instance, &item.id, cell, hierarchy));
        }
        let mut words = row().grow(1.0).min_w(0.0).gap(TONE_DOT_GAP).center_y();
        if role == ColumnRole::Count {
            words = words.justify_end();
        }
        // A status cell: the dot, or room for one, then the word. In a
        // status column the dot takes the cell's tone, else the row's.
        let dot_tone = if role == ColumnRole::Status {
            Some(cell_tone.unwrap_or(item.tone))
        } else {
            cell_tone
        };
        if let Some(tone) = dot_tone {
            words = words.child(match tone_dot(t.tone(tone), TONE_DOT) {
                Some(dot) => dot,
                None => el().size(TONE_DOT, TONE_DOT),
            });
        } else if index == 0
            && let Some(dot) = tone_dot(row_tone, TONE_DOT)
        {
            words = words.child(dot);
        }
        let ink = match role {
            ColumnRole::Path | ColumnRole::Time | ColumnRole::Hash => t.ink_dim(),
            ColumnRole::Kind => t.ink_soft(),
            _ if name => t.ink_strong(),
            _ => t.ink_body(),
        };
        let face = if role_is_mono(role) {
            theme::mono(theme::TYPE_SMALL)
        } else {
            theme::TYPE_SMALL
        };
        slot = slot.child(
            words
                .child(
                    text(role_text(role, cell))
                        .font(face)
                        .color(ink)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .py(6.0)
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
    if action {
        let mut disclosure = col().w(TABLE_ACTION_WIDTH).h_full().role("cell");
        if item.expandable {
            disclosure = disclosure.child(chevron(
                instance,
                &item.id,
                item.expanded,
                "row".to_owned(),
                TABLE_ACTION_WIDTH,
            ));
        }
        line = line.child(disclosure);
    }
    let mut record = col()
        .key(format!("item:{instance:?}:{}", item.id))
        .w_full()
        .min_w(0.0)
        .role("rowgroup")
        .child(line);
    if item.expanded {
        record = record.child(details(&item.detail));
    }
    record
}

/// The place before a hierarchical table row's first cell: an indent of
/// `TABLE_INDENT` per level, then the fold mark when the row has children. The
/// structure comes from the core's retained tree projection, never inferred.
fn tree_prefix(
    instance: &RyeOsViewInstanceKey,
    item_id: &str,
    label: &str,
    hierarchy: &RyeOsTableHierarchyVm,
) -> El {
    let t = Tokens::get();
    let p = Palette::get();
    let depth = hierarchy.ancestor_continues.len() as f32;
    let prefix = row()
        .center_y()
        .pl(depth * TABLE_INDENT)
        .w(depth * TABLE_INDENT + FOLD_MARK_WIDTH);
    if !hierarchy.has_children {
        return prefix;
    }
    let fold_key = instance.clone();
    let item_id = item_id.to_owned();
    let collapsed = hierarchy.collapsed;
    prefix.child(
        col()
            .center()
            .child(crate::icons::glyph(
                if collapsed {
                    &crate::icons::COLLAPSED
                } else {
                    &crate::icons::EXPANDED
                },
                12.0,
                t.ink_dim(),
            ))
            .min_w(FOLD_MARK_WIDTH)
            .padding(2.0)
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
    )
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

/// The timeline's chevron column.
const CHEVRON_WIDTH: f32 = 24.0;

/// An expand/collapse chevron `width` points wide: `ink.mute`, turned when the
/// record is open.
fn chevron(
    instance: &RyeOsViewInstanceKey,
    item_id: &str,
    expanded: bool,
    what: String,
    width: f32,
) -> El {
    let t = Tokens::get();
    let expand_key = instance.clone();
    let item_id = item_id.to_owned();
    col()
        .w(width)
        .h_full()
        .center()
        .hover(t.select_fill())
        .role("button")
        .label(format!(
            "{} {what}",
            if expanded { "Collapse" } else { "Expand" }
        ))
        .expanded(expanded)
        .child(crate::icons::glyph(
            if expanded {
                &crate::icons::EXPANDED
            } else {
                &crate::icons::COLLAPSED
            },
            12.0,
            t.ink_mute(),
        ))
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
    roles: &'a [String],
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
                entry_roles,
                entry_indents,
                selected,
                entry_expandable,
                entry_expanded,
                entry_details,
                ..
            } => Some(Timeline {
                entries,
                ids: entry_ids,
                roles: entry_roles,
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
/// Content sits in a column no wider than this, centred in the tile.
const TIMELINE_COLUMN: f32 = 720.0;
/// Tile padding either side of the column, and above and below it.
const TIMELINE_PAD_X: f32 = 24.0;
const TIMELINE_PAD_TOP: f32 = 14.0;
/// The feed's viewport never gets shorter than this.
const TIMELINE_FEED_FLOOR: f32 = 160.0;
/// The space above the status lane, outside the feed's viewport.
const TIMELINE_LANE_SPACE: f32 = 16.0;
/// The tallest the dock grows once it holds more than one card; the cards
/// scroll inside it.
const DOCK_STACK_MAX: f32 = 260.0;
/// How far a feed fades where its lines run out of view.
const TIMELINE_FADE: f32 = 16.0;
/// Between entries.
const TIMELINE_GAP: f32 = 16.0;
/// A selected line's fill reaches this far past the column on either side.
const SELECT_REACH: f32 = 10.0;

const TIMELINE_PAD: ListPad = ListPad {
    x: TIMELINE_PAD_X - SELECT_REACH,
    top: 0.0,
    bottom: 0.0,
    gap: TIMELINE_GAP,
    column: Some(TIMELINE_COLUMN + 2.0 * SELECT_REACH),
};

fn timeline_view(
    instance: &RyeOsViewInstanceKey,
    title: &str,
    status: Option<&RyeOsSourceStatusVm>,
    timeline: Timeline<'_>,
) -> El {
    let status = status.map(|status| row().w_full().center().child(source_status(status)));
    if timeline.entries.len() > VIRTUAL_ROWS_FROM {
        let keys = (0..timeline.entries.len())
            .map(|index| timeline_entry_key(instance, &timeline, index))
            .collect();
        return timeline_inset(virtual_list(
            format!("timeline:{instance:?}"),
            title,
            "list",
            status,
            instance,
            keys,
            TIMELINE_ENTRY_ESTIMATE,
            true,
            TIMELINE_PAD,
            |view, instance, index| {
                Timeline::of(view)
                    .filter(|timeline| index < timeline.entries.len())
                    .map(|timeline| timeline_entry(instance, &timeline, index))
            },
        ));
    }
    // The insets sit outside the scrolled column, so they stay put as the feed
    // scrolls and a clipped line is cut 14 below the header, not flush with it.
    let mut column = col()
        .w_full()
        .max_w(TIMELINE_COLUMN + 2.0 * SELECT_REACH)
        .gap(TIMELINE_GAP);
    if let Some(status) = status {
        column = column.child(status);
    }
    for index in 0..timeline.entries.len() {
        column = column.child(timeline_entry(instance, &timeline, index));
    }
    // A feed: it opens at the newest entry and follows new ones while there.
    timeline_inset(
        col()
            .key(format!("timeline:{instance:?}"))
            .w_full()
            .shrink()
            .min_h(0.0)
            .px(TIMELINE_PAD.x)
            .items_center()
            .label(title.to_owned())
            .scroll(ScrollSpec { stick_bottom: true })
            .scroll_edge_fade(TIMELINE_FADE)
            .child(column),
    )
}

/// The quiet band between a tile's header and the feed's viewport, and the
/// least height the feed keeps however tall the foot grows.
fn timeline_inset(feed: El) -> El {
    col()
        .w_full()
        .grow(1.0)
        .shrink()
        .min_h(TIMELINE_FEED_FLOOR)
        .pt(TIMELINE_PAD_TOP)
        .child(feed)
}

/// Height reserved for the status lane whether or not it has text, so the
/// transcript does not move when the lane appears or clears.
const LANE_HEIGHT: f32 = 24.0;
/// The lane's dot.
const LANE_DOT: f32 = 7.0;
/// A dock card: its padding, the gap inside it, and the gap between cards.
const DOCK_PAD: f32 = 16.0;
const DOCK_GAP: f32 = 10.0;
const DOCK_STACK_GAP: f32 = 8.0;
/// The dock card's border and fill, as opacities of `warn`.
const DOCK_BORDER: f32 = 0.50;
const DOCK_FILL: f32 = 0.05;
/// A dock button's height and horizontal padding.
const DOCK_BUTTON_HEIGHT: f32 = 32.0;
const DOCK_BUTTON_PAD: f32 = 14.0;

/// The status lane, then each pending ask docked below it; a pending ask is
/// drawn here only. Both sit in the timeline's column.
fn timeline_foot(
    instance: &RyeOsViewInstanceKey,
    lane: Option<&RyeOsTimelineLaneVm>,
    dock: &[RyeOsTimelineDockVm],
) -> El {
    let t = Tokens::get();
    let mut lane_row = row()
        .key(format!("lane:{instance:?}"))
        .w_full()
        .min_w(0.0)
        .min_h(LANE_HEIGHT)
        .gap(10.0)
        .center_y();
    if let Some(lane) = lane {
        lane_row = lane_row.role("status").label(lane.label.clone());
        if let Some(dot) = tone_dot(t.tone(lane.tone), LANE_DOT) {
            lane_row = lane_row.child(dot);
        }
        lane_row = lane_row.child(
            text(theme::drawable(&lane.label, theme::Face::Ui).into_owned())
                .font(theme::medium(theme::TYPE_LINE))
                .color(t.ink_body())
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        );
    }
    let mut foot = col()
        .w_full()
        .max_w(TIMELINE_COLUMN)
        .min_w(0.0)
        .gap(DOCK_STACK_GAP)
        .child(lane_row);
    let mut cards = col().w_full().min_w(0.0).gap(DOCK_STACK_GAP);
    for ask in dock {
        let (primary, meta) = match &ask.entry {
            RyeOsTimelineEntryVm::Line { primary, meta, .. } => (primary.clone(), meta.clone()),
            other => (other.label(), None),
        };
        let mut card = col()
            .key(format!("dock:{instance:?}:{}", ask.id))
            .w_full()
            .min_w(0.0)
            .padding(DOCK_PAD)
            .gap(DOCK_GAP)
            .radius(theme::RADIUS_CARD)
            .border(1.0, t.warn.alpha(DOCK_BORDER))
            .fill(t.warn.alpha(DOCK_FILL))
            .child(
                row()
                    .gap(10.0)
                    .center_y()
                    .child(
                        el().size(TONE_DOT, TONE_DOT)
                            .radius(TONE_DOT / 2.0)
                            .fill(t.warn),
                    )
                    .child(
                        text(theme::drawable(&primary, theme::Face::Ui).into_owned())
                            .font(theme::TYPE_TITLE)
                            .color(t.ink_strong())
                            .wrap()
                            .grow(1.0)
                            .min_w(0.0),
                    ),
            );
        if let Some(target) = meta {
            card = card.child(
                text(target)
                    .font(theme::mono(theme::TYPE_SMALL))
                    .color(t.ink_soft())
                    .wrap_anywhere(),
            );
        }
        // The first action the ask allows is the filled one; when the request
        // does not allow the first, the next takes the fill.
        let filled = ask
            .actions
            .iter()
            .position(|action| action.intent.is_some());
        let mut actions = row().gap(DOCK_STACK_GAP).center_y();
        for (index, action) in ask.actions.iter().enumerate() {
            actions = actions.child(dock_action(action, Some(index) == filled));
        }
        cards = cards.child(card.child(actions));
    }
    // More than one pending ask would crowd the feed out: the cards scroll
    // within a capped height instead.
    if !dock.is_empty() {
        if dock.len() > 1 {
            cards = cards.max_h(DOCK_STACK_MAX).shrink().scroll(ScrollSpec {
                stick_bottom: false,
            });
        }
        foot = foot.child(cards);
    }
    // The space above the lane belongs to the foot, not to the scrolled feed,
    // so it is there whatever entry is selected.
    col()
        .w_full()
        .min_w(0.0)
        .px(TIMELINE_PAD_X)
        .items_center()
        .child(el().h(TIMELINE_LANE_SPACE))
        .child(foot)
}

/// The gap between a dock button's word and its key.
const DOCK_KEY_GAP: f32 = 8.0;

/// A key's cap: a small bordered chip holding the key's name.
fn key_cap(key: &str) -> El {
    let t = Tokens::get();
    row()
        .h(20.0)
        .min_w(20.0)
        .px(6.0)
        .center()
        .radius(5.0)
        .border(1.0, t.ink_at(0.16))
        .child(
            text(key.to_owned())
                .font(theme::TYPE_KEY)
                .color(t.ink_at(0.60)),
        )
}

/// One answering action beside a docked ask. One the ask's authority does not
/// allow is drawn faint, with its reason as its label, and does nothing.
fn dock_action(action: &RyeOsTimelineDockActionVm, filled: bool) -> El {
    let p = Palette::get();
    let t = Tokens::get();
    let button = row()
        .h(DOCK_BUTTON_HEIGHT)
        .px(DOCK_BUTTON_PAD)
        .center()
        .radius(theme::RADIUS_BUTTON)
        .role("button");
    match &action.intent {
        Some(intent) => {
            let intent = intent.clone();
            let button = if filled {
                let mut button = button
                    .gap(DOCK_KEY_GAP)
                    .fill(t.ink)
                    .hover(t.ink_strong())
                    .child(
                        text(action.label.clone())
                            .font(theme::medium(theme::TYPE_CELL))
                            .color(t.bg),
                    );
                // The key sits in the button as quiet text on the fill.
                if let Some(key) = &action.key {
                    button = button.child(
                        text(key.clone())
                            .font(theme::medium(theme::TYPE_NOTE))
                            .color(t.bg.alpha(0.5)),
                    );
                }
                button
            } else {
                let mut button = button
                    .gap(DOCK_KEY_GAP)
                    .border(1.0, t.control_border_strong())
                    .hover(p.plane_selected)
                    .child(
                        text(action.label.clone())
                            .font(theme::TYPE_CELL)
                            .color(t.ink_body()),
                    );
                if let Some(key) = &action.key {
                    button = button.child(key_cap(key));
                }
                button
            };
            button
                .label(action.label.clone())
                .on_press(move |app: &mut App| {
                    app.ui(RyeOsUiEvent::Activate {
                        intent: intent.clone(),
                    });
                })
        }
        None => button
            .border(1.0, t.control_border())
            .child(
                text(action.label.clone())
                    .font(theme::TYPE_CELL)
                    .color(t.ink_faint()),
            )
            .label(
                action
                    .disabled_reason
                    .clone()
                    .unwrap_or_else(|| action.label.clone()),
            ),
    }
}

/// Estimated height of a timeline entry not yet measured.
const TIMELINE_ENTRY_ESTIMATE: f32 = 44.0;

/// A timeline entry's key: its id, or its place when it has none.
fn timeline_entry_key(
    instance: &RyeOsViewInstanceKey,
    timeline: &Timeline<'_>,
    index: usize,
) -> String {
    format!(
        "entry:{instance:?}:{}",
        timeline
            .ids
            .get(index)
            .map_or_else(|| index.to_string(), Clone::clone)
    )
}

/// One timeline entry with its chevron and, when expanded, its details.
fn timeline_entry(instance: &RyeOsViewInstanceKey, timeline: &Timeline<'_>, index: usize) -> El {
    let p = Palette::get();
    let t = Tokens::get();
    let entry = &timeline.entries[index];
    // The renderer never trusts the parallel arrays to be complete.
    let id = timeline.ids.get(index);
    let role = timeline.roles.get(index).map_or("", String::as_str);
    let indent = f32::from(timeline.indents.get(index).copied().unwrap_or(0));
    let selected = timeline.selected == Some(index);
    let expandable = timeline.expandable.get(index).copied().unwrap_or(false);
    let expanded = timeline.expanded.get(index).copied().unwrap_or(false);
    let content = match entry {
        // Prose is plain text, never folded.
        RyeOsTimelineEntryVm::Block { text: body, .. } => {
            let body = theme::drawable(undecorated(body), theme::Face::Ui).into_owned();
            text(body)
                .selectable(true)
                .font(theme::TYPE_PROSE)
                .color(t.ink_body())
                .wrap()
                .max_w(TIMELINE_COLUMN)
                .grow(1.0)
                .min_w(0.0)
        }
        RyeOsTimelineEntryVm::Line {
            primary,
            meta,
            tone,
            state,
            ..
        } => match role {
            // A boundary in the log: a rule with a word in it.
            "marker" | "status" | "summary" => marker_rule(primary, meta.as_deref()),
            "cluster" => one_line(None, primary, t.ink_soft(), None, None),
            // A recap's step count stands where a state word would.
            "recap" => one_line(None, primary, t.ink_strong(), None, meta.as_deref()),
            _ => one_line(
                t.tone(*tone),
                primary,
                t.ink_soft(),
                meta.as_deref(),
                state.as_deref(),
            ),
        },
        RyeOsTimelineEntryVm::Pair {
            summary,
            meta,
            tone,
            state,
            ..
        } => one_line(
            t.tone(*tone),
            summary,
            t.ink_soft(),
            meta.as_deref(),
            state.as_deref(),
        ),
        RyeOsTimelineEntryVm::Separator { label } if role == "message" => message_bubble(label),
        RyeOsTimelineEntryVm::Separator { label } => marker_rule(label, None),
    };
    let mut button = row()
        .grow(1.0)
        .min_w(0.0)
        .center_y()
        .role("button")
        .label(match timeline.roles.get(index) {
            Some(role) => format!("{role}, {}", entry.label()),
            None => entry.label(),
        })
        .focus_selected(selected)
        .child(content);
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
    let mut head = row()
        .w_full()
        .min_w(0.0)
        .px(SELECT_REACH)
        .radius(theme::RADIUS_ROW)
        .child(button);
    if selected {
        head = head.fill(t.select_fill());
    }
    if expandable && let Some(id) = id {
        head = head.child(chevron(
            instance,
            id,
            expanded,
            "timeline entry".to_owned(),
            CHEVRON_WIDTH,
        ));
    }
    // The indent narrows the record from the left, so a nested entry's chevron
    // still stands at the column's right edge.
    let mut record = col()
        .key(timeline_entry_key(instance, timeline, index))
        .w_full()
        .min_w(0.0)
        .pl(indent * TIMELINE_INDENT)
        .child(head);
    if expanded && let Some(rows) = timeline.details.get(index) {
        record = record.child(details(rows));
    }
    record
}

/// Height of a one-line entry, and of a marker.
const LINE_ENTRY_HEIGHT: f32 = 26.0;
const MARKER_HEIGHT: f32 = 22.0;
/// Between the elements of a one-line entry.
const LINE_ENTRY_GAP: f32 = 9.0;
/// Between a marker's rule and its word.
const MARKER_GAP: f32 = 12.0;
/// A message bubble: its widest, where its text wraps, and its padding.
const BUBBLE_WIDTH: f32 = 502.0;
const BUBBLE_WRAP: f32 = 470.0;
const BUBBLE_PAD_X: f32 = 16.0;
const BUBBLE_PAD_Y: f32 = 12.0;
/// The bubble's fill, as an opacity of `ink`.
const BUBBLE_FILL: f32 = 0.09;

/// Marks the core puts before a headline that the look does not draw.
const LEADING_MARKS: [char; 2] = ['↘', '✻'];

fn undecorated(text: &str) -> &str {
    text.trim_start_matches(&LEADING_MARKS[..]).trim_start()
}

/// The least width a one-line entry's detail keeps: a long headline wraps
/// before the detail is squeezed to a bare ellipsis against the state word.
const DETAIL_FLOOR: f32 = 72.0;

/// A one-line entry: a tone dot or nothing, the headline, detail taking the
/// remaining width and truncating, a state word. The headline never truncates.
fn one_line(
    tone: Option<rune::Color>,
    headline: &str,
    headline_ink: rune::Color,
    detail: Option<&str>,
    state: Option<&str>,
) -> El {
    let t = Tokens::get();
    let mut line = row()
        .grow(1.0)
        .min_w(0.0)
        .min_h(LINE_ENTRY_HEIGHT)
        .gap(LINE_ENTRY_GAP)
        .center_y();
    if let Some(dot) = tone_dot(tone, TONE_DOT) {
        line = line.child(dot);
    }
    line = line.child(
        text(theme::drawable(undecorated(headline), theme::Face::Ui).into_owned())
            .font(theme::TYPE_LINE)
            .color(headline_ink)
            .wrap()
            .min_w(0.0),
    );
    // The detail takes whatever width is left; without one a spacer does.
    line = match detail {
        Some(detail) => line.child(
            text(detail.to_owned())
                .font(theme::TYPE_LINE)
                .color(t.ink_dim())
                .truncate()
                .grow(1.0)
                .min_w(DETAIL_FLOOR),
        ),
        None => line.child(el().grow(1.0)),
    };
    if let Some(state) = state {
        line = line.child(
            text(state.to_owned())
                .font(theme::TYPE_SMALL)
                .color(t.ink_dim()),
        );
    }
    line
}

/// A boundary in the log: a rule either side of a word.
fn marker_rule(word: &str, detail: Option<&str>) -> El {
    let t = Tokens::get();
    let rule = || el().grow(1.0).h(1.0).fill(t.rule());
    let say = |what: &str| {
        text(theme::drawable(what, theme::Face::Ui).into_owned())
            .font(theme::TYPE_MICRO)
            .color(t.ink_dim())
    };
    let mut line = row()
        .grow(1.0)
        .min_w(0.0)
        .min_h(MARKER_HEIGHT)
        .gap(MARKER_GAP)
        .center_y()
        .child(rule())
        .child(say(word));
    if let Some(detail) = detail {
        line = line.child(say(detail));
    }
    line.child(rule())
}

/// The operator's message: a bubble against the right edge.
fn message_bubble(label: &str) -> El {
    let t = Tokens::get();
    row().grow(1.0).min_w(0.0).justify_end().child(
        el().max_w(BUBBLE_WIDTH)
            .min_w(0.0)
            .px(BUBBLE_PAD_X)
            .py(BUBBLE_PAD_Y)
            .radius(theme::RADIUS_BUBBLE)
            .fill(t.ink_at(BUBBLE_FILL))
            .child(
                text(theme::drawable(label, theme::Face::Ui).into_owned())
                    .selectable(true)
                    .font(theme::TYPE_PROSE)
                    .color(t.ink_strong())
                    .wrap()
                    .max_w(BUBBLE_WRAP),
            ),
    )
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
    let t = Tokens::get();
    let pairs: Vec<(&str, String, Option<rune::Color>)> = details
        .iter()
        .map(|detail| {
            let field = if detail.field.is_empty() {
                "detail"
            } else {
                detail.field.as_str()
            };
            let tone = detail.tone.and_then(|tone| t.tone(tone));
            (field, detail.value.clone(), tone)
        })
        .collect();
    pair_block(&pairs, style)
}

/// Untoned field/value pairs.
pub(crate) fn detail_pairs(pairs: &[(&str, String)], style: Pairs) -> El {
    let pairs: Vec<(&str, String, Option<rune::Color>)> = pairs
        .iter()
        .map(|(field, value)| (*field, value.clone(), None))
        .collect();
    pair_block(&pairs, style)
}

/// Field names and values as a section draws them: the name dim, the value at
/// the cell step, a toned value behind its dot.
fn pair_block(pairs: &[(&str, String, Option<rune::Color>)], style: Pairs) -> El {
    let t = Tokens::get();
    let (label_width, gap) = match style {
        Pairs::Record => (110.0, FIELD_ROW_GAP),
        Pairs::Panel => (76.0, 8.0),
    };
    let mut block = col().w_full();
    block = match style {
        Pairs::Record => block.pt(7.0).pb(9.0).pl(34.0).pr(12.0),
        Pairs::Panel => block.py(5.0),
    };
    for (field, value, tone) in pairs {
        let mut words = row().grow(1.0).min_w(0.0).gap(TONE_DOT_GAP).center_y();
        if let Some(dot) = tone_dot(*tone, TONE_DOT) {
            words = words.child(dot);
        }
        block = block.child(
            row()
                .w_full()
                .min_w(0.0)
                .gap(gap)
                .py(2.0)
                .child(
                    text((*field).to_owned())
                        .font(theme::TYPE_SMALL)
                        .color(t.ink_dim())
                        .wrap_anywhere()
                        .w(label_width),
                )
                .child(
                    words.child(
                        text(value.clone())
                            .font(theme::TYPE_CELL)
                            .color(t.ink_body())
                            .wrap_anywhere()
                            .grow(1.0)
                            .min_w(0.0),
                    ),
                ),
        );
    }
    block
}

/// The admitted source's state when it is not simply live. A source that
/// could not be read says so in a sentence under a `warn` dot, with its reason
/// beneath, and is never drawn as empty.
fn source_status(status: &RyeOsSourceStatusVm) -> El {
    let t = Tokens::get();
    let stale = if status.stale {
        "Last observation (stale) · "
    } else {
        ""
    };
    if is_unreadable(&status.state) {
        let said = match status.state.as_str() {
            "unresolved" => "Waiting for something to read from.",
            "unavailable" => "Unavailable.",
            _ => "Could not be read.",
        };
        return unavailable(said, &format!("{stale}{}", status.message));
    }
    row().w_full().px(STATE_PAD_X).py(6.0).child(
        text(format!("{stale}{}", status.message))
            .font(theme::TYPE_SMALL)
            .color(t.ink_dim())
            .wrap_anywhere()
            .grow(1.0)
            .min_w(0.0)
            .role("status"),
    )
}

/// The left inset of a state's lines, and the room above them.
const STATE_PAD_X: f32 = 10.0;
const STATE_PAD_TOP: f32 = 10.0;

/// A source that could not be read: a `warn` dot and a sentence, with the
/// reason on a line beneath it, left-aligned under the header.
fn unavailable(said: &str, reason: &str) -> El {
    let t = Tokens::get();
    let mut lines = col()
        .w_full()
        .min_w(0.0)
        .px(STATE_PAD_X)
        .pt(STATE_PAD_TOP)
        .gap(2.0)
        .role("status")
        .label(said.to_owned())
        .child(
            row()
                .gap(10.0)
                .min_h(26.0)
                .center_y()
                .child(
                    el().size(UNAVAILABLE_DOT, UNAVAILABLE_DOT)
                        .radius(UNAVAILABLE_DOT / 2.0)
                        .fill(t.warn),
                )
                .child(
                    text(said.to_owned())
                        .font(theme::TYPE_CELL)
                        .color(t.ink_body())
                        .wrap()
                        .grow(1.0)
                        .min_w(0.0),
                ),
        );
    if !reason.is_empty() {
        lines = lines.child(
            text(reason.to_owned())
                .font(theme::TYPE_MICRO)
                .color(t.ink_dim())
                .wrap_anywhere()
                .min_w(0.0)
                .pl(UNAVAILABLE_DOT + 10.0),
        );
    }
    lines
}

/// The unavailable state's dot.
const UNAVAILABLE_DOT: f32 = 7.0;

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

/// A view with nothing to draw yet: still loading, unreadable, or empty. The
/// message says which, in the words the core gave it.
fn empty_state(title: &str, message: &str) -> El {
    let t = Tokens::get();
    if message.starts_with("loading") {
        return loading_skeleton();
    }
    if let Some(reason) = ["failed: ", "unavailable: ", "unresolved: "]
        .into_iter()
        .find_map(|prefix| message.strip_prefix(prefix))
    {
        let said = if title.is_empty() {
            "Could not be read.".to_owned()
        } else {
            format!("{title} could not be read.")
        };
        return unavailable(&said, reason);
    }
    col().w_full().px(STATE_PAD_X).pt(STATE_PAD_TOP).child(
        text(message.to_owned())
            .font(theme::TYPE_SMALL)
            .color(t.ink_dim())
            .wrap()
            .min_w(0.0),
    )
}

/// The widths of a loading page's bars, in a row's columns.
const SKELETON_BARS: [f32; 6] = [90.0, 62.0, 120.0, 64.0, 83.0, 56.0];
const SKELETON_ROWS: usize = 8;
const SKELETON_ROW_HEIGHT: f32 = 28.0;
const SKELETON_BAR_HEIGHT: f32 = 8.0;
const SKELETON_FILL: f32 = 0.07;

/// A page that is still being read: still bars where the rows will be, and a
/// word under them. Nothing moves, so nothing seems to be happening that is
/// not.
fn loading_skeleton() -> El {
    let t = Tokens::get();
    let mut rows = col().w_full().gap(1.0);
    for index in 0..SKELETON_ROWS {
        let mut line = row()
            .key(format!("skeleton:{index}"))
            .w_full()
            .min_w(0.0)
            .h(SKELETON_ROW_HEIGHT)
            .gap(12.0)
            .center_y();
        for width in SKELETON_BARS {
            // The bars give way, so a narrow tile never clips the last one.
            line = line.child(
                el().size(width, SKELETON_BAR_HEIGHT)
                    .min_w(0.0)
                    .shrink()
                    .radius(SKELETON_BAR_HEIGHT / 2.0)
                    .fill(t.ink_at(SKELETON_FILL)),
            );
        }
        rows = rows.child(line);
    }
    col()
        .w_full()
        .min_w(0.0)
        .px(STATE_PAD_X)
        .pt(STATE_PAD_TOP)
        .gap(10.0)
        .role("status")
        .label("Reading")
        .child(rows)
        .child(
            text("Reading…")
                .font(theme::TYPE_MICRO)
                .color(t.ink_at(0.40)),
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
        assert!(is_launcher_chord(&press(
            k(),
            with(true, false, false, false)
        )));
        assert!(is_launcher_chord(&press(
            k(),
            with(false, true, false, false)
        )));
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
        let presentation = PresentationId::try_new("11111111-1111-4111-8111-111111111111").unwrap();
        RyeOsViewInstanceKey::shared_dialog(&presentation, "left").unwrap()
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
        // Depth is an indent and a fold mark, never drawn guide glyphs.
        let shown = texts(&rt);
        for guide in ["│├", "└", "─", "├"] {
            assert!(!shown.iter().any(|text| text == guide), "{guide:?} drawn");
        }
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
        assert!(
            !select.starts_with(&format!("{toggle}/"))
                && !toggle.starts_with(&format!("{select}/"))
        );
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
        assert!(
            blocked.opacity < fine.opacity,
            "{} vs {}",
            blocked.opacity,
            fine.opacity
        );
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
        assert!(
            !says(&rt, "Hidden primary"),
            "a detail record has no title line"
        );
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
        assert!(says(&empty, "Nothing observed yet."));
        let populated = drawn(table(
            vec![table_row("t1", ["Alpha", "beta"], json!({}))],
            1,
            Value::Null,
        ));
        assert!(!says(&populated, "Nothing observed yet."));
        // The page window can be empty while the collection is not.
        let windowed = drawn(table(Vec::new(), 5, Value::Null));
        assert!(!says(&windowed, "Nothing observed yet."));
    }

    /// A view's body with the foot a frame adds under it.
    fn drawn_with_foot(view: RyeOsViewVm) -> rune::Runtime<App> {
        let instance = instance();
        let local = std::rc::Rc::new(crate::Local::default());
        runtime((900.0, 700.0), move || {
            let mut tile = rune::col().child(body(&view, &instance, &local));
            if let Some(foot) = super::table_foot(&view) {
                tile = tile.child(foot);
            }
            tile
        })
    }

    #[test]
    fn the_coverage_line_is_shown_whether_or_not_there_are_rows() {
        let drawn = drawn_with_foot;
        let coverage = json!({
            "state": "partial", "reason": "page_limit",
            "label": "Part of what exists is shown.", "tone": "warn"
        });
        let empty = drawn(table(Vec::new(), 0, coverage.clone()));
        assert!(says(&empty, "Part of what exists is shown."));
        assert!(says(&empty, "Nothing observed yet."));
        let populated = drawn(table(
            vec![table_row("t1", ["Alpha", "beta"], json!({}))],
            1,
            coverage,
        ));
        // The sentence is drawn as written, never upper-cased and never as
        // the codes.
        assert!(says(&populated, "Part of what exists is shown."));
        assert!(
            !texts(&populated)
                .iter()
                .any(|text| text.contains("PAGE_LIMIT"))
        );
        assert!(
            !texts(&populated)
                .iter()
                .any(|text| text.contains("page_limit"))
        );
        let unreported = drawn(table(
            vec![table_row("t1", ["Alpha", "beta"], json!({}))],
            1,
            Value::Null,
        ));
        assert!(!texts(&unreported).iter().any(|text| text.contains(" / ")));
    }

    fn roled_table(roles: Value) -> RyeOsViewVm {
        serde_json::from_value(json!({
            "type": "table", "title": "Threads", "columns": ["name", "kind"],
            "column_roles": roles, "total_rows": 1,
            "rows": [table_row("t1", ["T-0123456789abcdef", "0123456789abcdef"], json!({}))]
        }))
        .unwrap()
    }

    #[test]
    fn an_id_is_cut_in_the_middle_and_a_hash_keeps_eight_characters() {
        let rt = drawn(roled_table(json!(["id", "hash"])));
        assert!(says(&rt, "T-0123…abcdef"));
        assert!(says(&rt, "01234567"));
        // With no roles the cells are drawn whole.
        let plain = drawn(roled_table(json!([])));
        assert!(says(&plain, "T-0123456789abcdef"));
        assert!(says(&plain, "0123456789abcdef"));
    }

    #[test]
    fn each_role_asks_for_its_width_and_the_flexible_roles_grow() {
        use super::ColumnWidth::{Fixed, Grow};
        use ryeos_client_base::ui::content::ColumnRole as R;
        for (role, width) in [
            (R::Id, 112.0),
            (R::Kind, 78.0),
            (R::Status, 104.0),
            (R::Time, 100.0),
            (R::Hash, 84.0),
            (R::Count, 56.0),
            (R::Action, 24.0),
        ] {
            assert_eq!(super::role_width(role, true), Fixed(width), "{role:?}");
        }
        for role in [R::Ref, R::Path] {
            assert!(matches!(super::role_width(role, true), Grow { .. }));
        }
        // Beside a ref column text is bounded; alone it takes its share.
        assert!(matches!(
            super::role_width(R::Text, true),
            Grow { max: Some(_), .. }
        ));
        assert!(matches!(
            super::role_width(R::Text, false),
            Grow { max: None, .. }
        ));
        for role in [R::Id, R::Ref, R::Path, R::Hash] {
            assert!(super::role_is_mono(role), "{role:?}");
        }
        for role in [R::Text, R::Kind, R::Status, R::Time, R::Count, R::Action] {
            assert!(!super::role_is_mono(role), "{role:?}");
        }
    }

    /// The columns of the threads table, by role.
    fn thread_roles() -> (Vec<String>, Vec<ColumnRole>) {
        use ryeos_client_base::ui::content::ColumnRole as R;
        (
            ["thread", "kind", "item", "project", "status", "created"]
                .map(String::from)
                .to_vec(),
            vec![R::Id, R::Kind, R::Ref, R::Text, R::Status, R::Time],
        )
    }

    fn shown(width: f32) -> Vec<bool> {
        let (columns, roles) = thread_roles();
        let shape = super::TableShape {
            columns: &columns,
            roles: &roles,
            width,
        };
        super::table_plan(shape, false)
            .iter()
            .map(Option::is_some)
            .collect()
    }

    #[test]
    fn a_narrow_tile_drops_columns_by_rank_and_keeps_id_and_status() {
        // Room for every column.
        assert_eq!(shown(940.0), vec![true; 6]);
        // The tile of a three-column page: time, project and kind go.
        assert_eq!(
            shown(450.0),
            vec![true, false, true, false, true, false],
            "id, item and status stay"
        );
        // Narrower still: the item goes before the id or the status do.
        assert_eq!(shown(190.0), vec![true, false, false, false, true, false]);
        // A window with no width yet keeps every column.
        assert_eq!(shown(f32::MAX), vec![true; 6]);
    }

    #[test]
    fn a_dropped_column_has_no_header_and_no_cell() {
        let columns = ["thread", "kind", "status"];
        let view: RyeOsViewVm = serde_json::from_value(json!({
            "type": "table", "title": "Threads", "columns": columns,
            "column_roles": ["id", "kind", "status"], "total_rows": 1,
            "rows": [{
                "id": "t1", "cells": ["T-1", "directive", "running"], "tone": "good",
                "selected": false, "expandable": false, "expanded": false, "detail": []
            }]
        }))
        .unwrap();
        let instance = instance();
        let local = std::rc::Rc::new(crate::Local::default());
        let rt = runtime((900.0, 700.0), move || {
            super::body_in(&view, &instance, &local, 200.0)
        });
        assert!(says(&rt, "T-1"));
        assert!(says(&rt, "running"));
        assert!(!says(&rt, "directive"));
        assert!(!says(&rt, "KIND"));
        assert!(says(&rt, "STATUS"));
    }

    #[test]
    fn rows_that_open_on_activation_draw_no_disclosure_column() {
        let expandable = |intent: Value| {
            let view: RyeOsViewVm = serde_json::from_value(json!({
                "type": "table", "title": "Threads", "columns": ["name", "kind"],
                "total_rows": 1,
                "rows": [table_row(
                    "t1", ["Alpha", "beta"],
                    json!({"expandable": true, "intent": intent})
                )]
            }))
            .unwrap();
            drawn(view)
        };
        let opening = expandable(json!({"type": "toggle_top_status_bar"}));
        assert!(!says(&opening, "DETAIL"));
        assert!(labelled_containing(&opening, "Expand row").is_empty());
        let plain = expandable(Value::Null);
        assert!(!says(&plain, "DETAIL"));
        assert_eq!(
            labelled_containing(&plain, "Expand row"),
            vec!["Expand row".to_owned()]
        );
    }

    #[test]
    fn middle_ellipsis_keeps_both_ends_and_leaves_short_text_alone() {
        assert_eq!(super::middle_ellipsis("short", 13), "short");
        let cut = super::middle_ellipsis("T-0123456789abcdef", 13);
        assert_eq!(cut.chars().count(), 13);
        assert!(cut.starts_with("T-0123"));
        assert!(cut.ends_with("abcdef"));
    }

    // ---- The look ----

    fn timeline(entries: Vec<Value>, roles: &[&str]) -> RyeOsViewVm {
        let ids: Vec<String> = (0..entries.len()).map(|i| format!("e{i}")).collect();
        serde_json::from_value(json!({
            "type": "timeline", "title": "Activity", "entries": entries,
            "entry_roles": roles, "entry_ids": ids
        }))
        .unwrap()
    }

    #[test]
    fn timeline_entries_draw_by_their_role() {
        let rt = drawn(timeline(
            vec![
                json!({"type": "separator", "label": "Keep the tiling behaviour."}),
                json!({"type": "block", "text": "I traced the selection.", "tone": "neutral"}),
                json!({"type": "pair", "summary": "Fetched", "meta": "docs.rs",
                       "tone": "good", "pending": false, "state": "done"}),
                json!({"type": "line", "primary": "↘ review", "meta": "T-e84f",
                       "tone": "good", "state": "completed"}),
                json!({"type": "line", "primary": "continued in a new thread",
                       "tone": "neutral"}),
            ],
            &["message", "prose", "activity", "link", "marker"],
        ));
        for shown in [
            "Keep the tiling behaviour.",
            "I traced the selection.",
            "Fetched",
            "docs.rs",
            "done",
            "review",
            "T-e84f",
            "completed",
            "continued in a new thread",
        ] {
            assert!(says(&rt, shown), "{shown:?} in {:?}", texts(&rt));
        }
        assert!(
            !texts(&rt).iter().any(|text| text.contains('↘')),
            "the core's leading mark is not drawn"
        );
    }

    #[test]
    fn a_docked_ask_is_a_card_with_its_target_and_actions() {
        use ryeos_client_base::ui::RyeOsUiIntent;
        use ryeos_client_base::ui::timeline::RyeOsTimelineEntryVm;
        use ryeos_client_base::ui::view_model::{
            RyeOsTimelineDockActionVm, RyeOsTimelineDockVm, RyeOsTimelineLaneVm, RyeOsTone,
        };

        let instance = instance();
        let dock = vec![RyeOsTimelineDockVm {
            id: "ask:1".to_owned(),
            entry: RyeOsTimelineEntryVm::Line {
                primary: "Write a file outside the workspace".to_owned(),
                meta: Some("~/.config/ryeos/presentation.yaml".to_owned()),
                tone: RyeOsTone::Accent,
                intent: None,
                secondary_intent: None,
                state: None,
            },
            actions: vec![
                RyeOsTimelineDockActionVm {
                    label: "Approve".to_owned(),
                    key: Some("A".to_owned()),
                    intent: None,
                    disabled_reason: Some("not allowed here".to_owned()),
                },
                RyeOsTimelineDockActionVm {
                    label: "Decline".to_owned(),
                    key: Some("D".to_owned()),
                    intent: Some(RyeOsUiIntent::Refresh),
                    disabled_reason: None,
                },
            ],
        }];
        let lane = RyeOsTimelineLaneVm {
            label: "Waiting for approval".to_owned(),
            tone: RyeOsTone::Warn,
        };
        let rt = runtime((900.0, 700.0), move || {
            super::timeline_foot(&instance, Some(&lane), &dock)
        });
        for shown in [
            "Waiting for approval",
            "Write a file outside the workspace",
            "~/.config/ryeos/presentation.yaml",
            "Approve",
            "Decline",
            "D",
        ] {
            assert!(says(&rt, shown), "{shown:?} in {:?}", texts(&rt));
        }
        assert!(
            labelled_containing(&rt, "not allowed here").len() == 1,
            "the action the ask does not allow carries its reason"
        );
    }

    #[test]
    fn a_section_row_is_a_name_and_a_value() {
        let view: RyeOsViewVm = serde_json::from_value(json!({
            "type": "sections", "title": "Thread",
            "sections": [{
                "id": "summary", "title": "Summary", "count": 1, "rows": [
                    list_row("r1", "status", None, json!({"meta": "running", "tone": "accent"}))
                ]
            }]
        }))
        .unwrap();
        let rt = drawn(view);
        for shown in ["SUMMARY", "status", "running"] {
            assert!(says(&rt, shown), "{shown:?} in {:?}", texts(&rt));
        }
    }

    #[test]
    fn a_source_that_could_not_be_read_is_never_an_empty_page() {
        let view: RyeOsViewVm = serde_json::from_value(json!({
            "type": "table", "title": "Threads", "columns": ["name"],
            "total_rows": 0, "rows": [],
            "source_status": {"state": "failed", "message": "denied", "stale": false}
        }))
        .unwrap();
        let rt = drawn(view);
        assert!(!says(&rt, "Nothing observed yet."));
        assert!(says(&rt, "Could not be read."));
        assert!(says(&rt, "denied"));
        assert!(!says(&rt, "failed: denied"));
    }

    #[test]
    fn a_page_still_loading_draws_bars_and_an_unreadable_one_a_sentence() {
        let placeholder = |message: &str| {
            drawn(
                serde_json::from_value(
                    json!({"type": "placeholder", "title": "Surfaces", "message": message}),
                )
                .unwrap(),
            )
        };
        let loading = placeholder("loading service:scheduler/list …");
        assert!(says(&loading, "Reading…"), "{:?}", texts(&loading));
        assert!(!says(&loading, "loading service:scheduler/list …"));
        let failed = placeholder("failed: service_cap_denied");
        assert!(says(&failed, "Surfaces could not be read."));
        assert!(says(&failed, "service_cap_denied"));
        let empty = placeholder("No activity yet");
        assert!(says(&empty, "No activity yet"));
    }

    #[test]
    fn a_status_cell_keeps_its_word() {
        let rt = drawn(table(
            vec![table_row(
                "t1",
                ["T-ab", "running"],
                json!({"cell_tones": [null, "accent"]}),
            )],
            1,
            Value::Null,
        ));
        assert!(says(&rt, "T-ab"));
        assert!(says(&rt, "running"));
    }
}
