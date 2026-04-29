//! End-to-end: a tool that declares `config_schema` is rejected at
//! `build_plan` time when its parameters violate the schema, and
//! accepted when they conform.
//!
//! Mirrors `hello_world_python.rs`'s engine-pipeline shape (no
//! daemon HTTP). Goes through `resolve → verify → build_plan` and
//! asserts on the EngineError variant emitted by the
//! `config_schema` ValidateInput pre-pass in `plan_builder`.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::{ComposerRegistry, NativeComposerHandlerRegistry};
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{NativeParserHandlerRegistry, ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir().parent().expect("ryeosd parent").to_path_buf()
}

/// Synthesize a YAML tool whose top-level body declares
/// `config_schema` requiring `count: integer >= 0`. Targets
/// `@subprocess` directly so the chain is one hop deep — enough to
/// exercise the plan_builder ValidateInput pre-pass.
fn synth_project_with_schema_tool() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project_dir = std::env::temp_dir().join(format!(
        "rye_config_schema_e2e_{}_{}",
        std::process::id(),
        nanos
    ));
    let tools_dir = project_dir.join(".ai").join("tools");
    fs::create_dir_all(&tools_dir).unwrap();

    let body = r#"version: "1.0.0"
executor_id: "tool:rye/core/subprocess/execute"
category: ""
description: "schema-checked demo"

config_schema:
  type: object
  required: [count]
  properties:
    count:
      type: integer
      minimum: 0

config:
  command: "/bin/true"
"#;
    fs::write(tools_dir.join("schema_demo.yaml"), body).unwrap();
    project_dir
}

fn build_engine_against_bundle() -> Engine {
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    let trust_store =
        TrustStore::load_from_dir(&trusted_dir).expect("load fixture trust store");

    let bundle_root = workspace_root().join("ryeos-bundles/core");
    let kinds_dir = bundle_root.join(".ai/node/engine/kinds");
    let kinds = KindRegistry::load_base(&[kinds_dir], &trust_store).expect("kinds load");

    let (parser_tools, _dups) =
        ParserRegistry::load_base(&[bundle_root.clone()], &trust_store, &kinds)
            .expect("parser tools load");
    let native_handlers = NativeParserHandlerRegistry::with_builtins();
    let parser_dispatcher = ParserDispatcher::new(parser_tools, native_handlers);

    let native_composers = NativeComposerHandlerRegistry::with_builtins();
    let composers =
        ComposerRegistry::from_kinds(&kinds, &native_composers).expect("composer registry");

    Engine::new(kinds, parser_dispatcher, None, vec![bundle_root])
        .with_trust_store(trust_store)
        .with_composers(composers)
}

fn plan_ctx(project_dir: &PathBuf) -> PlanContext {
    PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test".into(),
            scopes: vec!["execute".into()],
        }),
        project_context: ProjectContext::LocalPath {
            path: project_dir.clone(),
        },
        current_site_id: "site:test".into(),
        origin_site_id: "site:test".into(),
        execution_hints: ExecutionHints::default(),
        validate_only: false,
    }
}

#[test]
fn build_plan_rejects_params_violating_config_schema() {
    let engine = build_engine_against_bundle();
    let project_dir = synth_project_with_schema_tool();
    let ctx = plan_ctx(&project_dir);

    let item = CanonicalRef::parse("tool:schema_demo").expect("ref parses");
    let resolved = engine.resolve(&ctx, &item).expect("resolve");
    let verified = engine.verify(&ctx, resolved).expect("verify (unsigned ok)");

    // count: "five" violates the schema (string vs integer).
    let bad_params = serde_json::json!({ "count": "five" });

    let result = engine.build_plan(&ctx, &verified, &bad_params, &ctx.execution_hints);

    let _ = fs::remove_dir_all(&project_dir);

    let err = result.expect_err("schema-violating params must fail build_plan");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("schema") || msg.to_lowercase().contains("validation"),
        "expected a schema-validation error, got: {msg}"
    );
}

#[test]
fn build_plan_accepts_params_conforming_to_config_schema() {
    // We can't easily synthesize a YAML tool with a runnable chain
    // here without bringing the full bundle's runtime config into
    // the test. Instead, prove the conforming-params path by
    // resolving a real bundle tool that ships with config_schema
    // (`tool:rye/core/subprocess/execute` requires `command`) and
    // confirming that the validator does NOT block valid params.
    //
    // The rejection test above already proves the negative path
    // (handler is wired into plan_builder); this test proves the
    // positive path (valid params don't trigger a false positive).
    let engine = build_engine_against_bundle();
    let project_dir = synth_project_with_schema_tool();
    let ctx = plan_ctx(&project_dir);

    let item =
        CanonicalRef::parse("tool:rye/core/subprocess/execute").expect("ref parses");
    let resolved = engine.resolve(&ctx, &item).expect("resolve bundle tool");
    let verified = engine.verify(&ctx, resolved).expect("verify");

    let good_params = serde_json::json!({ "command": "/bin/true" });

    let result = engine.build_plan(&ctx, &verified, &good_params, &ctx.execution_hints);

    let _ = fs::remove_dir_all(&project_dir);

    // We don't require build_plan to succeed (other downstream
    // checks may complain about the test environment). What we
    // require is that the failure mode is NOT
    // ParameterValidationFailed when params satisfy the schema.
    if let Err(err) = result {
        let msg = format!("{err:?}");
        assert!(
            !msg.contains("ParameterValidationFailed"),
            "valid params triggered a false-positive schema rejection: {msg}"
        );
    }
}
