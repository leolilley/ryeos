//! Bundle parity guard: the canonical `ryeos-bundles/` tree must
//! always load cleanly into the engine the daemon would boot with.
//!
//! If anyone changes a kind schema YAML — adds an unknown step, breaks
//! the tagged-enum resolution shape, drops a required field, fails to
//! re-sign after editing — this test fails before the daemon ever sees
//! the bundle. That's the whole job: a minimal, hermetic, bundle-side
//! drift detector that runs in CI without network or absolute paths.
//!
//! Also asserts correct bundle ownership: engine kinds live in core,
//! workflow kinds live in standard. If someone accidentally moves a
//! kind back to the wrong bundle, this catches it.
//!
//! The trusted-signers fixture under `tests/fixtures/trusted_signers/`
//! is the bundle's own signing key — committed alongside the bundle so
//! the test runs from any checkout.

use std::fs;
use std::path::PathBuf;

use ryeos_engine::canonical_ref::CanonicalRef;

use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::item_resolution::ResolutionRoots;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{
    ParserDispatcher, ParserRegistry,
};
use ryeos_engine::resolution::run_resolution_pipeline;
use ryeos_engine::trust::TrustStore;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir().parent().expect("ryeosd has a parent dir").to_path_buf()
}

fn core_kinds_dir() -> PathBuf {
    workspace_root().join("ryeos-bundles/core/.ai/node/engine/kinds")
}

fn standard_kinds_dir() -> PathBuf {
    workspace_root().join("ryeos-bundles/standard/.ai/node/engine/kinds")
}

fn fixture_trust_store() -> TrustStore {
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    assert!(
        trusted_dir.is_dir(),
        "trusted-signers fixture missing at {}",
        trusted_dir.display()
    );
    let trust_store = TrustStore::load_from_dir(&trusted_dir)
        .expect("load fixture trust store");
    assert!(
        !trust_store.is_empty(),
        "fixture trust store has no signers — fixture is empty"
    );
    trust_store
}

// ── Bundle ownership assertions ──────────────────────────────────────

#[test]
fn core_bundle_owns_engine_kinds_only() {
    let trust_store = fixture_trust_store();
    let kinds_dir = core_kinds_dir();
    assert!(kinds_dir.is_dir(), "core kinds dir missing at {}", kinds_dir.display());

    let registry = KindRegistry::load_base(&[kinds_dir], &trust_store)
        .expect("core kinds load");

    let kinds: Vec<&str> = registry.kinds().collect();
    for expected in ["config", "handler", "parser", "protocol", "service",
                     "node", "tool", "streaming_tool", "runtime"] {
        assert!(
            registry.contains(expected),
            "core must own `{expected}` kind; got: {:?}", kinds
        );
    }

    // Workflow kinds must NOT be in core
    for forbidden in ["directive", "graph", "knowledge"] {
        assert!(
            !registry.contains(forbidden),
            "core must NOT own `{forbidden}` kind — it belongs in standard; got: {:?}",
            kinds
        );
    }
}

#[test]
fn standard_bundle_owns_workflow_kinds() {
    let trust_store = fixture_trust_store();
    let kinds_dir = standard_kinds_dir();
    assert!(kinds_dir.is_dir(), "standard kinds dir missing at {}", kinds_dir.display());

    let registry = KindRegistry::load_base(&[kinds_dir], &trust_store)
        .expect("standard kinds load");

    for expected in ["directive", "graph", "knowledge"] {
        assert!(
            registry.contains(expected),
            "standard must own `{expected}` kind; got: {:?}",
            registry.kinds().collect::<Vec<_>>()
        );
    }
}

// ── Combined load and pipeline tests ─────────────────────────────────

#[test]
fn live_bundle_kind_registry_loads_with_pinned_signer() {
    let trust_store = fixture_trust_store();

    // Load kinds from BOTH bundles — that's what the engine does at boot.
    let kinds_dirs = vec![core_kinds_dir(), standard_kinds_dir()];
    for dir in &kinds_dirs {
        assert!(dir.is_dir(), "kinds dir missing at {}", dir.display());
    }

    let registry = KindRegistry::load_base(&kinds_dirs, &trust_store)
        .unwrap_or_else(|e| {
            panic!("live bundle kind registry failed to load: {e}");
        });

    // Every kind shipped in the live bundles must be present after load.
    for required in ["directive", "graph", "knowledge", "parser", "tool"] {
        assert!(
            registry.contains(required),
            "live bundles missing required kind `{required}`; loaded = {:?}",
            registry.kinds().collect::<Vec<_>>()
        );
    }

    // Directive kind must be executable with extends-chain resolution.
    let directive = registry.get("directive").expect("directive kind present");
    assert!(directive.is_executable(), "directive kind must be executable");

    let directive_exec = directive
        .execution
        .as_ref()
        .expect("directive kind has execution block");

    let has_extends = directive_exec.resolution.iter().any(|d| {
        matches!(
            d,
            ryeos_engine::resolution::ResolutionStepDecl::ResolveExtendsChain { .. }
        )
    });
    assert!(
        has_extends,
        "directive kind must declare resolve_extends_chain; got {:?}",
        directive_exec.resolution
    );
}

/// Build a `ParserDispatcher` from both live bundles' parser tool
/// descriptors so dispatch goes through the same descriptors the
/// daemon would load at boot.
fn live_parser_dispatcher(trust_store: &TrustStore, kinds: &KindRegistry) -> ParserDispatcher {
    let core_root = workspace_root().join("ryeos-bundles/core");
    let std_root = workspace_root().join("ryeos-bundles/standard");
    let (parsers, _dups) = ParserRegistry::load_base(&[core_root, std_root], trust_store, kinds)
        .expect("live bundle parser tools load");
    ParserDispatcher::new(
        parsers,
        ryeos_engine::test_support::load_live_handler_registry(),
    )
}

fn synth_project_with_directive(name: &str, body: &str) -> PathBuf {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project_dir = std::env::temp_dir().join(format!(
        "ryeos_bundle_pipeline_test_{}_{}_{}",
        std::process::id(),
        name,
        nanos
    ));
    let directives_dir = project_dir.join(".ai").join("directives").join("test");
    fs::create_dir_all(&directives_dir).unwrap();
    fs::write(directives_dir.join("sample.md"), body).unwrap();
    project_dir
}

fn run_pipeline_against_bundle(directive_body: &str) -> ryeos_engine::resolution::ResolutionOutput {
    let trust_store = fixture_trust_store();

    // Load kinds from BOTH bundles — directive kind lives in standard.
    let kinds_dirs = vec![core_kinds_dir(), standard_kinds_dir()];
    let kinds = KindRegistry::load_base(&kinds_dirs, &trust_store)
        .expect("live bundle kinds load");
    let parsers = live_parser_dispatcher(&trust_store, &kinds);
    let composers = ComposerRegistry::from_kinds(
        &kinds,
        &ryeos_engine::test_support::load_live_handler_registry(),
    )
    .expect("composer registry derives from live bundle kinds");

    let project_dir = synth_project_with_directive("inline", directive_body);
    let roots = ResolutionRoots::from_flat(Some(project_dir.join(".ai")), None, vec![]);
    let item = CanonicalRef::parse("directive:test/sample").unwrap();

    let out = run_resolution_pipeline(&item, &kinds, &parsers, &roots, &trust_store, &composers)
        .expect("pipeline must succeed for an unsigned project directive");

    let _ = fs::remove_dir_all(&project_dir);
    out
}

/// Bundle parity, with teeth: drive `run_resolution_pipeline` end-to-end
/// against the live bundle kind registry, parser tool descriptors, and
/// composer registry. Form A (YAML frontmatter `---`) variant.
#[test]
fn pipeline_runs_against_live_bundle_kinds_form_a() {
    let body = "---\ncategory: \"test\"\nname: sample\n---\n\nHello from Form A.\n";
    let output = run_pipeline_against_bundle(body);
    let composed_body = output
        .composed
        .derived_string("body")
        .expect("the extends-chain composer must populate `body` derived field");
    assert!(
        composed_body.contains("Hello from Form A"),
        "composed body lost: {:?}",
        composed_body
    );
    assert!(
        output.ancestors.is_empty(),
        "no extends declared, ancestor chain must be empty"
    );
}

/// Form B (fenced ```yaml block) variant — same contract, different
/// surface syntax. Ensures the markdown/directive parser tool accepts
/// both forms via the live bundle descriptor.
#[test]
fn pipeline_runs_against_live_bundle_kinds_form_b() {
    let body = "```yaml\ncategory: \"test\"\nname: sample\n```\n\nHello from Form B.\n";
    let output = run_pipeline_against_bundle(body);
    let composed_body = output
        .composed
        .derived_string("body")
        .expect("the extends-chain composer must populate `body` derived field");
    assert!(
        composed_body.contains("Hello from Form B"),
        "composed body lost: {:?}",
        composed_body
    );
}
