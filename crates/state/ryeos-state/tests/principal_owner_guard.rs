//! A project owner is a principal, and the only way to get one from a key
//! is the node's ownership resolver. This guard fails when source anywhere
//! in the workspace:
//! - computes a solo principal outside the resolver, which would bring back
//!   "the owner is the authenticating key" as a convention;
//! - compares a project owner with a key fingerprint string;
//! - asks whether a key speaks for a principal outside the files allowed to
//!   decide project ownership (a link carries ownership, never authority:
//!   grant, scope, session and thread-read code must never call it).

use std::path::{Path, PathBuf};

/// The principal type itself and the resolver: the only producers of a solo
/// principal.
const SOLO_PRINCIPAL_PRODUCERS: &[&str] = &[
    "crates/state/ryeos-state/src/principal_id.rs",
    "crates/engine/ryeos-engine/src/authority/trust/ownership.rs",
];

/// Files that may ask whether a key speaks for a principal: the resolver,
/// the trust store and its endorsement ledger, and the project-ownership
/// sites (head advances, checkout registration, resume re-checks).
const SPEAKS_FOR_CALLERS: &[&str] = &[
    "crates/engine/ryeos-engine/src/authority/trust.rs",
    "crates/engine/ryeos-engine/src/authority/trust/ownership.rs",
    "crates/engine/ryeos-engine/src/authority/trust/endorsement.rs",
    "crates/engine/ryeos-engine/src/authority/trust/endorsement/tests.rs",
    "crates/daemon/ryeos-app/src/project_owner.rs",
    "crates/daemon/ryeos-api/src/handlers/dedicated_sessions.rs",
    "crates/daemon/ryeos-api/src/handlers/remote_pull_worker_result.rs",
    "crates/daemon/ryeos-api/src/handlers/remote_reconcile_project_head.rs",
    "crates/daemon/ryeos-api/src/routes/response_modes/execute_mode.rs",
    "crates/engine/ryeos-executor/src/execution/runner.rs",
    "crates/daemon/ryeos-app/src/worker_handoff.rs",
];

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
fn owners_come_only_from_the_resolver_and_are_never_compared_with_keys() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);
    rust_sources(&root.join("tests"), &mut files);
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
        let allowed_checks: [(&str, &[&str], &str); 2] = [
            (
                concat!("PrincipalId::", "solo("),
                SOLO_PRINCIPAL_PRODUCERS,
                "computes a solo principal outside the ownership resolver; ask the resolver (owner_for)",
            ),
            (
                concat!("speaks_", "for("),
                SPEAKS_FOR_CALLERS,
                "asks whether a key speaks for a principal outside the project-ownership sites",
            ),
        ];
        for (needle, allowed, reason) in allowed_checks {
            if text.contains(needle) && !allowed.contains(&relative.as_str()) {
                violations.push(format!("{relative}: {reason}"));
            }
        }
        let needle = concat!("owner().", "strip_prefix(");
        if text.contains(needle) {
            violations.push(format!(
                "{relative}: compares a project owner with a key fingerprint"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "project owners must come from the ownership resolver:\n{}",
        violations.join("\n")
    );
}

/// Files whose production code may build a `HeadAdvancer` directly: the
/// record type, the resolver (which checks the key acts for the head's owner
/// first), and the node's own compaction rewrite.
const HEAD_ADVANCER_BUILDERS: &[&str] = &[
    "crates/state/ryeos-state/src/head_advance.rs",
    "crates/daemon/ryeos-app/src/project_owner.rs",
    "crates/state/ryeos-state/src/gc/compact.rs",
];

/// Production code of a source file: everything before its first test
/// module. Test fixtures may build records directly.
fn production_text(text: &str) -> &str {
    text.find("#[cfg(test)]").map_or(text, |end| &text[..end])
}

#[test]
fn every_head_advance_gets_its_advancer_from_the_resolver() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);
    let mut violations = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .expect("source under the workspace")
            .to_string_lossy()
            .replace('\\', "/");
        if relative.contains("/tests/")
            || relative.ends_with("tests.rs")
            || relative.ends_with("/tests.rs")
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let needle = concat!("HeadAdvancer", " {");
        if production_text(&text).contains(needle)
            && !HEAD_ADVANCER_BUILDERS.contains(&relative.as_str())
        {
            violations.push(format!(
                "{relative}: builds a head advancer by hand; use project_owner::head_advancer"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "head advances must take their advancer from the ownership resolver:\n{}",
        violations.join("\n")
    );
}
