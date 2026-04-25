//! Bundle parity guard: the canonical `ryeos-bundles/core` tree must
//! always load cleanly into the engine the daemon would boot with.
//!
//! If anyone changes a kind schema YAML — adds an unknown step, breaks
//! the tagged-enum resolution shape, drops a required field, fails to
//! re-sign after editing — this test fails before the daemon ever sees
//! the bundle. That's the whole job: a minimal, hermetic, bundle-side
//! drift detector that runs in CI without network or absolute paths.
//!
//! The trusted-signers fixture under `tests/fixtures/trusted_signers/`
//! is the bundle's own signing key — committed alongside the bundle so
//! the test runs from any checkout.

use std::fs;
use std::path::PathBuf;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::{ComposerRegistry, NativeComposerHandlerRegistry};
use ryeos_engine::item_resolution::ResolutionRoots;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{
    NativeParserHandlerRegistry, ParserDispatcher, ParserRegistry,
};
use ryeos_engine::resolution::run_resolution_pipeline;
use ryeos_engine::trust::TrustStore;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir().parent().expect("ryeosd has a parent dir").to_path_buf()
}

#[test]
fn live_bundle_kind_registry_loads_with_pinned_signer() {
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

    let kinds_dir = workspace_root().join("ryeos-bundles/core/.ai/config/engine/kinds");
    assert!(
        kinds_dir.is_dir(),
        "live bundle kinds dir missing at {}",
        kinds_dir.display()
    );

    let registry = KindRegistry::load_base(&[kinds_dir.clone()], &trust_store)
        .unwrap_or_else(|e| {
            panic!(
                "live bundle kind registry failed to load from {}: {e}",
                kinds_dir.display()
            )
        });

    // Every kind shipped in the live bundle must be present after load.
    // If any of these go missing or get renamed, the daemon won't boot —
    // catch it here, not in the daemon launch path.
    for required in ["directive", "graph", "knowledge", "parser", "tool"] {
        assert!(
            registry.contains(required),
            "live bundle is missing required kind `{required}`; loaded kinds = {:?}",
            registry.kinds().collect::<Vec<_>>()
        );
    }

    // Every executable kind must declare the resolution pipeline shape
    // the daemon expects (tagged-enum step decls). Loading already
    // verifies they parse; assertions below make the contract explicit
    // so a bundle edit that drops or renames the extends step fails
    // here, not at first directive launch.
    let directive = registry.get("directive").expect("directive kind present");
    assert!(
        directive.is_executable(),
        "directive kind must be executable in the live bundle"
    );

    let directive_exec = directive
        .execution
        .as_ref()
        .expect("directive kind has execution block");

    // The directive kind must run an extends-chain step — the daemon
    // resolves directives via this step, runtimes consume the
    // pre-resolved chain. Drop or rename it and the runtime starves.
    let has_extends = directive_exec.resolution.iter().any(|d| {
        matches!(
            d,
            ryeos_engine::resolution::ResolutionStepDecl::ResolveExtendsChain { .. }
        )
    });
    assert!(
        has_extends,
        "directive kind must declare resolve_extends_chain in execution.resolution; \
         got {:?}",
        directive_exec.resolution
    );
}

/// Build a `ParserDispatcher` from the live bundle's parser tool
/// descriptors so dispatch goes through the same descriptors the
/// daemon would load at boot.
fn live_parser_dispatcher(trust_store: &TrustStore, kinds: &KindRegistry) -> ParserDispatcher {
    let bundle_root = workspace_root().join("ryeos-bundles/core");
    let (parsers, _dups) = ParserRegistry::load_base(&[bundle_root], trust_store, kinds)
        .expect("live bundle parser tools load");
    ParserDispatcher::new(parsers, NativeParserHandlerRegistry::with_builtins())
}

fn synth_project_with_directive(name: &str, body: &str) -> PathBuf {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project_dir = std::env::temp_dir().join(format!(
        "rye_bundle_pipeline_test_{}_{}_{}",
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
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    let trust_store =
        TrustStore::load_from_dir(&trusted_dir).expect("load fixture trust store");

    let kinds_dir = workspace_root().join("ryeos-bundles/core/.ai/config/engine/kinds");
    let kinds = KindRegistry::load_base(&[kinds_dir], &trust_store)
        .expect("live bundle kinds load");
    let parsers = live_parser_dispatcher(&trust_store, &kinds);
    let composers = ComposerRegistry::from_kinds(&kinds, &NativeComposerHandlerRegistry::with_builtins())
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
    let body = "---\nname: test/sample\n---\n\nHello from Form A.\n";
    let output = run_pipeline_against_bundle(body);
    let composed_body = output
        .composed
        .derived_string("body")
        .expect("ExtendsChainComposer must populate `body` derived field");
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
    let body = "```yaml\nname: test/sample\n```\n\nHello from Form B.\n";
    let output = run_pipeline_against_bundle(body);
    let composed_body = output
        .composed
        .derived_string("body")
        .expect("ExtendsChainComposer must populate `body` derived field");
    assert!(
        composed_body.contains("Hello from Form B"),
        "composed body lost: {:?}",
        composed_body
    );
}
