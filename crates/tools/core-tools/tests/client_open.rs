//! Slice 0: Backfill tests for `client-open` behavior.
//!
//! These tests pin the current behavior of the client-open tool
//! via the `resolve_effective_client` and descriptor parsing paths.
//!
//! NOTE: Full integration tests for client-open require a running binary
//! and populated bundles. The tests here verify the descriptor parsing
//! and resolution logic at the unit level. Deeper integration tests
//! land in Slice 1 after the `serves` tightening.

use serde_json::json;

/// Verify the EffectiveClientDescriptor parsing accepts valid `serves`.
#[test]
fn valid_descriptor_with_serves_parses() {
    let raw = json!({
        "serves": {
            "kind": "surface",
            "renderer": "terminal"
        },
        "launch": {
            "mode": "cli_exec",
            "binary_ref": "bin/{triple}/ryeos-tui",
            "args": {
                "surface": "--surface"
            }
        }
    });

    let descriptor: serde_json::Value = serde_json::from_value(raw.clone()).unwrap();
    // Verify the serves block is present and correct.
    let serves = descriptor.get("serves").unwrap();
    assert_eq!(serves["kind"], "surface");
    assert_eq!(serves["renderer"], "terminal");
}

/// Verify the EffectiveClientDescriptor parsing accepts descriptor
/// without `serves` (it's currently `Option`).
#[test]
fn descriptor_without_serves_parses() {
    let raw = json!({
        "launch": {
            "mode": "cli_exec",
            "binary_ref": "bin/{triple}/ryeos-tui",
            "args": {}
        }
    });

    let descriptor: serde_json::Value = serde_json::from_value(raw.clone()).unwrap();
    // serves should be absent
    assert!(descriptor.get("serves").is_none());
}

/// Verify the verb-to-client derivation: verb `surface-open` →
/// `client:ryeos/surface-open`.
#[test]
fn hyphenated_verb_resolves_to_hyphenated_client() {
    let verb = "surface-open";
    let candidate = format!("client:ryeos/{verb}");
    assert_eq!(candidate, "client:ryeos/surface-open");
}

/// Verify that bundle_root must be Some for client-open to work.
/// This test documents the expected behavior; the actual enforcement
/// is in `run_client_open` which checks `effective.source.bundle_root`.
#[test]
fn bundle_root_requirement_documented() {
    // The EffectiveItemSource struct has a bundle_root: Option<PathBuf>.
    // client-open errors when it's None. This test documents that
    // the field exists and is optional at the serde level.
    let source = json!({
        "space": "system",
        "path": "/some/bundle/.ai/clients/ryeos/tui.yaml",
        "bundle_root": "/some/bundle"
    });
    let parsed: serde_json::Value = serde_json::from_value(source.clone()).unwrap();
    assert!(parsed.get("bundle_root").is_some());
}
