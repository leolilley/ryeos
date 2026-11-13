//! Test support: draw one element tree in a windowless runtime and read it
//! back through Rune's element list, the way a screen reader or a snapshot
//! sees it.

use std::cell::RefCell;

use rune::{AppSpec, Cx, El, Env, Runtime, el};

use crate::App;

type Build = Box<dyn Fn() -> El>;

thread_local! {
    static BUILD: RefCell<Option<Build>> = const { RefCell::new(None) };
}

fn view(_: &App, _: &mut Cx<App>) -> El {
    BUILD.with(|build| match &*build.borrow() {
        Some(build) => build(),
        None => el(),
    })
}

/// A windowless runtime of `size` that draws whatever `build` returns. The
/// state is the real `App`, which has no core: presses run the renderer's own
/// handlers and the core ignores them.
pub(crate) fn runtime(size: (f32, f32), build: impl Fn() -> El + 'static) -> Runtime<App> {
    BUILD.with(|slot| *slot.borrow_mut() = Some(Box::new(build)));
    let mut rt = rune::App::new(AppSpec::new("probe", size, App::new, view))
        .headless_runtime(size, Env::new(true, 1.0));
    for _ in 0..3 {
        rt.frame(1.0 / 60.0);
    }
    rt
}

/// Everything the tree says in text, in tree order.
pub(crate) fn texts(rt: &Runtime<App>) -> Vec<String> {
    rt.elements()
        .into_iter()
        .filter_map(|element| element.text)
        .collect()
}

/// The text under the first element named `label`, in tree order.
pub(crate) fn texts_under(rt: &Runtime<App>, label: &str) -> Vec<String> {
    let elements = rt.elements();
    let Some(root) = elements
        .iter()
        .find(|element| element.label.as_deref() == Some(label))
    else {
        return Vec::new();
    };
    let prefix = format!("{}/", root.path);
    elements
        .iter()
        .filter(|element| element.path.starts_with(&prefix))
        .filter_map(|element| element.text.clone())
        .collect()
}

/// Whether a text equal to `wanted` is drawn.
pub(crate) fn says(rt: &Runtime<App>, wanted: &str) -> bool {
    texts(rt).iter().any(|text| text == wanted)
}
