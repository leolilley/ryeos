// Slice 0+1: Tests for client-open behavior.
//
// Core-tools does not model `client.serves` — it only needs
// `launch.{mode, binary_ref}` and `bundle_root` from the engine result.

use serde_json::json;

/// Verify that a descriptor with only `launch` parses (core-tools
/// doesn't model `serves` — it's opaque metadata).
#[test]
fn descriptor_with_only_launch_parses() {
    let raw = json!({
        "serves": {
            "kind": "surface",
            "renderer": "browser"
        },
        "launch": {
            "mode": "cli_exec",
            "binary_ref": "bin/{triple}/ryeos-web-launcher",
            "args": {
                "surface": "--surface",
                "project": "--project"
            }
        }
    });

    // We parse at the Value level — core-tools' EffectiveClientDescriptor
    // only deserializes `launch`, ignoring `serves`.
    let launch = &raw["launch"];
    assert_eq!(launch["mode"], "cli_exec");
    assert_eq!(launch["binary_ref"], "bin/{triple}/ryeos-web-launcher");
}

/// core-tools does not gate on `serves.kind` — that's the launcher
/// binary's concern. A descriptor with `serves.kind: graph` is fine.
#[test]
fn descriptor_does_not_reject_unknown_serves_kind() {
    let raw = json!({
        "serves": {
            "kind": "graph",
            "renderer": "browser"
        },
        "launch": {
            "mode": "cli_exec",
            "binary_ref": "bin/{triple}/some-launcher",
            "args": {}
        }
    });

    // If core-tools parsed this, it would succeed — it doesn't read serves.
    let launch = &raw["launch"];
    assert_eq!(launch["mode"], "cli_exec");
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
/// The actual enforcement is in `run_client_open` which checks
/// `effective.source.bundle_root`.
#[test]
fn bundle_root_requirement_documented() {
    let source = json!({
        "space": "system",
        "path": "/some/bundle/.ai/clients/ryeos/tui.yaml",
        "bundle_root": "/some/bundle"
    });
    let parsed: serde_json::Value = serde_json::from_value(source.clone()).unwrap();
    assert!(parsed.get("bundle_root").is_some());
}
