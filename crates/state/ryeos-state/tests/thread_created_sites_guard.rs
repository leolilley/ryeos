//! A thread's first event, `thread_created`, will carry the owner of the
//! work, stamped when the work is admitted (`ryeos_state::owner_stamp`). This
//! guard pins every production line in the workspace that names the birth
//! event: the daemon's builders, which must stamp, and the matchers and
//! definitions, which must not build one. A new line, or a moved one, fails
//! here until it is classified in `SITES`, so a new way to birth a thread
//! cannot be added without deciding whose work it records.
//!
//! Production text is read with the shared helper, so code under
//! `#[cfg(test)]` does not count.

use std::path::{Path, PathBuf};

#[path = "../../../../tests/support/rust_source_text.rs"]
mod rust_source_text;

/// (file, production lines naming the event, what those lines are). A BUILDER
/// constructs a `thread_created` event and so must stamp an owner: a ROOT
/// stamps its own, from the admitting key through the ownership resolver; a
/// CHILD or CONTINUATION copies the chain root's stamp; a REMOTE ADOPTION
/// stamps the receiver's own owner (a chain from another node never supplies
/// it). Everything else only matches or defines the event.
const SITES: &[(&str, usize, &str)] = &[
    (
        "crates/daemon/ryeos-app/src/state_store/thread_create.rs",
        4,
        "BUILDERS: the in-chain thread (:192), the in-process child birth (:418), the root with events (:665, ROOT), the trace branch (:971)",
    ),
    (
        "crates/daemon/ryeos-app/src/state_store/continuation.rs",
        2,
        "BUILDERS: the running-continuation successor (:966; CONTINUATION, and REMOTE ADOPTION on its RemoteAdoption path) and the admitted continuation (:1321, CONTINUATION)",
    ),
    (
        "crates/daemon/ryeos-app/src/state_store/thread_finalize.rs",
        1,
        "BUILDER: the admitted continuation created at finalize (:1023, CONTINUATION)",
    ),
    (
        "crates/daemon/ryeos-app/src/event_store_service.rs",
        1,
        "the daemon-only list: a runtime may not append a birth, so it cannot forge or restate an owner (pinned below)",
    ),
    (
        "crates/daemon/ryeos-app/src/thread_lifecycle.rs",
        1,
        "a check that the first event of a page is the birth",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/stream_helpers.rs",
        2,
        "matches the event for a stream lifecycle hint",
    ),
    (
        "crates/daemon/ryeos-ui/src/handlers/ui_seat/authenticated_history/fixed_anchor_replay.rs",
        1,
        "reads the event in a replay",
    ),
    (
        "crates/engine/ryeos-runtime/src/events.rs",
        2,
        "the runtime's wire enum maps the event name both ways",
    ),
    (
        "crates/state/ryeos-state/src/chain.rs",
        1,
        "the first-event check of a chain",
    ),
    (
        "crates/state/ryeos-state/src/event_types.rs",
        1,
        "the definition of the event name",
    ),
    (
        "crates/state/ryeos-state/src/projection/events.rs",
        1,
        "the projection reads the birth event (the usage subject, and the owner once stamped)",
    ),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root above crates/state/ryeos-state")
        .to_path_buf()
}

fn production_sources(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if kind.is_dir() {
            if matches!(name.as_str(), "tests" | "target" | "benches" | "examples") {
                continue;
            }
            production_sources(&path, out);
        } else if kind.is_file()
            && path.extension().is_some_and(|ext| ext == "rs")
            && !name.ends_with("tests.rs")
            && !name.contains("test_support")
        {
            out.push(path);
        }
    }
}

#[test]
fn every_production_line_naming_the_birth_event_is_classified() {
    let root = workspace_root();
    let mut files = Vec::new();
    production_sources(&root.join("crates"), &mut files);
    assert!(files.len() > 500, "the sweep found {} files", files.len());
    let mut found: Vec<(String, usize)> = Vec::new();
    for file in &files {
        let relative = file
            .strip_prefix(&root)
            .expect("source under the workspace")
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let production = rust_source_text::production_text(&relative, &text);
        let lines = production
            .lines()
            .filter(|line| line.contains("\"thread_created\"") || line.contains("THREAD_CREATED"))
            .count();
        if lines > 0 {
            found.push((relative, lines));
        }
    }
    found.sort();
    let mut expected: Vec<(String, usize)> = SITES
        .iter()
        .map(|(path, lines, _)| ((*path).to_owned(), *lines))
        .collect();
    expected.sort();
    assert_eq!(
        found, expected,
        "a production line naming the thread_created event appeared, moved or went: classify it in SITES (a builder must stamp the owner)"
    );
}

#[test]
fn a_runtime_cannot_append_a_birth_because_it_is_on_the_daemon_only_list() {
    let root = workspace_root();
    let relative = "crates/daemon/ryeos-app/src/event_store_service.rs";
    let text = std::fs::read_to_string(root.join(relative)).expect("read the event store service");
    let production = rust_source_text::production_text(relative, &text);
    let listed: Vec<&str> = production
        .lines()
        .map(str::trim)
        .filter(|line| *line == "ryeos_state::event_types::THREAD_CREATED,")
        .collect();
    assert_eq!(
        listed.len(),
        1,
        "thread_created must stay on the list of events only the daemon appends: that list keeps a runtime from forging a birth or restating an owner"
    );
}
