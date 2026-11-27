//! The services a node answers with no key. A service is keyless exactly when
//! its `disclosure` block declares a `public` tier, so the set of such
//! services is pinned here: a new keyless service fails this test until it is
//! reviewed and added.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryeos_api::handlers::{health_status, identity_public_key};
use ryeos_api::test_support as test_state;
use ryeos_node_runtime::disclosure::{DisclosureClass, ServiceDisclosure, shape_result};
use serde_json::Value;

/// Every service that still declares a `public` tier. Each is keyless on
/// purpose: liveness, the node's public identity, the ingest rules a pusher
/// reads before it is admitted, the token-gated admission claim, and the
/// one-time browser launch link.
const KEYLESS: &[&str] = &[
    "admission/claim",
    "health/status",
    "identity/public_key",
    "system/ingest-ignore",
    "ui/launch",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            out.push(path);
        }
    }
}

/// Every service item in every bundle: its name and parsed body.
fn services() -> Vec<(String, Value)> {
    let mut found = Vec::new();
    for bundle in std::fs::read_dir(repo().join("bundles")).unwrap().flatten() {
        let root = bundle.path().join(".ai/services");
        let mut files = Vec::new();
        collect(&root, &mut files);
        for file in files {
            let name = file
                .strip_prefix(&root)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .into_owned();
            let body: Value = serde_yaml::from_str(&std::fs::read_to_string(&file).unwrap())
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            found.push((name, body));
        }
    }
    found
}

fn disclosure_of(name: &str) -> ServiceDisclosure {
    let (_, body) = services()
        .into_iter()
        .find(|(found, _)| found == name)
        .unwrap_or_else(|| panic!("no service {name}"));
    ServiceDisclosure::parse(body.get("disclosure").expect("disclosure block")).unwrap()
}

#[test]
fn the_services_with_a_public_tier_are_exactly_the_pinned_list() {
    let declared: BTreeSet<String> = services()
        .into_iter()
        .filter(|(_, body)| {
            body.get("disclosure")
                .and_then(|block| block.get("public"))
                .and_then(Value::as_array)
                .is_some_and(|entries| !entries.is_empty())
        })
        .map(|(name, _)| name)
        .collect();
    let pinned: BTreeSet<String> = KEYLESS.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        declared, pinned,
        "the keyless services changed: a service with a `public` tier answers with no key, \
         so review it and update KEYLESS"
    );
}

#[test]
fn the_keyless_health_answer_is_liveness_alone() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let full = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(health_status::handle(
            health_status::Request::default(),
            Arc::new(state),
        ))
        .unwrap();
    let disclosure = disclosure_of("health/status");
    let shaped = shape_result(
        DisclosureClass::Public,
        Some(&disclosure),
        full.clone(),
        disclosure.bounds(),
    )
    .unwrap();
    let keys: Vec<&String> = shaped.as_object().expect("an object").keys().collect();
    assert_eq!(keys, vec!["status"], "{shaped}");
    assert!(shaped["status"].is_string());
    // The handler answers more; a keyless caller gets none of it.
    assert!(full.get("operational_services").is_some());
}

#[test]
fn the_keyless_public_key_answer_carries_no_version_or_operator_detail() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let full = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(identity_public_key::handle(
            identity_public_key::Request::default(),
            Arc::new(state),
        ))
        .unwrap();
    let disclosure = disclosure_of("identity/public_key");
    let shaped = shape_result(
        DisclosureClass::Public,
        Some(&disclosure),
        full.clone(),
        disclosure.bounds(),
    )
    .unwrap();
    let keys: BTreeSet<&str> = shaped
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert!(keys.contains("signing_key") && keys.contains("fingerprint"));
    assert!(!keys.contains("version"), "{shaped}");
    let allowed: BTreeSet<&str> = [
        "principal_id",
        "fingerprint",
        "signing_key",
        "site_id",
        "vault_fingerprint",
    ]
    .into_iter()
    .collect();
    assert!(keys.is_subset(&allowed), "{shaped}");
    assert!(full.get("version").is_some());
}
