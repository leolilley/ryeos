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
    let mut rt = fixture_runtime(presentation::core(scene), size);
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

fn type_text(rt: &mut Runtime<App>, text: &str) {
    for ch in text.chars() {
        rt.key(KeyEvent {
            key: Key::Text(ch.to_string()),
            mods: Mods::default(),
        });
        rt.frame(FRAME);
    }
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
    assert_eq!(query.text, "new");
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

#[test]
fn the_launcher_opens_from_the_status_bar_and_closes_from_its_control() {
    let mut rt = shell("", (1280.0, 800.0));
    assert!(model(&rt).dialogs.is_empty());
    press(&mut rt, "shell:launch");
    assert_eq!(
        model(&rt).dialogs.len(),
        1,
        "the status bar opened the launcher"
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

#[test]
fn the_system_bar_never_shows_the_project_path() {
    for scene in ["", "field", "launcher"] {
        let rt = shell(scene, (1280.0, 800.0));
        let vm = model(&rt);
        let path = vm
            .session
            .project_path
            .clone()
            .expect("the fixture session has a project path to hide");
        // The system bar is the first 28 points of the page.
        let bar = texts_in_band(&rt, 0.0, 28.0);
        assert!(!bar.is_empty(), "{scene}: the system bar says something");
        for text in &bar {
            assert!(
                !text.contains(&path) && !text.contains("/home/"),
                "{scene}: the system bar shows {text:?}"
            );
        }
    }
}

#[test]
fn the_status_bar_shows_only_its_segments() {
    let rt = shell("", (1280.0, 800.0));
    let vm = model(&rt);
    let status = &vm.presentation.chrome.status_bar;
    let Some([_, top, _, height]) = keyed(&rt, "shell:launch") else {
        assert!(
            !status.visible,
            "a visible status bar draws its launch control"
        );
        return;
    };
    assert!(status.visible);
    let expected: Vec<String> = status
        .segments
        .iter()
        .map(|segment| {
            match segment.label.as_deref().filter(|label| !label.is_empty()) {
                Some(label) => format!("{label} {}", segment.value),
                None => segment.value.clone(),
            }
            .to_uppercase()
        })
        .collect();
    let shown = texts_in_band(&rt, top, top + height);
    for segment in &expected {
        assert!(shown.contains(segment), "segment {segment:?} in {shown:?}");
    }
    assert!(
        shown.contains(&"RYEOS".to_owned()),
        "the launch control's name"
    );
    let others: Vec<&String> = shown
        .iter()
        .filter(|text| *text != "RYEOS" && !expected.contains(text))
        .collect();
    assert!(
        others.len() <= 1,
        "only the key hint may follow the segments, found {others:?}"
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

/// RyeOS fills the bottom-left corner and opens the launcher; the open
/// Surfaces run along the top from the left, the current one filled; the two
/// bars are the same height.
#[test]
fn ryeos_holds_the_bottom_left_corner_and_the_bars_match() {
    let (width, height) = (1280.0, 800.0);
    let rt = shell("", (width, height));
    let [x, y, _, bottom] = keyed(&rt, "shell:launch").expect("the launch control is drawn");
    assert_eq!(x, 0.0, "RyeOS starts at the left edge");
    assert!(
        (y + bottom - height).abs() < 0.5,
        "RyeOS reaches the bottom edge"
    );
    let vm = model(&rt);
    let current = vm
        .session
        .surfaces
        .iter()
        .find(|surface| surface.active)
        .expect("the fixture has a current Surface");
    let [_, top, _, tall] = keyed(&rt, &format!("surface:{:?}", current.surface_instance_id))
        .expect("the current Surface is in the top bar");
    assert!(top.abs() < 0.5, "the Surfaces sit in the top bar");
    assert!(
        (tall - bottom).abs() <= 1.0,
        "top {tall} and bottom {bottom} bars match"
    );
}
