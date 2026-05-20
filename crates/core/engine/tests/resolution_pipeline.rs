//! Integration tests for the resolution pipeline.
//!
//! Currently exercises the BFS regression for `resolve_references` —
//! the bug where a node visited via a long path before a short one
//! would have its outgoing edges silently dropped because of a
//! visited-set check that ignored depth.

use std::fs;
use std::path::{Path, PathBuf};

use lillux::crypto::SigningKey;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{ParserDescriptor, ParserDispatcher};
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::resolution::run_resolution_pipeline;
use ryeos_engine::test_support::load_live_handler_registry;
use ryeos_engine::trust::{compute_fingerprint, TrustStore, TrustedSigner};
use ryeos_engine::{contracts::ItemSpace, item_resolution::ResolutionRoots};

fn dispatcher_for_yaml_and_markdown_directive() -> ParserDispatcher {
    use serde_json::json;
    let mk = |handler: &str, parser_config: serde_json::Value| ParserDescriptor {
        version: "1.0.0".into(),
        category: None,
        description: None,
        handler: handler.into(),
        parser_api_version: 1,
        parser_config,
        output_schema: ryeos_engine::contracts::ValueShape::any_mapping(),
    };
    let entries = vec![
        (
            "parser:ryeos/core/yaml/yaml".to_string(),
            mk(
                "handler:ryeos/core/yaml-document",
                json!({ "require_mapping": true }),
            ),
        ),
        (
            "parser:ryeos/core/markdown/directive".to_string(),
            mk(
                "handler:ryeos/core/yaml-header-document",
                json!({
                    "require_header": true,
                    "body_field": "body",
                    "forms": [
                        { "kind": "frontmatter", "delimiter": "---" },
                        { "kind": "fenced_block", "language": "yaml" }
                    ]
                }),
            ),
        ),
    ];
    ryeos_engine::test_support::build_parser_dispatcher_from_roots(entries)
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn trust_store() -> TrustStore {
    let sk = signing_key();
    let vk = sk.verifying_key();
    let fp = compute_fingerprint(&vk);
    TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: fp,
        verifying_key: vk,
        label: None,
    }])
}

fn tempdir() -> PathBuf {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rye_resolution_test_{}_{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn sign_yaml(yaml: &str) -> String {
    // Inject the now-mandatory composed_value_contract for kind
    // schemas that don't exercise contract semantics.
    let is_kind_schema = yaml.contains("kind-schema") || yaml.contains("location:");
    let mut yaml_owned = yaml.to_string();
    if is_kind_schema {
        if !yaml_owned.contains("composed_value_contract") {
            yaml_owned.push_str(
                "composed_value_contract:\n  root_type: mapping\n  required: {}\n",
            );
        }
        if !yaml_owned.contains("composer:") {
            yaml_owned.push_str("composer: handler:ryeos/core/identity\n");
        }
    }
    lillux::signature::sign_content(&yaml_owned, &signing_key(), "#", None)
}

/// Sign a YAML body so it parses both as a signed item *and* as data —
/// the signature is a `#` comment line, the body below it is real YAML.
fn write_signed_yaml(path: &Path, yaml: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, sign_yaml(yaml)).unwrap();
}

const NODE_SCHEMA: &str = "\
location:
  directory: nodes
formats:
  - extensions: [\".yaml\"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: \"#\"
execution:
  thread_profile: node_run
  delegate:
    via: runtime_registry
  resolution:
    - step: resolve_references
      field: refs
      max_depth: 3
";

fn write_node_schema(kinds_dir: &Path) {
    let dir = kinds_dir.join("node");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("node.kind-schema.yaml"), sign_yaml(NODE_SCHEMA)).unwrap();
}

/// Regression for the BFS-on-references bug: when DFS reaches a node via
/// a long branch first (root→A→X→C at depth 3), a global visited-set
/// would lock C and skip its children, so root→B→C never expands C→D.
///
/// The fix tracks `best_depth_seen` per node and re-enters when a
/// shorter path arrives — the only way edges that were truncated by
/// `max_depth` last time can ever be discovered.
#[test]
fn references_bfs_does_not_drop_edges_on_long_path_first() {
    let project_dir = tempdir();
    let kinds_dir = tempdir();
    write_node_schema(&kinds_dir);

    let kinds = KindRegistry::load_base(&[kinds_dir], &trust_store()).unwrap();

    let nodes_dir = project_dir.join(".ai").join("nodes");

    // Root deliberately lists A *before* B so DFS climbs A→X→C (depth 3)
    // before it ever sees B→C (depth 2). Without the fix, C would be
    // marked visited at depth 3 and the recursion into B→C would skip
    // C's children entirely, dropping C→D.
    write_signed_yaml(
        &nodes_dir.join("root.yaml"),
        "refs:\n  - node:a\n  - node:b\n",
    );
    write_signed_yaml(&nodes_dir.join("a.yaml"), "refs:\n  - node:x\n");
    write_signed_yaml(&nodes_dir.join("x.yaml"), "refs:\n  - node:c\n");
    write_signed_yaml(&nodes_dir.join("b.yaml"), "refs:\n  - node:c\n");
    write_signed_yaml(&nodes_dir.join("c.yaml"), "refs:\n  - node:d\n");
    write_signed_yaml(&nodes_dir.join("d.yaml"), "refs: []\n");

    let roots = ResolutionRoots::from_flat(
        Some(project_dir.join(".ai")),
        None,
        vec![],
    );
    let _ = ItemSpace::Project;

    let parsers = dispatcher_for_yaml_and_markdown_directive();
    let trust = trust_store();
    let item = CanonicalRef::parse("node:root").unwrap();

    let handlers = load_live_handler_registry();
    let composers = ComposerRegistry::from_kinds(&kinds, &handlers)
        .expect("from_kinds must bind node kind");
    let output = run_resolution_pipeline(&item, &kinds, &parsers, &roots, &trust, &composers)
        .expect("resolution pipeline succeeded");

    let edges: Vec<(String, String)> = output
        .references_edges
        .iter()
        .map(|e| (e.from_ref.clone(), e.to_ref.clone()))
        .collect();

    let has = |from: &str, to: &str| {
        edges
            .iter()
            .any(|(f, t)| f == from && t == to)
    };

    assert!(has("node:root", "node:a"), "missing root→a in {edges:?}");
    assert!(has("node:root", "node:b"), "missing root→b in {edges:?}");
    assert!(has("node:a", "node:x"), "missing a→x in {edges:?}");
    assert!(has("node:x", "node:c"), "missing x→c in {edges:?}");
    assert!(has("node:b", "node:c"), "missing b→c in {edges:?}");
    // The regression target: c→d must be present despite C being first
    // visited via the long branch.
    assert!(
        has("node:c", "node:d"),
        "regression: c→d edge dropped because C was first visited via the longer A→X→C branch; edges = {edges:?}"
    );
}

const DIRECTIVE_SCHEMA: &str = "\
location:
  directory: directives
formats:
  - extensions: [\".md\"]
    parser: parser:ryeos/core/markdown/directive
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
execution:
  thread_profile: directive_run
  delegate:
    via: runtime_registry
  resolution: []
";

/// Regression: the bytes the parser sees and the bytes the envelope
/// binds and ships in `raw_content` MUST be byte-identical. Before the
/// fix, `load_item_at` stripped via the generic `strip_signature_lines`
/// helper, which removes any `# ryeos:signed:...` line regardless of
/// envelope. For a markdown directive (envelope `<!-- ... -->`), a
/// `# ryeos:signed:...` literal in the BODY is just text — the parser
/// keeps it, and the resolution payload must too.
#[test]
fn raw_content_uses_envelope_aware_strip_for_markdown() {
    let project_dir = tempdir();
    let kinds_dir = tempdir();
    let dir = kinds_dir.join("directive");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("directive.kind-schema.yaml"),
        sign_yaml(DIRECTIVE_SCHEMA),
    )
    .unwrap();

    let kinds = KindRegistry::load_base(&[kinds_dir], &trust_store()).unwrap();

    let directives_dir = project_dir.join(".ai").join("directives").join("test");
    fs::create_dir_all(&directives_dir).unwrap();
    // Unsigned markdown directive whose BODY contains a literal that
    // looks like a `#`-envelope signature line. The directive's own
    // envelope is `<!-- ... -->`, so this line is body text, not a
    // signature.
    let body = "---\n\
                name: test/sample\n\
                ---\n\
                \n\
                pre-marker\n\
                # ryeos:signed:fake-not-a-real-sig\n\
                post-marker\n";
    fs::write(directives_dir.join("sample.md"), body).unwrap();

    let roots = ResolutionRoots::from_flat(Some(project_dir.join(".ai")), None, vec![]);
    let parsers = dispatcher_for_yaml_and_markdown_directive();
    let trust = trust_store();
    let handlers = load_live_handler_registry();
    let composers = ComposerRegistry::from_kinds(&kinds, &handlers)
        .expect("from_kinds must bind directive kind");
    let item = CanonicalRef::parse("directive:test/sample").unwrap();

    let output = run_resolution_pipeline(&item, &kinds, &parsers, &roots, &trust, &composers)
        .expect("pipeline must succeed for unsigned markdown directive");

    assert!(
        output.root.raw_content.contains("# ryeos:signed:fake-not-a-real-sig"),
        "envelope-aware strip must NOT remove a `#`-prefixed line from a markdown body \
         (envelope is `<!-- ... -->`); raw_content = {:?}",
        output.root.raw_content
    );
    assert!(output.root.raw_content.contains("pre-marker"));
    assert!(output.root.raw_content.contains("post-marker"));
}

