//! A project head is keyed only by `ProjectHeadKey`: the owner from the
//! authenticated principal or a found head key, and the sha256 of a
//! `ProjectName`. This guard fails when source anywhere in the workspace
//! builds a head ref path by hand, adopts head coordinates outside the
//! readers of this node's own sealed records, or canonicalizes a checkout
//! path anywhere a head could be keyed by it.

use std::path::{Path, PathBuf};

/// Files that may format `projects/{..}/head` themselves: the one formatter
/// and the key type's own tests.
const HEAD_REF_FORMATTERS: &[&str] = &[
    "crates/state/ryeos-state/src/refs.rs",
    "crates/state/ryeos-state/src/project_identity.rs",
];

/// Readers of this node's own sealed records or ref tree, the only places
/// head coordinates may be taken back without a project name.
const SEALED_COORDINATE_READERS: &[&str] = &[
    "crates/state/ryeos-state/src/project_identity.rs",
    "crates/state/ryeos-state/src/refs.rs",
    "crates/bin/daemon/src/scheduler_impl.rs",
    "crates/daemon/ryeos-api/src/handlers/dedicated_sessions.rs",
    "crates/daemon/ryeos-api/src/routes/response_modes/execute_mode.rs",
    "crates/daemon/ryeos-app/src/state_store/credential_profiles.rs",
];

/// The live-root resolver, the only place a checkout path is canonicalized
/// into a project reference.
const LIVE_ROOT_CANONICALIZERS: &[&str] =
    &["crates/engine/ryeos-executor/src/execution/project_source.rs"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root above crates/state/ryeos-state")
        .to_path_buf()
}

fn rust_sources(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_head_key_is_built_from_anything_but_a_project_head_key() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);
    assert!(!files.is_empty(), "no sources under {}", root.display());
    let this_file = Path::new(file!())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let mut violations = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .expect("source under the workspace")
            .to_string_lossy()
            .replace('\\', "/");
        if relative.ends_with(&this_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let checks: [(&str, &[&str], &str); 3] = [
            (
                concat!("\"projects/", "{"),
                HEAD_REF_FORMATTERS,
                "formats a project head ref by hand; use ProjectHeadKey::ref_path",
            ),
            (
                concat!("ProjectHeadKey::", "from_sealed("),
                SEALED_COORDINATE_READERS,
                "adopts head coordinates outside this node's sealed-record readers",
            ),
            (
                concat!("canonical_project_", "ref("),
                LIVE_ROOT_CANONICALIZERS,
                "canonicalizes a checkout path where a head could be keyed by it",
            ),
        ];
        for (needle, allowed, reason) in checks {
            if text.contains(needle) && !allowed.contains(&relative.as_str()) {
                violations.push(format!("{relative}: {reason}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "project head keys must come from ProjectHeadKey:\n{}",
        violations.join("\n")
    );
}
