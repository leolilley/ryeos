// The node's revised-item event names a project by identity and never carries
// a checkout path, and a stored checkout path never keys what a session is
// told. The guard reads the event's own source whole and pins the lines that
// would let a path back in, so the shape cannot change without this file
// changing with it.

use std::path::{Path, PathBuf};

fn workspace_file(relative: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    std::fs::read_to_string(Path::new(&root).join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

/// The body of `pub struct ItemRevised { ... }`: the fields between its braces.
fn item_revised_fields(source: &str) -> Vec<String> {
    let start = source
        .find("pub struct ItemRevised {")
        .expect("ItemRevised is declared in item_revised.rs");
    let body = &source[start..];
    let end = body.find("\n}").expect("ItemRevised struct closes");
    body[..end]
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| line.starts_with("pub "))
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_revised_item_event_names_an_identity_and_carries_no_path() {
    let source = workspace_file("crates/daemon/ryeos-app/src/item_revised.rs");
    assert_eq!(
        item_revised_fields(&source),
        [
            "pub project: Option<ProjectIdentity>,",
            "pub item_ref: String,",
            "pub digest: Option<String>,",
            "pub revision: u64,",
            "pub change_set: Option<String>,",
        ],
        "ItemRevised may carry the project identity and the item's own fields only; a checkout path stays in RevisedAt, which never reaches a subscriber"
    );
}

#[test]
fn the_fan_out_matches_by_identity_and_asks_the_node_at_every_delivery() {
    let source = workspace_file("crates/daemon/ryeos-ui/src/item_hints.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("item_hints.rs has production code");
    for required in [
        "authority.may_read(&key, event.project.as_ref())",
        "authority.checkout_identity(Path::new(&checkout)).as_ref() == project",
    ] {
        assert!(
            production.contains(required),
            "the fan-out no longer contains `{required}`"
        );
    }
    for forbidden in [
        "event.project.as_deref()",
        "Some(project.as_str())",
        "registered_project_id",
    ] {
        assert!(
            !production.contains(forbidden),
            "the fan-out compares projects by path or by a stored id again: `{forbidden}`"
        );
    }
}

#[test]
fn only_the_operator_handler_takes_a_checkout_path_in_a_request() {
    // The one request that names a checkout path is the operator's own
    // `items/revised` call; the event it produces carries the registry's
    // identity, resolved after admission, never the path it was given.
    let handler = workspace_file("crates/daemon/ryeos-api/src/handlers/items_revised.rs");
    assert!(
        handler.contains("registered_identity_for_checkout"),
        "items/revised stamps the identity the registry holds for the checkout"
    );
    assert!(
        handler.contains("require_operator_call(&ctx)?"),
        "items/revised stays an operator-only call"
    );
}
