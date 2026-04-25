//! End-to-end proof: the engine pipeline executes a Python tool from a
//! project space and the daemon's dispatch layer captures stdout.
//!
//! Mirrors `ryeosd::services::thread_lifecycle::spawn_item`'s shape
//! (resolve → verify → build_plan → execute_plan), skipping HTTP, the
//! thread DB, and the spawn/wait split. We call execute_plan directly
//! so we can inspect the captured stdout in the returned
//! `ExecutionCompletion.result` field.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::{ComposerRegistry, NativeComposerHandlerRegistry};
use ryeos_engine::contracts::{
    EffectivePrincipal, EngineContext, ExecutionHints, LaunchMode, PlanContext, Principal,
    ProjectContext, ThreadTerminalStatus,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{NativeParserHandlerRegistry, ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir().parent().expect("ryeosd has a parent dir").to_path_buf()
}

fn synth_project_with_hello() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project_dir = std::env::temp_dir().join(format!(
        "rye_hello_world_test_{}_{}",
        std::process::id(),
        nanos
    ));
    let tools_dir = project_dir.join(".ai").join("tools");
    fs::create_dir_all(&tools_dir).unwrap();

    // Tool kind schema's .py format declares `after_shebang: true` for
    // its signature envelope — meaning the signature line lives AFTER
    // the shebang. We don't sign at all here (Unsigned trust class is
    // accepted by the engine for items the chain doesn't gate on), so
    // the shebang is the very first line.
    //
    // The dunders below match the kind schema's `metadata.rules`:
    //   __executor_id__ → routes to the python script runtime, which
    //                     itself targets `@subprocess` (alias).
    let body = r#"#!/usr/bin/env python3
__version__ = "1.0.0"
__executor_id__ = "tool:rye/core/runtimes/python/script"
__category__ = "test/demo"
__tool_description__ = "Hello world demo"
__tool_type__ = "python"

import sys
print("hello world")
sys.exit(0)
"#;
    fs::write(tools_dir.join("hello.py"), body).unwrap();
    project_dir
}

fn build_engine_against_bundle() -> Engine {
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    let trust_store =
        TrustStore::load_from_dir(&trusted_dir).expect("load fixture trust store");

    let bundle_root = workspace_root().join("ryeos-bundles/core");
    let kinds_dir = bundle_root.join(".ai/config/engine/kinds");
    let kinds = KindRegistry::load_base(&[kinds_dir], &trust_store)
        .expect("live bundle kinds load");

    let (parser_tools, _dups) =
        ParserRegistry::load_base(&[bundle_root.clone()], &trust_store, &kinds)
            .expect("live bundle parser tools load");
    let native_handlers = NativeParserHandlerRegistry::with_builtins();
    let parser_dispatcher = ParserDispatcher::new(parser_tools, native_handlers);

    let native_composers = NativeComposerHandlerRegistry::with_builtins();
    let composers = ComposerRegistry::from_kinds(&kinds, &native_composers)
        .expect("composer registry derives from live bundle kinds");

    Engine::new(kinds, parser_dispatcher, None, vec![bundle_root])
        .with_trust_store(trust_store)
        .with_composers(composers)
}

#[test]
fn daemon_executes_python_hello_world_end_to_end() {
    let engine = build_engine_against_bundle();
    let project_dir = synth_project_with_hello();

    let plan_ctx = PlanContext {
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
    };

    let item = CanonicalRef::parse("tool:hello").expect("canonical ref parses");

    // Mirror spawn_item: resolve → verify → build_plan → execute_plan
    let resolved = engine
        .resolve(&plan_ctx, &item)
        .expect("resolve hello.py from project space");

    // Sanity: the kind schema must have extracted the executor_id dunder.
    assert_eq!(
        resolved.metadata.executor_id.as_deref(),
        Some("tool:rye/core/runtimes/python/script"),
        "extraction rules failed to pull __executor_id__ from hello.py"
    );

    let verified = engine
        .verify(&plan_ctx, resolved)
        .expect("verify hello.py (unsigned is allowed)");

    let plan = engine
        .build_plan(
            &plan_ctx,
            &verified,
            &serde_json::Value::Null,
            &plan_ctx.execution_hints,
        )
        .expect("build_plan walks executor chain to subprocess terminal");

    // The chain MUST traverse: hello → tool:rye/core/runtimes/python/script
    //                         → @subprocess → tool:rye/core/subprocess/execute
    assert!(
        plan.executor_chain
            .iter()
            .any(|e| e == "tool:rye/core/runtimes/python/script"),
        "executor_chain missing python runtime: {:?}",
        plan.executor_chain
    );
    assert!(
        plan.executor_chain.iter().any(|e| e == "@subprocess"),
        "executor_chain missing @subprocess alias hop: {:?}",
        plan.executor_chain
    );

    let engine_ctx = EngineContext {
        thread_id: "thread:test".into(),
        chain_root_id: "chain:test".into(),
        current_site_id: "site:test".into(),
        origin_site_id: "site:test".into(),
        upstream_site_id: None,
        upstream_thread_id: None,
        continuation_from_id: None,
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test".into(),
            scopes: vec!["execute".into()],
        }),
        project_context: ProjectContext::LocalPath { path: project_dir.clone() },
        launch_mode: LaunchMode::Inline,
    };

    let completion = engine
        .execute_plan(&engine_ctx, plan)
        .expect("dispatch.execute_plan runs the subprocess");

    // Cleanup before assertions so a failed run still leaves /tmp clean.
    let _ = fs::remove_dir_all(&project_dir);

    assert_eq!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "subprocess did not complete cleanly: {completion:?}"
    );

    // dispatch::translate_result tries JSON-parse first, falls back to
    // String when stdout isn't JSON. "hello world\n" is not valid JSON
    // → result lands as a plain string.
    let result = completion.result.expect("captured stdout in result");
    let stdout_text = result
        .as_str()
        .expect("non-JSON stdout becomes a JSON string");
    assert!(
        stdout_text.contains("hello world"),
        "expected 'hello world' in captured stdout, got {stdout_text:?}"
    );
}
