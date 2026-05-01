//! End-to-end: a tool that declares `native_async` propagates the
//! cancellation policy through the engine pipeline into the
//! daemon's `RuntimeLaunchMetadata`, which is what
//! `drain_running_threads` consults to decide SIGTERM vs SIGKILL.
//!
//! Sending real signals to a real subprocess belongs in an OS-level
//! integration suite. This test pins the wire-format integration:
//!
//!   YAML `native_async: true` (bool shorthand)
//!     → engine `NativeAsyncSpec { cancellation_mode: Graceful{3} }`
//!     → `RuntimeLaunchMetadata::cancellation_mode = Some(Graceful{3})`
//!     → daemon's `resolve_shutdown_action` returns
//!       `ShutdownAction::Graceful(3s)`
//!
//! Plus the rich form (`graceful` / `hard`) and the explicit
//! `false`-rejection from the engine handler.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::contracts::{
    CancellationMode, EffectivePrincipal, ExecutionHints, PlanContext, PlanNode, Principal,
    ProjectContext,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

use ryeosd::launch_metadata::RuntimeLaunchMetadata;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir().parent().expect("ryeosd parent").to_path_buf()
}

fn unique_project_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rye_native_async_e2e_{}_{}_{}",
        label,
        std::process::id(),
        nanos
    ))
}

fn synth_project_with_async_tool(native_async_yaml: &str) -> PathBuf {
    let project_dir = unique_project_dir(
        native_async_yaml
            .replace([':', ' ', '\n', '"', '{', '}'], "_")
            .trim_matches('_'),
    );
    let tools_dir = project_dir.join(".ai").join("tools");
    let runtime_dir = tools_dir.join("local_async_runtime");
    fs::create_dir_all(&runtime_dir).unwrap();

    // Synth a wrapper runtime that targets @subprocess and DECLARES
    // the `native_async` block under test. This goes into the chain
    // (runtime YAMLs are chain elements), so the FirstWins handler
    // claims it and propagates into SubprocessSpec.execution.native_async.
    let runtime_body = format!(
        r#"version: "1.0.0"
executor_id: "@subprocess"
category: local_async_runtime
description: "test runtime with native_async block under test"

native_async: {native_async_yaml}

config:
  command: "/bin/true"
"#
    );
    fs::write(runtime_dir.join("runtime.yaml"), runtime_body).unwrap();

    // The user-facing tool just routes to the custom runtime above.
    let tool_body = r#"version: "1.0.0"
executor_id: "tool:local_async_runtime/runtime"
category: ""
description: "native_async demo"
"#;
    fs::write(tools_dir.join("async_demo.yaml"), tool_body).unwrap();
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
        ParserRegistry::load_base(std::slice::from_ref(&bundle_root), &trust_store, &kinds)
            .expect("parser tools load");
    let native_handlers = ryeos_engine::test_support::load_live_handler_registry();
    let parser_dispatcher =
        ParserDispatcher::new(parser_tools, std::sync::Arc::clone(&native_handlers));

    let composers =
        ComposerRegistry::from_kinds(&kinds, &native_handlers).expect("composer registry");

    Engine::new(kinds, parser_dispatcher, None, vec![bundle_root])
        .with_trust_store(trust_store)
        .with_composers(composers)
}

fn plan_ctx(project_dir: &Path) -> PlanContext {
    PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test".into(),
            scopes: vec!["execute".into()],
        }),
        project_context: ProjectContext::LocalPath {
            path: project_dir.to_path_buf(),
        },
        current_site_id: "site:test".into(),
        origin_site_id: "site:test".into(),
        execution_hints: ExecutionHints::default(),
        validate_only: false,
    }
}

/// Run resolve→verify→build_plan and return the first
/// DispatchSubprocess node's `cancellation_mode`, if any.
fn build_and_extract_cancellation(
    native_async_yaml: &str,
) -> Result<Option<CancellationMode>, String> {
    let engine = build_engine_against_bundle();
    let project_dir = synth_project_with_async_tool(native_async_yaml);
    let ctx = plan_ctx(&project_dir);

    let item = CanonicalRef::parse("tool:async_demo").map_err(|e| e.to_string())?;
    let resolved = engine.resolve(&ctx, &item).map_err(|e| e.to_string())?;
    let verified = engine.verify(&ctx, resolved).map_err(|e| e.to_string())?;
    let plan = engine
        .build_plan(&ctx, &verified, &serde_json::Value::Null, &ctx.execution_hints)
        .map_err(|e| e.to_string())?;

    let _ = fs::remove_dir_all(&project_dir);

    let spec = plan
        .nodes
        .iter()
        .find_map(|n| match n {
            PlanNode::DispatchSubprocess { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .ok_or_else(|| "no DispatchSubprocess node in plan".to_string())?;

    Ok(spec.execution.native_async.as_ref().map(|a| a.cancellation_mode))
}

#[test]
fn bool_true_shorthand_yields_graceful_default() {
    // Bool shorthand `native_async: true` resolves to the handler's
    // graceful default. The exact default value is owned by the
    // engine — what we check here is the SHAPE (Graceful, not Hard).
    let mode = build_and_extract_cancellation("true").expect("plan builds");
    match mode {
        Some(CancellationMode::Graceful { grace_secs }) => {
            assert!(
                grace_secs > 0,
                "default grace_secs should be positive, got {grace_secs}"
            );
        }
        other => panic!("expected Graceful{{_}}, got {other:?}"),
    }
}

#[test]
fn rich_hard_form_yields_hard_cancellation() {
    let mode = build_and_extract_cancellation(r#"{"cancel_mode": "hard"}"#)
        .expect("plan builds");
    assert_eq!(mode, Some(CancellationMode::Hard));
}

#[test]
fn rich_graceful_form_with_explicit_grace_secs_propagates() {
    let mode = build_and_extract_cancellation(
        r#"{"cancel_mode": "graceful", "graceful_shutdown_secs": 17}"#,
    )
    .expect("plan builds");
    assert_eq!(mode, Some(CancellationMode::Graceful { grace_secs: 17 }));
}

#[test]
fn explicit_false_is_rejected_loudly() {
    // `native_async: false` is a configuration mistake — if you don't
    // want native_async semantics, omit the block. The handler must
    // reject this rather than silently falling back to defaults.
    let err = build_and_extract_cancellation("false").expect_err("must reject false");
    let msg = err.to_lowercase();
    assert!(
        msg.contains("native_async") || msg.contains("false"),
        "expected loud rejection of native_async: false, got: {err}"
    );
}

#[test]
fn launch_metadata_from_spec_carries_cancellation_mode() {
    // Round-trip from the engine SubprocessSpec into the daemon's
    // RuntimeLaunchMetadata, which is what
    // `resolve_shutdown_action` consults at shutdown to decide
    // between SIGKILL-only and SIGTERM-then-SIGKILL.
    use ryeos_engine::contracts::{ExecutionDecorations, NativeAsyncSpec, PlanSubprocessSpec};
    use std::collections::HashMap;

    let spec = PlanSubprocessSpec {
        cmd: "/bin/true".into(),
        args: vec![],
        cwd: None,
        env: HashMap::new(),
        stdin_data: None,
        timeout_secs: 60,
        execution: ExecutionDecorations {
            native_async: Some(NativeAsyncSpec {
                cancellation_mode: CancellationMode::Graceful { grace_secs: 9 },
            }),
            ..Default::default()
        },
    };
    let metadata = RuntimeLaunchMetadata::from_spec(&spec);
    assert_eq!(
        metadata.cancellation_mode,
        Some(CancellationMode::Graceful { grace_secs: 9 })
    );
}
