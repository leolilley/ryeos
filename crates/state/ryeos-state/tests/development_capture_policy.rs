//! Repository-data contract test, not project-specific capture behavior.
use ryeos_state::ignore::{IgnoreConfig, IgnoreMatcher};
use ryeos_state::project_sync::{ProjectSyncScope, capture_snapshot_policy};

#[test]
fn development_source_policy_keeps_source_and_excludes_generated_payloads() {
    let profile: serde_yaml::Value = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../bundles/.ai/node/init/profiles/development.yaml"
    )))
    .unwrap();
    let node = IgnoreMatcher::from_config(&IgnoreConfig {
        patterns: serde_yaml::from_value(profile["policies"]["ingest_ignore"]["patterns"].clone())
            .unwrap(),
    })
    .unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let policy = capture_snapshot_policy(root, &node, ProjectSyncScope::FullProject).unwrap();
    let matcher = policy.matcher().unwrap();
    for generated in [
        ".tmp/platform/rust/bin/rustc",
        ".worktrees/other-project/Cargo.toml",
        ".local/node/state.sqlite3",
        "target/release/ryeos",
        "bundles/.publisher-locks/core.lock",
        ".ai/bin/ryeos-development-operation",
        ".ai/objects/generated-object",
        ".ai/refs/generated-head",
        "nvim.log",
        ".ai/directives/test/replay-smoke.md",
        ".ai/tools/test/echo.yaml",
        ".ai/surfaces/ryeos/ui/atlas.yaml",
        "tests/e2e/execute-stream-latency/.ai/state/threads/old/thread.json",
        "tests/e2e/execute-stream-latency/outputs/probe/samples.jsonl",
        "tests/e2e/manual-directives/.ai/state/threads/old/thread.json",
        "tests/e2e/manual-directives/.ai/knowledge/agent/threads/old.md",
        "bundles/sandbox-linux-bubblewrap/.ai/bin/retired-adapter",
        "bundles/tv-tracker-authoring/PUBLISHER_TRUST.toml",
        "bundles/tv-tracker-authoring/.ai/bin/generated-tool",
    ] {
        assert!(matcher.is_ignored(generated), "captured {generated}");
    }
    for bundle in profile["exact_bundles"].as_sequence().unwrap() {
        let bundle = bundle.as_str().unwrap();
        for generated in ["bin", "objects", "refs"] {
            let path = format!("bundles/{bundle}/.ai/{generated}/artifact");
            assert!(matcher.is_ignored(&path), "captured {path}");
        }
        let source = format!("bundles/{bundle}/.ai/tools/source.yaml");
        assert!(!matcher.is_ignored(&source), "excluded {source}");
    }
    for source in [
        "Cargo.lock",
        "crates/bin/daemon/src/main.rs",
        ".dev-keys/PUBLISHER_DEV.pem",
        ".ai/config/execution/project-snapshot.yaml",
        ".ai/config/development/ryeos/stage0-platform-x86_64-linux.yaml",
        ".ai/tools/ryeos/development/check.yaml",
        "tests/e2e/execute-stream-latency/run-latency-matrix.mjs",
        "tests/e2e/manual-directives/.ai/directives/test/context/base_context.md",
        "tests/e2e/manual-directives/.ai/knowledge/test-findings.md",
        "tests/e2e/development-cargo/qualification.json",
        "bundles/tv-tracker-authoring/.ai/manifest.source.yaml",
        "bundles/tv-tracker-authoring/.ai/tools/tv-tracker-authoring/author-context-doc.yaml",
    ] {
        assert!(!matcher.is_ignored(source), "excluded {source}");
    }
}
