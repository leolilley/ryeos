//! The shell driven as an operator drives it, over the in-memory fixture:
//! presses, typing and keys go through Rune's own input routing, and the
//! assertions read the core's view model and the element tree.

#[path = "../fixtures/presentation.rs"]
mod presentation;

use std::rc::Rc;

use rune::{Key, KeyEvent, Mods, Runtime};
use ryeos_client_base::ui::view_model::{RyeOsInputVm, RyeOsViewModel};
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
fn a_narrow_page_moves_navigation_behind_a_toggle() {
    let narrow = shell("", (700.0, 800.0));
    assert!(keyed(&narrow, "navigation-toggle").is_some());
    let wide = shell("", (1280.0, 800.0));
    assert!(keyed(&wide, "navigation-toggle").is_none());
}
