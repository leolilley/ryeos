//! The shell driven as an operator drives it, over the in-memory fixture:
//! presses, typing and keys go through Rune's own input routing, and the
//! assertions read the core's view model and the element tree.

#[path = "../fixtures/presentation.rs"]
mod presentation;

use std::rc::Rc;

use rune::{Key, KeyEvent, Mods, Runtime};
use ryeos_client_base::ui::view_model::{RyeOsInputVm, RyeOsViewModel, RyeOsViewVm};
use ryeos_client_rune::{App, fixture_runtime};
use serde_json::Value;

const FRAME: f64 = 1.0 / 60.0;

fn shell(scene: &str, size: (f32, f32)) -> Runtime<App> {
    let mut rt = fixture_runtime(presentation::core(scene), presentation::dress(scene), size);
    rt.frame(0.0);
    // Layout reports the viewport on the first frame and the shell redraws
    // at its next wake; a second of frames settles both, as a snapshot does.
    for _ in 0..60 {
        rt.frame(FRAME);
    }
    rt
}

fn model(rt: &Runtime<App>) -> Rc<RyeOsViewModel> {
    rt.state.view_model().expect("the fixture has a core")
}

/// Every input the presentation shows, wherever it is mounted.
fn inputs(vm: &RyeOsViewModel) -> Vec<RyeOsInputVm> {
    fn walk(value: &Value, found: &mut Vec<RyeOsInputVm>) {
        match value {
            Value::Object(map) => {
                if map.contains_key("address")
                    && map.contains_key("placeholder")
                    && let Ok(input) = serde_json::from_value(value.clone())
                {
                    found.push(input);
                }
                map.values().for_each(|child| walk(child, found));
            }
            Value::Array(items) => items.iter().for_each(|child| walk(child, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(&serde_json::to_value(vm).unwrap(), &mut found);
    found
}

fn keyed(rt: &Runtime<App>, prefix: &str) -> Option<[f32; 4]> {
    rt.elements()
        .into_iter()
        .find(|element| {
            element
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with(prefix))
        })
        .map(|element| element.rect)
}

fn press(rt: &mut Runtime<App>, prefix: &str) {
    let [x, y, w, h] = keyed(rt, prefix).unwrap_or_else(|| panic!("no element keyed {prefix}*"));
    rt.pointer_down(x + w / 2.0, y + h / 2.0);
    rt.pointer_up(x + w / 2.0, y + h / 2.0);
    rt.frame(FRAME);
}

/// Empty the find line the launcher scenes open with.
fn clear_query(rt: &mut Runtime<App>) {
    for _ in 0..8 {
        bare_key(rt, Key::Backspace);
    }
}

fn type_text(rt: &mut Runtime<App>, text: &str) {
    for ch in text.chars() {
        rt.key(KeyEvent {
            key: Key::Text(ch.to_string()),
            mods: Mods::default(),
        });
        rt.frame(FRAME);
    }
}

/// Every checked-in scene file loads into a core, and its dress (if it names
/// one) is known.
#[test]
fn every_scene_file_loads() {
    for scene in presentation::SCENES {
        presentation::core(scene).unwrap_or_else(|error| panic!("scene {scene}: {error}"));
        let _ = presentation::dress(scene);
    }
}

/// The scene files on disk are exactly the scenes the fixture lists.
#[test]
fn scene_list_matches_files() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/scenes");
    let mut on_disk: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".json").unwrap().to_owned()
        })
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = presentation::SCENES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    listed.sort();
    assert_eq!(on_disk, listed);
}

#[test]
fn an_opened_launcher_takes_typing_without_a_click() {
    let mut rt = shell("launcher", (1280.0, 800.0));
    let before = model(&rt);
    assert_eq!(before.dialogs.len(), 1, "the launcher is open");
    type_text(&mut rt, "new");
    let after = model(&rt);
    let query = after.dialogs[0]
        .input
        .as_ref()
        .expect("the launcher's query");
    // The scene's own query is already in the find line.
    assert_eq!(query.text, "treenew");
    assert_ne!(
        serde_json::to_value(&before.dialogs[0].view).unwrap(),
        serde_json::to_value(&after.dialogs[0].view).unwrap(),
        "the query filters the launcher"
    );
}

#[test]
fn the_launcher_chord_from_the_composer_keeps_the_draft() {
    let mut rt = shell("", (1280.0, 800.0));
    assert!(model(&rt).dialogs.is_empty());
    press(&mut rt, "composer:");
    type_text(&mut rt, "hi");
    rt.key(KeyEvent {
        key: Key::Text("k".to_owned()),
        mods: Mods {
            alt: true,
            ..Mods::default()
        },
    });
    rt.frame(FRAME);
    let vm = model(&rt);
    assert_eq!(vm.dialogs.len(), 1, "Alt+K opened the launcher");
    assert!(
        inputs(&vm).iter().any(|input| input.text == "hi"),
        "the composer kept its draft, and no \"k\" was typed into it"
    );
}

#[test]
fn a_narrow_page_shows_only_the_focused_tile() {
    let tiles = |rt: &Runtime<App>| {
        rt.elements()
            .into_iter()
            .filter(|element| {
                element
                    .key
                    .as_deref()
                    .is_some_and(|key| key.starts_with("tile:"))
            })
            .count()
    };
    assert_eq!(tiles(&shell("", (700.0, 800.0))), 1);
    assert_eq!(tiles(&shell("", (1280.0, 800.0))), 2);
}

/// Every field the presentation shows, wherever it is mounted.
fn fields(vm: &RyeOsViewModel) -> Vec<Value> {
    fn walk(value: &Value, found: &mut Vec<Value>) {
        match value {
            Value::Object(map) => {
                if let Some(field) = map.get("field")
                    && field.get("entities").is_some()
                {
                    found.push(field.clone());
                }
                map.values().for_each(|child| walk(child, found));
            }
            Value::Array(items) => items.iter().for_each(|child| walk(child, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(&serde_json::to_value(vm).unwrap(), &mut found);
    found
}

#[test]
fn the_field_scene_draws_its_execution_entities() {
    let rt = shell("field", (1440.0, 900.0));
    let vm = model(&rt);
    let field = fields(&vm).pop().expect("the execution field is mounted");
    let stage = keyed(&rt, "field-stage:").expect("the stage is drawn");
    let entities: Vec<[f32; 4]> = rt
        .elements()
        .into_iter()
        .filter(|element| {
            element
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with("field-entity:"))
        })
        .map(|element| element.rect)
        .collect();
    assert_eq!(
        entities.len(),
        field["entities"].as_array().map_or(0, Vec::len),
        "every entity is drawn"
    );
    let first = entities.iter().map(|rect| rect[0]).fold(f32::MAX, f32::min);
    assert!(
        first < stage[0] + stage[2] / 2.0,
        "the authored first group opens the stage, not an empty one ({first} in {stage:?})"
    );
}

/// The centre of the first element whose key starts with `prefix` and ends
/// with `suffix`.
fn keyed_between(rt: &Runtime<App>, prefix: &str, suffix: &str) -> Option<[f32; 4]> {
    rt.elements()
        .into_iter()
        .find(|element| {
            !element.exiting
                && element
                    .key
                    .as_deref()
                    .is_some_and(|key| key.starts_with(prefix) && key.ends_with(suffix))
        })
        .map(|element| element.rect)
}

fn press_rect(rt: &mut Runtime<App>, [x, y, w, h]: [f32; 4]) {
    rt.pointer_down(x + w / 2.0, y + h / 2.0);
    rt.pointer_up(x + w / 2.0, y + h / 2.0);
    rt.frame(FRAME);
}

fn bare_key(rt: &mut Runtime<App>, key: Key) {
    rt.key(KeyEvent {
        key,
        mods: Mods::default(),
    });
    rt.frame(FRAME);
}

/// The launcher's rows as (id, selected), in order.
fn launcher_rows(vm: &RyeOsViewModel) -> Vec<(String, bool)> {
    match &vm.dialogs[0].view {
        RyeOsViewVm::Rows { rows, .. } => rows
            .iter()
            .map(|row| (row.id.clone(), row.selected))
            .collect(),
        other => panic!("the launcher is not a rows view: {other:?}"),
    }
}

fn selected_position(rows: &[(String, bool)]) -> usize {
    let selected: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, (_, selected))| *selected)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(selected.len(), 1, "exactly one launcher row is selected");
    selected[0]
}

#[test]
fn a_launcher_dialog_opens_moves_chooses_and_closes() {
    let mut rt = shell("launcher", (1280.0, 800.0));
    // Opened: one dialog with several rows.
    let rows = launcher_rows(&model(&rt));
    assert!(rows.len() >= 3, "the launcher lists several choices");
    // Moving down moves the single selection towards the end of the list.
    bare_key(&mut rt, Key::Down);
    let first = selected_position(&launcher_rows(&model(&rt)));
    bare_key(&mut rt, Key::Down);
    let second = selected_position(&launcher_rows(&model(&rt)));
    assert!(
        second > first,
        "Down moved the selection ({first} then {second})"
    );
    bare_key(&mut rt, Key::Up);
    let back = selected_position(&launcher_rows(&model(&rt)));
    assert!(back < second, "Up moved it back ({second} then {back})");
    // Choosing runs the selected item. What it does is the item's business,
    // so the dialog may close itself; whichever it does, it stays one dialog
    // at most and the close control then finishes the sequence.
    bare_key(&mut rt, Key::Enter);
    let after_choice = model(&rt).dialogs.len();
    assert!(after_choice <= 1);
    if after_choice == 1 {
        let close = keyed_between(&rt, "dialog:", ":close").expect("the dialog's close control");
        press_rect(&mut rt, close);
    }
    assert!(model(&rt).dialogs.is_empty(), "the launcher is closed");
}

/// The launcher sheet's own element: the dialog panel, not its close control.
fn launcher_sheet(rt: &Runtime<App>) -> Option<[f32; 4]> {
    rt.elements()
        .into_iter()
        .find(|element| {
            element
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with("dialog:") && !key.ends_with(":close"))
        })
        .map(|element| element.rect)
}

fn launcher_places(vm: &RyeOsViewModel) -> Vec<String> {
    match &vm.dialogs[0].view {
        RyeOsViewVm::Rows { rows, .. } => rows
            .iter()
            .filter(|row| row.launch.as_ref().is_some_and(|launch| launch.place))
            .map(|row| row.primary.clone())
            .collect(),
        other => panic!("the launcher is not a rows view: {other:?}"),
    }
}

#[test]
fn the_launcher_sheet_has_a_find_line_group_labels_and_a_footer_but_no_title_bar() {
    let rt = shell("launcher", (1280.0, 800.0));
    let texts: Vec<String> = drawn_texts(&rt).into_iter().map(|(text, _)| text).collect();
    assert!(
        !texts.iter().any(|text| text == "LAUNCH"),
        "the sheet draws no title bar, saw {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("Opening a view runs nothing")),
        "the footer carries the view's own note, saw {texts:?}"
    );
    assert!(texts.iter().any(|text| text == "cancel"));
    let vm = model(&rt);
    let RyeOsViewVm::Rows { rows, .. } = &vm.dialogs[0].view else {
        panic!("the launcher is a rows view");
    };
    let group = rows
        .iter()
        .find_map(|row| row.launch.as_ref().map(|launch| launch.group.clone()))
        .expect("launcher rows carry their group");
    assert!(
        texts.contains(&group.to_uppercase()),
        "the group {group:?} is a label, saw {texts:?}"
    );
    let [_, _, width, _] = launcher_sheet(&rt).expect("the launcher sheet");
    assert!(
        (width - 860.0).abs() < 1.0,
        "the sheet is 860 wide, not {width}"
    );
    assert_eq!(
        vm.dialogs[0].title, "Launch",
        "the dialog keeps the title the smoke test finds"
    );
}

#[test]
fn enter_chooses_a_view_then_a_place_and_escape_closes_the_second_stage_first() {
    let mut rt = shell("launcher", (1280.0, 800.0));
    clear_query(&mut rt);
    assert!(launcher_places(&model(&rt)).is_empty());
    // Down to the first view that can be chosen: commands and views that
    // need a selection do not stage anything.
    let chosen = |rt: &Runtime<App>| match &model(rt).dialogs[0].view {
        RyeOsViewVm::Rows { rows, .. } => rows.iter().any(|row| {
            row.selected
                && row.intent.is_some()
                && row
                    .launch
                    .as_ref()
                    .is_some_and(|launch| launch.group != "Commands")
        }),
        _ => false,
    };
    for _ in 0..60 {
        if chosen(&rt) {
            break;
        }
        bare_key(&mut rt, Key::Down);
    }
    assert!(chosen(&rt), "the launcher lists a view that can be chosen");
    bare_key(&mut rt, Key::Enter);
    let vm = model(&rt);
    assert_eq!(vm.dialogs.len(), 1, "choosing a view keeps the sheet");
    let places = launcher_places(&vm);
    assert_eq!(
        places.first().map(String::as_str),
        Some("In this tile, as a tab"),
        "the first place is the one Enter takes"
    );
    assert!(places.iter().any(|place| place == "New tab"));
    assert!(
        drawn_texts(&rt)
            .iter()
            .any(|(text, _)| text == "WHERE IT LANDS"),
        "the places are drawn under their label"
    );
    bare_key(&mut rt, Key::Escape);
    let vm = model(&rt);
    assert_eq!(vm.dialogs.len(), 1, "Escape closes the second stage first");
    assert!(launcher_places(&vm).is_empty());
    bare_key(&mut rt, Key::Escape);
    assert!(model(&rt).dialogs.is_empty(), "then the sheet");
}

#[test]
fn the_launcher_stage_scene_draws_where_the_chosen_view_lands() {
    let rt = shell("launcher-stage", (1280.0, 800.0));
    let vm = model(&rt);
    assert_eq!(vm.dialogs.len(), 1, "the launcher stays open");
    let places = launcher_places(&vm);
    assert_eq!(
        places.first().map(String::as_str),
        Some("In this tile, as a tab")
    );
    let drawn = drawn_texts(&rt);
    for word in ["WHERE IT LANDS", "In this tile, as a tab", "New tab"] {
        assert!(
            drawn.iter().any(|(text, _)| text == word),
            "the second stage draws {word:?}"
        );
    }
}

#[test]
fn the_launcher_lists_no_selection_command_beside_its_views() {
    let mut rt = shell("launcher", (1280.0, 800.0));
    clear_query(&mut rt);
    let RyeOsViewVm::Rows { rows, .. } = &model(&rt).dialogs[0].view else {
        panic!("the launcher is a rows view");
    };
    for row in rows {
        for command in ["Tree", "Inspect", "Compare as left", "Compare as right"] {
            assert_ne!(row.primary, command, "{command} is not listed unsearched");
        }
    }
}

#[test]
fn a_table_tile_draws_its_note_and_no_detail_column() {
    let rt = shell("", (1280.0, 800.0));
    let drawn = drawn_texts(&rt);
    assert!(
        drawn
            .iter()
            .any(|(text, _)| text == "200 at most · sorted by watch"),
        "the table's tile says how the page is bounded"
    );
    assert!(
        !drawn.iter().any(|(text, _)| text == "DETAIL"),
        "rows that open on activation have no disclosure column"
    );
}

#[test]
fn the_launcher_opens_from_the_switcher_and_closes_from_its_control() {
    let mut rt = shell("", (1280.0, 800.0));
    assert!(model(&rt).dialogs.is_empty());
    press(&mut rt, "switcher:add");
    assert_eq!(
        model(&rt).dialogs.len(),
        1,
        "the switcher's plus opened the launcher"
    );
    assert!(
        keyed_between(&rt, "dialog:", ":close").is_some(),
        "the open dialog draws its close control"
    );
    let close = keyed_between(&rt, "dialog:", ":close").unwrap();
    press_rect(&mut rt, close);
    assert!(model(&rt).dialogs.is_empty());
    assert!(keyed_between(&rt, "dialog:", "").is_none());
}

/// Text elements whose centre lies inside the horizontal band `top..bottom`.
fn texts_in_band(rt: &Runtime<App>, top: f32, bottom: f32) -> Vec<String> {
    rt.elements()
        .into_iter()
        .filter_map(|element| {
            let [_, y, _, h] = element.rect;
            let middle = y + h / 2.0;
            (middle >= top && middle <= bottom)
                .then_some(element.text)
                .flatten()
        })
        .collect()
}

/// Every drawn text with its rectangle.
fn drawn_texts(rt: &Runtime<App>) -> Vec<(String, [f32; 4])> {
    rt.elements()
        .into_iter()
        .filter_map(|element| element.text.map(|text| (text, element.rect)))
        .collect()
}

/// The chrome never shows the project's filesystem path. The chrome is the
/// Surfaces switcher down the left edge (x under 50) and the tabs band (the top
/// 50 points); it must draw something, and none of it carries the path.
#[test]
fn the_chrome_never_shows_the_project_path() {
    for scene in ["", "field", "launcher"] {
        let rt = shell(scene, (1280.0, 800.0));
        let vm = model(&rt);
        let path = vm
            .session
            .project_path
            .clone()
            .expect("the fixture session has a project path to hide");
        let chrome: Vec<String> = drawn_texts(&rt)
            .into_iter()
            .filter(|(_, [x, y, _, h])| *x < 50.0 || y + h / 2.0 <= 50.0)
            .map(|(text, _)| text)
            .collect();
        assert!(!chrome.is_empty(), "{scene}: the chrome says something");
        for text in &chrome {
            assert!(
                !text.contains(&path) && !text.contains("/home/"),
                "{scene}: the chrome shows {text:?}"
            );
        }
    }
}

/// The status bar is two floating islands: the key hints, and the core's
/// segments with the node's health. It shows exactly the segments (as the
/// core words them, not upper-cased), and
/// carries no product name.
#[test]
fn the_status_bar_shows_only_its_segments() {
    let (width, _) = (1280.0_f32, 800.0_f32);
    let rt = shell("", (width, 800.0));
    let vm = model(&rt);
    let status = &vm.presentation.chrome.status_bar;
    let Some([_, top, _, height]) = keyed(&rt, "shell:keys") else {
        assert!(
            !status.visible || status.key_hints.is_empty(),
            "a visible status bar with hints draws its keys island"
        );
        return;
    };
    assert!(status.visible);
    let [launch_x, launch_y, launch_w, launch_h] =
        keyed(&rt, "shell:keys").expect("the keys island");
    let launch_right = launch_x + launch_w;
    // The islands share one row; read the whole band the launcher sits in.
    let shown = texts_in_band(&rt, top, top + height);
    // The dialog is not open in this fixture, so no text "Launch" belongs in
    // the status band.
    assert!(
        !shown.iter().any(|text| text == "Launch"),
        "the status bar carries no launcher wording, saw {shown:?}"
    );
    // The left island is every text on the keys island's row left of the
    // window's middle: it holds nothing but the key hints. The hint is checked by its
    // ASCII core, since the drawn form may substitute glyphs the faces lack.
    let left: Vec<(String, [f32; 4])> = drawn_texts(&rt)
        .into_iter()
        .filter(|(_, [x, y, _, h])| {
            let middle = y + h / 2.0;
            *x >= 0.0 && *x < width / 2.0 && middle >= launch_y && middle <= launch_y + launch_h
        })
        .collect();
    // Each hint draws as its key chip and its word, nothing else.
    let left: Vec<String> = left.into_iter().map(|(text, _)| text).collect();
    assert!(
        status
            .key_hints
            .iter()
            .any(|hint| hint.keys == "ctrl+k" && hint.word == "open"),
        "the fixture's hints keep their ASCII core"
    );
    assert!(
        left.windows(2)
            .any(|pair| pair[0] == "ctrl+k" && pair[1] == "open"),
        "the open hint as its key then its word, saw {left:?}"
    );
    assert!(
        left.len() <= 2 * status.key_hints.len() && left.len().is_multiple_of(2),
        "only the key hints in the left island, saw {left:?}"
    );
    // A text is in the right island when it is right of the keys island and
    // its vertical centre is within the island's top..bottom band.
    let in_right_island = |wanted: &str| {
        drawn_texts(&rt).into_iter().any(|(text, [x, y, _, h])| {
            let middle = y + h / 2.0;
            text == wanted
                && x >= launch_right
                && middle >= launch_y
                && middle <= launch_y + launch_h
        })
    };
    for segment in &status.segments {
        if let Some(label) = segment.label.as_deref().filter(|label| !label.is_empty()) {
            assert!(
                in_right_island(label),
                "segment label {label:?} in the right island, saw {shown:?}"
            );
        }
        assert!(
            in_right_island(&segment.value),
            "segment value {:?} in the right island, saw {shown:?}",
            segment.value
        );
    }
    assert!(
        drawn_texts(&rt)
            .iter()
            .any(|(text, [x, _, _, _])| *text == vm.chrome.health_label && *x > width / 2.0),
        "the node health {:?} sits right of the window's middle",
        vm.chrome.health_label
    );
    assert!(
        in_right_island(&vm.chrome.health_label),
        "the node health {:?} in the right island, saw {shown:?}",
        vm.chrome.health_label
    );
    assert!(
        !shown.iter().any(|text| text.eq_ignore_ascii_case("RYEOS")),
        "the status bar carries no product name, found {shown:?}"
    );
    assert!(
        !shown.iter().any(|text| text.contains("/home/")),
        "no path in the status bar"
    );
}

#[test]
fn the_fixture_renders_at_every_viewport_width() {
    // Narrow shows only the focused tile; compact and wide show both. There
    // is no navigation rail at any width: views are ordinary tiles.
    for scene in ["", "field"] {
        for (width, tiles) in [(700.0_f32, 1_usize), (900.0, 2), (1440.0, 2)] {
            let rt = shell(scene, (width, 800.0));
            let found: Vec<[f32; 4]> = rt
                .elements()
                .into_iter()
                .filter(|element| {
                    element
                        .key
                        .as_deref()
                        .is_some_and(|key| key.starts_with("tile:"))
                })
                .map(|element| element.rect)
                .collect();
            assert_eq!(found.len(), tiles, "scene {scene:?} at {width}");
            for [x, _, w, _] in found {
                assert!(
                    x + w <= width + 0.5,
                    "scene {scene:?} at {width}: a tile ends at {}",
                    x + w
                );
            }
            assert!(
                model(&rt).view_set.is_some(),
                "scene {scene:?} at {width}: the view set is mounted"
            );
        }
    }
}

/// The longest array named `name` anywhere in the view model.
fn longest(vm: &RyeOsViewModel, name: &str) -> usize {
    fn walk(value: &Value, name: &str, best: &mut usize) {
        match value {
            Value::Object(map) => {
                if let Some(Value::Array(items)) = map.get(name) {
                    *best = (*best).max(items.len());
                }
                map.values().for_each(|child| walk(child, name, best));
            }
            Value::Array(items) => items.iter().for_each(|child| walk(child, name, best)),
            _ => {}
        }
    }
    let mut best = 0;
    walk(&serde_json::to_value(vm).unwrap(), name, &mut best);
    best
}

fn keyed_count(rt: &Runtime<App>, prefix: &str) -> usize {
    rt.elements()
        .into_iter()
        .filter(|element| {
            element
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with(prefix))
        })
        .count()
}

#[test]
fn long_lists_build_only_the_rows_in_view_and_the_feed_opens_at_its_end() {
    let rt = shell("long", (1280.0, 800.0));
    let vm = model(&rt);
    let rows = longest(&vm, "rows");
    let entries = longest(&vm, "entries");
    assert!(rows > 64, "the thread list holds {rows} rows");
    assert!(entries > 64, "the timeline holds {entries} entries");
    let built_rows = keyed_count(&rt, "item:");
    let built_entries = keyed_count(&rt, "entry:");
    assert!(
        built_rows > 0 && built_rows < rows,
        "{built_rows} of {rows} rows built"
    );
    assert!(
        built_entries > 0 && built_entries < entries,
        "{built_entries} of {entries} entries built"
    );
    let texts: Vec<String> = rt
        .elements()
        .into_iter()
        .filter_map(|element| element.text)
        .collect();
    // The long fixture's last step.
    let last = "Step 200 of the run.".to_owned();
    assert!(texts.contains(&last), "the feed opens at its newest entry");
    assert!(!texts.iter().any(|text| text == "Step 1 of the run."));
}

/// The open Surfaces are a switcher of small chips pinned in from the top-left
/// corner; the status islands float in from the bottom-left corner, with the
/// node's health in the bottom band.
#[test]
fn the_switcher_holds_the_left_edge_and_the_status_islands_float() {
    let (width, height) = (1280.0, 800.0);
    let rt = shell("", (width, height));
    let close = |value: f32, wanted: f32, what: &str| {
        assert!(
            (value - wanted).abs() <= 2.0,
            "{what}: {value}, wanted about {wanted}"
        );
    };
    // The status row has a 10 inset at the sides and the bottom, so the keys
    // island sits at x = 10. The row is 40 high with 10 beneath; the island is
    // 30 high: bottom = height - 10.
    let [x, y, _, tall] = keyed(&rt, "shell:keys").expect("the keys island is drawn");
    close(x, 10.0, "the keys island's x");
    close(y + tall, height - 10.0, "the keys island's bottom");
    close(tall, 30.0, "the keys island's height");
    let vm = model(&rt);
    let current = vm
        .session
        .surfaces
        .iter()
        .find(|surface| surface.active)
        .expect("the fixture has a current Surface");
    // The chip has no key of its own; its entry holds it. The switcher is 10
    // in from the corner, then 6 (x) and 3 (y) of padding.
    let [sx, sy, sw, _] = keyed(&rt, &format!("surface:{:?}", current.surface_instance_id))
        .expect("the current Surface is a chip in the switcher");
    close(sx, 10.0 + 6.0, "the Surface chip's x");
    close(sy, 10.0 + 3.0, "the Surface chip's y");
    assert!(sw <= 40.0, "the Surface chip is narrow, {sw} wide");
    // The first tile is the first thing the body draws (keys `tile:`); the
    // frame reserves 44 and the body 18, so it starts at 62 or later.
    let mut tiles: Vec<[f32; 4]> = rt
        .elements()
        .into_iter()
        .filter(|element| {
            element
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with("tile:"))
        })
        .map(|element| element.rect)
        .collect();
    assert!(!tiles.is_empty(), "the body draws a tile");
    tiles.sort_by(|a, b| a[0].total_cmp(&b[0]));
    assert!(
        tiles[0][0] >= 62.0 - 0.01,
        "the first tile starts clear of the switcher, at {}",
        tiles[0][0]
    );
    let body_bottom = tiles
        .iter()
        .map(|[_, ty, _, th]| ty + th)
        .fold(0.0_f32, f32::max);
    // Views draw their own `status` roles (a timeline's lane, a field's
    // readout), so the node's health is found by its own key.
    let [_, status_top, _, _] =
        keyed(&rt, "shell:health").expect("the node's health is in the status bar");
    assert!(
        status_top >= body_bottom - 0.5,
        "the node's status at {} sits below the body, which ends at {body_bottom}",
        status_top
    );
}

/// With the status bar hidden (Alt+B), nothing draws the keys island and
/// the node's health remains as a 6-point dot at the bottom-right, keyed
/// `shell:health`.
#[test]
fn a_hidden_status_bar_keeps_only_the_health_dot() {
    let mut rt = shell("", (1280.0, 800.0));
    rt.key(KeyEvent {
        key: Key::Text("b".to_owned()),
        mods: Mods {
            alt: true,
            ..Mods::default()
        },
    });
    for _ in 0..30 {
        rt.frame(FRAME);
    }
    assert!(
        !model(&rt).presentation.chrome.status_bar.visible,
        "Alt+B hid the status bar"
    );
    assert!(keyed(&rt, "shell:keys").is_none(), "no keys island");
    let [x, y, w, h] = keyed(&rt, "shell:health").expect("the health dot remains");
    assert!(
        (w - 6.0).abs() < 0.5 && (h - 6.0).abs() < 0.5,
        "a 6-point dot, not {w}x{h}"
    );
    // pinned 14 from the right and bottom of a 1280 x 800 window
    assert!(
        (x + w - (1280.0 - 14.0)).abs() < 1.0,
        "dot right edge at {}",
        x + w
    );
    assert!(
        (y + h - (800.0 - 14.0)).abs() < 1.0,
        "dot bottom edge at {}",
        y + h
    );
}

fn exactly_keyed(rt: &Runtime<App>, key: &str) -> Option<[f32; 4]> {
    rt.elements()
        .into_iter()
        .find(|element| element.key.as_deref() == Some(key))
        .map(|element| element.rect)
}

#[test]
fn the_current_view_set_tab_stays_in_view_when_the_tabs_scroll() {
    let (width, height) = (900.0, 800.0);
    let rt = shell("tabs", (width, height));
    let vm = model(&rt);
    let tabs = &vm.presentation.chrome.top_bar.tabs;
    assert!(
        vm.presentation.chrome.top_bar.visible,
        "the tab band is shown"
    );
    assert_eq!(tabs.len(), 15, "the scene declares fifteen view sets");
    let current = tabs
        .iter()
        .find(|tab| tab.active)
        .expect("a view set is current");
    assert_eq!(
        current.view_set_id,
        tabs.last().unwrap().view_set_id,
        "the newest view set is current"
    );
    let rects: Vec<[f32; 4]> = tabs
        .iter()
        .filter_map(|tab| exactly_keyed(&rt, &format!("view-set:{}", tab.view_set_id)))
        .collect();
    let left = rects.iter().map(|r| r[0]).fold(f32::INFINITY, f32::min);
    let right = rects
        .iter()
        .map(|r| r[0] + r[2])
        .fold(f32::NEG_INFINITY, f32::max);
    let drawn: Vec<String> = rt
        .elements()
        .into_iter()
        .filter_map(|element| element.key)
        .filter(|key| key.contains("view-set"))
        .collect();
    assert!(
        right - left > width,
        "the tabs overflow the band ({left}..{right} in {width}); band visible: {}; tabs: {}; drawn: {drawn:?}",
        vm.presentation.chrome.top_bar.visible,
        tabs.len()
    );
    let [x, _, w, _] = exactly_keyed(&rt, &format!("view-set:{}", current.view_set_id))
        .expect("the current tab is drawn");
    assert!(
        x >= 0.0 && x + w <= width,
        "the current tab is in view: {x}..{} in {width}",
        x + w
    );
}

#[test]
fn surface_chips_draw_their_marks_and_dots() {
    let rt = shell("chips", (1280.0, 800.0));
    let vm = model(&rt);
    let surfaces = &vm.session.surfaces;
    assert_eq!(surfaces.len(), 4, "the scene opens four Surfaces");
    let key = |surface: &ryeos_client_base::ui::view_model::RyeOsSurfaceChoiceVm| {
        format!("surface:{:?}", surface.surface_instance_id)
    };
    // Home: work needing you and running; needing you wins.
    let home = &surfaces[0];
    assert!(exactly_keyed(&rt, &format!("needs-you:{}", key(home))).is_some());
    assert!(exactly_keyed(&rt, &format!("running:{}", key(home))).is_none());
    // The node: running only.
    let node = &surfaces[1];
    assert!(exactly_keyed(&rt, &format!("running:{}", key(node))).is_some());
    // The project: zero counts draw no dot, and no chip carries a project mark.
    let project = &surfaces[2];
    for word in ["needs-you", "running", "project"] {
        assert!(exactly_keyed(&rt, &format!("{word}:{}", key(project))).is_none());
    }
    // Unknown counts draw nothing.
    let plain = &surfaces[3];
    for word in ["needs-you", "running", "project"] {
        assert!(exactly_keyed(&rt, &format!("{word}:{}", key(plain))).is_none());
    }
    // A dot sits inside its chip's top-right corner.
    let [cx, cy, cw, _] = exactly_keyed(&rt, &key(home)).expect("Home's chip");
    let [dx, dy, dw, _] =
        exactly_keyed(&rt, &format!("needs-you:{}", key(home))).expect("Home's dot");
    assert!(dx + dw <= cx + cw && dx > cx + cw / 2.0 && dy >= cy && dy < cy + 8.0);
}

#[test]
fn a_tab_with_work_carries_its_dot_after_its_word() {
    let rt = shell("tabs", (900.0, 800.0));
    let vm = model(&rt);
    let tabs = &vm.presentation.chrome.top_bar.tabs;
    let needing = tabs.last().expect("the scene has tabs");
    let running = &tabs[4];
    let drawn = |word: &str, id| exactly_keyed(&rt, &format!("{word}:view-set:{id}"));
    // Only tabs in view are drawn; the scene scrolls to the last tab, so
    // check what is drawn against what the model says.
    for tab in tabs {
        let in_view = exactly_keyed(&rt, &format!("view-set:{}", tab.view_set_id)).is_some();
        let expects_needing = tab.view_set_id == needing.view_set_id;
        let expects_running = tab.view_set_id == running.view_set_id;
        if in_view {
            assert_eq!(
                drawn("needs-you", tab.view_set_id).is_some(),
                expects_needing
            );
            assert_eq!(drawn("running", tab.view_set_id).is_some(), expects_running);
        }
    }
}

/// The tiles' left and right extents.
fn tile_extent(rt: &Runtime<App>) -> (f32, f32) {
    let tiles: Vec<[f32; 4]> = rt
        .elements()
        .into_iter()
        .filter(|element| {
            element
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with("tile:"))
        })
        .map(|element| element.rect)
        .collect();
    assert!(!tiles.is_empty(), "the body draws a tile");
    (
        tiles.iter().map(|r| r[0]).fold(f32::INFINITY, f32::min),
        tiles.iter().map(|r| r[0] + r[2]).fold(0.0, f32::max),
    )
}

#[test]
fn a_switcher_on_the_right_and_a_status_bar_on_top_move_their_room() {
    let (width, height) = (1280.0, 800.0);
    let rt = shell("switcher-right", (width, height));
    let vm = model(&rt);
    let current = vm
        .session
        .surfaces
        .iter()
        .find(|surface| surface.active)
        .expect("a current Surface");
    let [sx, sy, sw, _] = keyed(&rt, &format!("surface:{:?}", current.surface_instance_id))
        .expect("the switcher draws");
    assert!(
        sx + sw <= width - 10.0 + 0.5 && sx >= width - 60.0,
        "switcher at the right: {sx}"
    );
    let [_, ly, _, lh] = keyed(&rt, "shell:keys").expect("the status bar draws");
    assert!(ly + lh <= 45.0, "the status bar is at the top: {ly}");
    assert!(
        sy >= 40.0,
        "the switcher clears the status bar on top: {sy}"
    );
    let (left, right) = tile_extent(&rt);
    assert!(left < 40.0, "no room is kept on the left: {left}");
    assert!(
        right <= width - 44.0 + 0.5,
        "the tiles clear the switcher: {right}"
    );
    assert!(
        exactly_keyed(&rt, "shell:health").is_some_and(|[_, y, _, _]| y < 50.0),
        "the node's health is in the top bar"
    );
}

#[test]
fn a_switcher_along_the_top_is_a_band_of_named_pills() {
    let (width, height) = (1280.0, 800.0);
    let rt = shell("switcher-top", (width, height));
    let vm = model(&rt);
    assert_eq!(vm.session.surfaces.len(), 4);
    for surface in &vm.session.surfaces {
        let [_, y, _, h] =
            exactly_keyed(&rt, &format!("surface:{:?}", surface.surface_instance_id))
                .expect("each Surface is a pill");
        assert!(
            y >= 10.0 - 0.5 && y + h <= 50.0 + 0.5,
            "the pill is in the top band: {y}"
        );
    }
    let texts: Vec<String> = drawn_texts(&rt).into_iter().map(|(text, _)| text).collect();
    assert!(texts.iter().any(|text| text == "Home"), "{texts:?}");
    assert!(
        texts.iter().any(|text| text == "ryeos"),
        "the project's tag carries no mark: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text == "ryeos·"),
        "no project mark on the tag: {texts:?}"
    );
    let home = &vm.session.surfaces[0];
    assert!(
        exactly_keyed(
            &rt,
            &format!("needs-you:surface:{:?}", home.surface_instance_id)
        )
        .is_some()
    );
    let (left, _) = tile_extent(&rt);
    assert!(left < 40.0, "no side room with the switcher on top: {left}");
}

#[test]
fn a_bare_surface_draws_no_switcher_and_no_status_bar_but_keeps_the_health_dot() {
    let (width, height) = (1280.0, 800.0);
    let rt = shell("bare", (width, height));
    assert!(keyed(&rt, "surface:").is_none(), "no switcher");
    assert!(keyed(&rt, "shell:keys").is_none(), "no status bar");
    let [x, y, w, h] = exactly_keyed(&rt, "shell:health").expect("the health dot");
    assert!(w <= 6.5 && h <= 6.5 && x > width - 30.0 && y > height - 30.0);
    let (left, _) = tile_extent(&rt);
    assert!(left < 40.0, "no switcher room: {left}");
    assert!(
        model(&rt).presentation.chrome.top_bar.visible,
        "the scene shows its tabs"
    );
    assert!(
        exactly_keyed(&rt, "shell:new-view-set").is_none(),
        "a Surface with add_tab false has no plus"
    );
}

#[test]
fn the_overlay_draws_its_sections_rows_and_the_selected_rows_actions() {
    let (width, height) = (1280.0, 800.0);
    let rt = shell("overlay", (width, height));
    let vm = model(&rt);
    assert!(vm.overlay.open);
    let [sx, sy, sw, _] = exactly_keyed(&rt, "overlay").expect("the overlay sheet");
    assert!((sy - 150.0).abs() < 1.0, "the sheet sits 150 down: {sy}");
    assert!(
        sw <= 920.5 && (sx + sw / 2.0 - width / 2.0).abs() < 1.0,
        "centred, at most 920"
    );
    for surface in &vm.session.surfaces {
        assert!(
            exactly_keyed(
                &rt,
                &format!("overlay:surface:{:?}", surface.surface_instance_id)
            )
            .is_some(),
            "a card per open Surface"
        );
    }
    // Needs-you rows take the first indices; running continues after them.
    assert!(exactly_keyed(&rt, "overlay:needs-you:0").is_some());
    assert!(exactly_keyed(&rt, "overlay:needs-you:1").is_some());
    assert!(exactly_keyed(&rt, "overlay:needs-you:2").is_some());
    assert!(exactly_keyed(&rt, "overlay:running:3").is_some());
    let texts: Vec<String> = drawn_texts(&rt).into_iter().map(|(text, _)| text).collect();
    for wanted in [
        "Approve the deploy",
        "this node",
        "acknowledge",
        "12m",
        "2h",
    ] {
        assert!(
            texts.iter().any(|text| text == wanted),
            "{wanted:?} in {texts:?}"
        );
    }
    // Only the selected row shows its actions; the others show their age.
    assert!(
        !texts.iter().any(|text| text == "4m"),
        "the selected row shows actions, not age"
    );
    for hint in &vm.overlay.key_hints {
        assert!(texts.contains(&hint.word), "footer hint {:?}", hint.word);
    }
}

#[test]
fn the_tab_band_carries_no_layout_readout_and_sets_its_words_level_with_the_chips() {
    let rt = shell("chips", (1280.0, 800.0));
    let vm = model(&rt);
    let top_bar = &vm.presentation.chrome.top_bar;
    assert!(top_bar.visible, "the chips scene shows the tab band");
    let texts = drawn_texts(&rt);
    assert!(
        !texts
            .iter()
            .any(|(text, _)| text.contains('|') || text.contains('│')),
        "no layout readout in the band: {texts:?}"
    );
    let current = top_bar
        .tabs
        .iter()
        .find(|tab| tab.active)
        .expect("a current tab");
    let (_, [_, y, _, h]) = texts
        .iter()
        .find(|(text, _)| *text == current.title)
        .expect("the current tab's word is drawn");
    assert!(
        (y + h / 2.0 - 28.0).abs() <= 2.0,
        "the word's centre is level with the chips: {}",
        y + h / 2.0
    );
}

#[test]
fn the_draft_chip_shows_only_with_a_draft_and_the_field_shows_the_draft() {
    let empty = shell("timeline", (1280.0, 800.0));
    let drawn = |rt: &Runtime<App>, wanted: &str| {
        drawn_texts(rt)
            .iter()
            .any(|(text, _)| text.starts_with(wanted))
    };
    assert!(!drawn(&empty, "DRAFT"), "no draft chip over an empty box");
    let typed = shell("timeline-draft", (1280.0, 800.0));
    assert!(drawn(&typed, "DRAFT"), "the draft chip shows with a draft");
    assert!(
        drawn(&typed, "Retry the fetch"),
        "the pre-typed draft is in the field from the first frame"
    );
}

#[test]
fn the_overlay_draws_a_find_line_and_cards_at_the_reference_size() {
    let rt = shell("overlay", (1280.0, 800.0));
    let vm = model(&rt);
    let texts = drawn_texts(&rt);
    assert!(
        texts
            .iter()
            .any(|(text, _)| text.starts_with("Find a view")),
        "the find line's prompt"
    );
    for surface in &vm.session.surfaces {
        let [_, _, w, h] = exactly_keyed(
            &rt,
            &format!("overlay:surface:{:?}", surface.surface_instance_id),
        )
        .expect("a card per open Surface");
        assert!(
            (w - 206.0).abs() < 0.5 && (h - 118.0).abs() < 0.5,
            "card {w}x{h}"
        );
    }
}

#[test]
fn a_narrow_overlay_wraps_its_cards_inside_the_sheet() {
    let rt = shell("overlay", (900.0, 800.0));
    let vm = model(&rt);
    let [sx, _, sw, _] = exactly_keyed(&rt, "overlay").expect("the sheet");
    for surface in &vm.session.surfaces {
        let [x, _, w, _] = exactly_keyed(
            &rt,
            &format!("overlay:surface:{:?}", surface.surface_instance_id),
        )
        .expect("a card per open Surface");
        assert!(
            x >= sx && x + w <= sx + sw + 0.5,
            "the card sits inside the sheet: {x}..{}",
            x + w
        );
    }
}

#[test]
fn the_project_scenes_mount_the_project_surface_with_threads_in_view() {
    for scene in ["project", "project-review", "project-launcher"] {
        let rt = shell(scene, (1440.0, 900.0));
        let vm = model(&rt);
        let text = serde_json::to_string(&*vm).unwrap();
        assert!(
            text.contains("Project"),
            "{scene}: the project Surface is active"
        );
        assert!(text.contains("T-7f3a"), "{scene}: thread rows are present");
        assert!(
            text.contains("T-0a55"),
            "{scene}: the last of eight threads is present"
        );
        // The Review tab's history tile is narrow: it drops the id column
        // (kind, item and status stay), so its rows are told by their item.
        let row = if scene == "project-review" {
            "worker:claude-code"
        } else {
            "T-7f3a"
        };
        let drawn = drawn_texts(&rt);
        assert!(
            drawn.iter().any(|(text, _)| text.contains(row)),
            "{scene}: the thread rows are drawn"
        );
    }
    let rt = shell("project-review", (1440.0, 900.0));
    assert!(
        drawn_texts(&rt)
            .iter()
            .any(|(text, _)| text.contains("Write a file outside the workspace")),
        "the Review tab draws the pending ask"
    );
}
