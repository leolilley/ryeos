//! The keyed element rectangles of every scene at three viewport sizes,
//! checked in as text, and the guarantee that building a scene twice gives the
//! same shell. Set `RYEOS_UPDATE_SNAPSHOTS=1` to rewrite the checked-in files.

#[path = "../fixtures/presentation.rs"]
mod presentation;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use rune::Runtime;
use ryeos_client_rune::{App, fixture_runtime};

const FRAME: f64 = 1.0 / 60.0;
const UPDATE_SNAPSHOTS_ENV: &str = "RYEOS_UPDATE_SNAPSHOTS";
/// The viewports the rectangles are taken at: the design size, a compact
/// width and a narrow one.
const SIZES: [(u32, u32); 3] = [(1600, 1000), (1100, 800), (640, 800)];

fn shell(scene: &str, (width, height): (u32, u32)) -> Runtime<App> {
    let mut rt = fixture_runtime(
        presentation::core(scene),
        presentation::dress(scene),
        (width as f32, height as f32),
    );
    rt.frame(0.0);
    // Layout reports the viewport on the first frame and the shell redraws at
    // its next wake; a second of frames settles both, as a snapshot does.
    for _ in 0..60 {
        rt.frame(FRAME);
    }
    rt
}

/// One line per keyed element, `key x y w h`, sorted by key then rectangle.
fn rect_lines(rt: &Runtime<App>) -> Vec<String> {
    let mut lines: Vec<String> = rt
        .elements()
        .into_iter()
        .filter(|element| !element.exiting)
        .filter_map(|element| {
            let key = element.key?;
            let [x, y, w, h] = element.rect;
            Some(format!("{key} {x:.2} {y:.2} {w:.2} {h:.2}"))
        })
        .collect();
    lines.sort();
    lines
}

fn snapshot_path(scene: &str, (width, height): (u32, u32)) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/rects")
        .join(format!("{scene}@{width}x{height}.txt"))
}

/// Lines by element, numbering a key that repeats so each occurrence compares
/// with its own.
fn by_element(lines: &[String]) -> BTreeMap<String, String> {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let mut found = BTreeMap::new();
    for line in lines {
        let split = line.rsplitn(5, ' ').last().unwrap_or(line);
        let nth = seen.entry(split).or_default();
        *nth += 1;
        let rect = line[split.len()..].trim().to_owned();
        found.insert(format!("{split}#{nth}"), rect);
    }
    found
}

fn diff(expected: &[String], actual: &[String]) -> String {
    let (expected, actual) = (by_element(expected), by_element(actual));
    let mut out = String::new();
    for (key, was) in &expected {
        match actual.get(key) {
            None => writeln!(out, "  removed  {key}  was {was}").unwrap(),
            Some(now) if now != was => {
                writeln!(out, "  changed  {key}  {was} -> {now}").unwrap();
            }
            Some(_) => {}
        }
    }
    for (key, now) in &actual {
        if !expected.contains_key(key) {
            writeln!(out, "  added    {key}  {now}").unwrap();
        }
    }
    out
}

#[test]
fn every_scene_keeps_its_element_rectangles_at_every_size() {
    let update = std::env::var_os(UPDATE_SNAPSHOTS_ENV).is_some();
    let mut failures = String::new();
    for scene in presentation::SCENES {
        for size in SIZES {
            let actual = rect_lines(&shell(scene, size));
            let path = snapshot_path(scene, size);
            let text = actual.join("\n") + "\n";
            if update {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &text).unwrap();
                continue;
            }
            match std::fs::read_to_string(&path) {
                Err(error) => writeln!(failures, "{}: {error}", path.display()).unwrap(),
                Ok(expected) => {
                    let expected: Vec<String> = expected.lines().map(str::to_owned).collect();
                    if expected != actual {
                        writeln!(
                            failures,
                            "{scene}@{}x{}:\n{}",
                            size.0,
                            size.1,
                            diff(&expected, &actual)
                        )
                        .unwrap();
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "element rectangles differ (rerun with {UPDATE_SNAPSHOTS_ENV}=1 to rewrite):\n{failures}"
    );
}

/// The checked-in files are exactly the scenes at the three sizes: no file
/// left behind by a removed scene.
#[test]
fn the_rectangle_files_are_exactly_the_scenes_at_the_three_sizes() {
    if std::env::var_os(UPDATE_SNAPSHOTS_ENV).is_some() {
        return;
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/rects");
    let mut on_disk: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    on_disk.sort();
    let mut expected: Vec<String> = presentation::SCENES
        .iter()
        .flat_map(|scene| {
            SIZES
                .iter()
                .map(move |(w, h)| format!("{scene}@{w}x{h}.txt"))
        })
        .collect();
    expected.sort();
    assert_eq!(on_disk, expected);
}

/// The same scene built again is the same shell: the view model (notice
/// numbers and focus included) and every keyed rectangle. Each build makes its
/// own hash maps, so an order that leaks from one into the core shows here.
#[test]
fn building_a_scene_again_gives_the_same_shell() {
    for scene in presentation::SCENES {
        let first = shell(scene, SIZES[0]);
        let model = serde_json::to_string(&*first.state.view_model().unwrap()).unwrap();
        let lines = rect_lines(&first);
        for build in 1..8 {
            let again = shell(scene, SIZES[0]);
            assert_eq!(
                serde_json::to_string(&*again.state.view_model().unwrap()).unwrap(),
                model,
                "scene {scene}: the view model of build {build} differs from the first"
            );
            assert_eq!(
                rect_lines(&again),
                lines,
                "scene {scene}: the rectangles of build {build} differ from the first"
            );
        }
    }
}
