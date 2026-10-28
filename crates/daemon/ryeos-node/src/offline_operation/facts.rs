//! The node facts a request is signed against, and how they are measured.
//!
//! Every fact is the SHA-256 of raw bytes on the volume, never of a decoded
//! view: the node a request is for may be exactly the one whose configuration
//! no longer decodes. A fact whose subject is absent is `absent`; one that
//! cannot be read safely is `unreadable`. Measuring never fails and never
//! writes.

use std::collections::BTreeMap;
use std::path::Path;

use lillux::DirectoryTraversalBudget;
use ryeos_engine::roots::RuntimeRoot;
use serde_json::json;

use super::contract::{FACT_ABSENT, FACT_UNREADABLE};

/// Digest of the signed init completion record.
pub const FACT_INIT_COMPLETION: &str = "init_completion";
/// Digest of the last applied receipt, or the genesis head.
pub const FACT_LEDGER_HEAD: &str = "ledger_head";
/// Digest of the files of the signed node-policy generation.
pub const FACT_POLICY_GENERATION: &str = "policy_generation";
/// Digest of the native-capacity account and the operator's onboarding
/// documents that authorize its renewals and moves.
pub const FACT_CAPACITY_ACCOUNT: &str = "capacity_account";
/// Digest of the restart inhibition record.
pub const FACT_RESTART_INHIBITION: &str = "restart_inhibition";
/// Digest of the node-signed authorized-key grants.
pub const FACT_AUTHORIZED_KEYS: &str = "authorized_keys";
/// Digest of the installed external-execution bindings.
pub const FACT_EXTERNAL_EXECUTION: &str = "external_execution_bindings";
/// Digest of the installed runtime-snapshot production and qualification
/// bindings.
pub const FACT_RUNTIME_SNAPSHOTS: &str = "runtime_snapshot_bindings";

/// Every fact the node can report, in report order.
pub const ALL_FACTS: &[&str] = &[
    FACT_INIT_COMPLETION,
    FACT_LEDGER_HEAD,
    FACT_POLICY_GENERATION,
    FACT_CAPACITY_ACCOUNT,
    FACT_RESTART_INHIBITION,
    FACT_AUTHORIZED_KEYS,
    FACT_EXTERNAL_EXECUTION,
    FACT_RUNTIME_SNAPSHOTS,
];

/// Facts every request pins, whatever its operation.
pub const MANDATORY_FACTS: &[&str] = &[FACT_INIT_COMPLETION, FACT_LEDGER_HEAD];

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const TREE_BUDGET: DirectoryTraversalBudget = DirectoryTraversalBudget::new(8192, 8);

/// The measured facts of one node.
pub type NodeFacts = BTreeMap<String, String>;

/// Measure every fact. `ledger_head` is supplied by the caller, who has
/// verified the ledger it comes from.
pub fn measure(app_root: &Path, ledger_head: &str) -> NodeFacts {
    let root = RuntimeRoot::new(app_root.to_path_buf());
    let ai = root.ai();
    let node = root.node();
    let mut facts = NodeFacts::new();
    facts.insert(
        FACT_INIT_COMPLETION.to_owned(),
        file_digest(&ai.join("config/onboarding/init-completion.json")),
    );
    facts.insert(FACT_LEDGER_HEAD.to_owned(), ledger_head.to_owned());
    facts.insert(
        FACT_POLICY_GENERATION.to_owned(),
        tree_digest(&ryeos_node_runtime::node_policy::generation::policy_directory(app_root)),
    );
    facts.insert(
        FACT_CAPACITY_ACCOUNT.to_owned(),
        combined_digest(&[
            ("account", tree_digest(&node.join("native-capacity"))),
            ("onboarding", tree_digest(&ai.join("config/onboarding"))),
        ]),
    );
    facts.insert(
        FACT_RESTART_INHIBITION.to_owned(),
        file_digest(
            &root
                .state()
                .join(crate::lifecycle_marker::RESTART_INHIBIT_FILE),
        ),
    );
    facts.insert(
        FACT_AUTHORIZED_KEYS.to_owned(),
        tree_digest(&root.authorized_keys_dir()),
    );
    facts.insert(
        FACT_EXTERNAL_EXECUTION.to_owned(),
        tree_digest(&node.join("external_execution")),
    );
    facts.insert(
        FACT_RUNTIME_SNAPSHOTS.to_owned(),
        combined_digest(&[
            (
                "production",
                tree_digest(&node.join("runtime_snapshot_production")),
            ),
            (
                "qualification",
                tree_digest(&node.join("runtime_snapshot_qualification")),
            ),
        ]),
    );
    facts
}

fn combined_digest(parts: &[(&str, String)]) -> String {
    if parts.iter().all(|(_, digest)| digest == FACT_ABSENT) {
        return FACT_ABSENT.to_owned();
    }
    if parts.iter().any(|(_, digest)| digest == FACT_UNREADABLE) {
        return FACT_UNREADABLE.to_owned();
    }
    let map: BTreeMap<&str, &str> = parts
        .iter()
        .map(|(name, digest)| (*name, digest.as_str()))
        .collect();
    match lillux::canonical_json(&json!(map)) {
        Ok(text) => lillux::sha256_hex(text.as_bytes()),
        Err(_) => FACT_UNREADABLE.to_owned(),
    }
}

/// Digest of one regular file, read without following links.
pub fn file_digest(path: &Path) -> String {
    match lillux::read_optional_regular_file_bounded_no_follow(path, MAX_FILE_BYTES) {
        Ok(Some(bytes)) => lillux::sha256_hex(&bytes),
        Ok(None) => FACT_ABSENT.to_owned(),
        Err(_) => FACT_UNREADABLE.to_owned(),
    }
}

/// Digest of every regular file below `root` by relative path and content.
/// A link or special entry anywhere below makes the tree unreadable.
pub fn tree_digest(root: &Path) -> String {
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    let outcome = lillux::visit_regular_files_no_follow_bounded(
        root,
        TREE_BUDGET,
        |_, _| Ok(false),
        |relative, file| {
            let bytes = lillux::read_open_regular_file_bounded(file, MAX_FILE_BYTES)?;
            entries.insert(
                relative.to_string_lossy().into_owned(),
                lillux::sha256_hex(&bytes),
            );
            Ok(())
        },
    );
    match outcome {
        Ok(false) => FACT_ABSENT.to_owned(),
        Ok(true) => match lillux::canonical_json(&json!(entries)) {
            Ok(text) => lillux::sha256_hex(text.as_bytes()),
            Err(_) => FACT_UNREADABLE.to_owned(),
        },
        Err(_) => FACT_UNREADABLE.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_subjects_are_marked_absent() {
        let root = tempfile::tempdir().unwrap();
        let facts = measure(root.path(), "head");
        assert_eq!(facts[FACT_INIT_COMPLETION], FACT_ABSENT);
        assert_eq!(facts[FACT_POLICY_GENERATION], FACT_ABSENT);
        assert_eq!(facts[FACT_CAPACITY_ACCOUNT], FACT_ABSENT);
        assert_eq!(facts[FACT_RUNTIME_SNAPSHOTS], FACT_ABSENT);
        assert_eq!(facts[FACT_LEDGER_HEAD], "head");
        assert_eq!(facts.len(), ALL_FACTS.len());
    }

    #[test]
    fn a_tree_digest_follows_content_and_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"one").unwrap();
        let first = tree_digest(root.path());
        std::fs::write(root.path().join("a"), b"two").unwrap();
        let second = tree_digest(root.path());
        std::fs::write(root.path().join("b"), b"two").unwrap();
        let third = tree_digest(root.path());
        assert!(first != second && second != third);
        assert_eq!(tree_digest(root.path()), third);
    }
}
