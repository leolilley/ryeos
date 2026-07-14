//! End-to-end proof: the engine pipeline executes a Python tool from a
//! project space and the daemon's dispatch layer captures stdout.
//!
//! Mirrors `ryeos_app::thread_lifecycle::spawn_item`'s shape
//! (resolve → verify → build_plan → execute_plan), skipping HTTP, the
//! thread DB, and the spawn/wait split. We call execute_plan directly
//! so we can inspect the captured stdout in the returned
//! `ExecutionCompletion.result` field.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::contracts::{
    EffectivePrincipal, EngineContext, ExecutionHints, LaunchMode, PlanContext, PlanNode,
    Principal, ProjectContext, ThreadTerminalStatus,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

fn sandbox_app_root() -> PathBuf {
    let root = tempfile::tempdir().unwrap().keep();
    let node = root.join(".ai/node");
    fs::create_dir_all(&node).unwrap();
    fs::write(node.join("sandbox.yaml"), "version: 1\nmode: enforce\nbackend:\n  kind: bubblewrap\n  executable: /usr/bin/bwrap\nfilesystem:\n  readable: [\"{verified_code}\"]\n  writable: [\"{project}\"]\nnetwork:\n  mode: isolated\nenvironment:\n  allow: [\"*\"]\nlimits:\n  open_files: 128\n  verified_artifact_file_bytes: 67108864\n  verified_artifact_total_bytes: 268435456\n  verified_artifact_files: 4096\n").unwrap();
    root
}

fn sandbox_context() -> (PathBuf, Arc<ryeos_engine::sandbox::SandboxRuntime>) {
    let app_root = sandbox_app_root();
    let sandbox = Arc::new(
        ryeos_engine::sandbox::SandboxRuntime::load(&app_root)
            .expect("load enforced sandbox fixture"),
    );
    (app_root, sandbox)
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/ directory")
        .to_path_buf()
}

fn synth_project_with_hello() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project_dir = std::env::temp_dir().join(format!(
        "ryeos_hello_world_test_{}_{}",
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
    // The ryeos-tool header below matches the kind schema's `metadata.rules`:
    //   executor_id → routes to the python script runtime, which itself
    //                 targets `@subprocess` (alias).
    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: hello
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   description: "Hello world demo"

import sys
print("hello world")
sys.exit(0)
"#;
    let tool_dir = tools_dir.join("hello");
    fs::create_dir_all(&tool_dir).unwrap();
    fs::write(tool_dir.join("hello.py"), body).unwrap();
    project_dir
}

fn synth_project_with_bundle_local_imports() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project_dir = std::env::temp_dir().join(format!(
        "ryeos_python_import_test_{}_{}",
        std::process::id(),
        nanos
    ));
    let bundle_root = project_dir.join(".ai").join("tools").join("example");
    fs::create_dir_all(bundle_root.join("lib")).unwrap();
    fs::create_dir_all(bundle_root.join("campaign")).unwrap();
    fs::create_dir_all(bundle_root.join("system")).unwrap();
    fs::write(
        project_dir.join("json.py"),
        "raise RuntimeError('project-root json.py should not be importable')\n",
    )
    .unwrap();

    fs::write(
        bundle_root.join("lib").join("util.py"),
        "def message():\n    return 'ok'\n",
    )
    .unwrap();
    fs::write(
        bundle_root.join("campaign").join("_lifecycle.py"),
        "def suffix():\n    return 'campaign'\n",
    )
    .unwrap();

    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: example/system
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   description: "Bundle-local import health check"

import json
from lib.util import message
from campaign._lifecycle import suffix

print(json.dumps({"message": message(), "suffix": suffix()}))
"#;
    fs::write(bundle_root.join("system").join("health.py"), body).unwrap();
    project_dir
}

fn synth_project_with_function_bundle_local_imports() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project_dir = std::env::temp_dir().join(format!(
        "ryeos_python_function_import_test_{}_{}",
        std::process::id(),
        nanos
    ));
    let bundle_root = project_dir.join(".ai").join("tools").join("example");
    fs::create_dir_all(bundle_root.join("lib")).unwrap();
    fs::create_dir_all(bundle_root.join("function")).unwrap();
    fs::write(
        project_dir.join("json.py"),
        "raise RuntimeError('project-root json.py should not be importable')\n",
    )
    .unwrap();

    fs::write(
        bundle_root.join("lib").join("util.py"),
        "def message():\n    return 'ok'\n",
    )
    .unwrap();

    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: example/function
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   description: "Bundle-local import function health check"

from lib.util import message

def execute(params, project_path):
    return {"message": message(), "project_path_seen": bool(project_path)}
"#;
    fs::write(bundle_root.join("function").join("health.py"), body).unwrap();
    project_dir
}

fn build_engine_against_bundle() -> Engine {
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load fixture trust store");

    let bundle_root = workspace_root().join("bundles/core");
    let kinds_dir = bundle_root.join(".ai/node/engine/kinds");
    let kinds =
        KindRegistry::load_base(&[kinds_dir], &trust_store).expect("live bundle kinds load");

    let (parser_tools, _dups) =
        ParserRegistry::load_base(std::slice::from_ref(&bundle_root), &trust_store, &kinds)
            .expect("live bundle parser tools load");
    let native_handlers = ryeos_engine::test_support::load_live_handler_registry();
    let parser_dispatcher =
        ParserDispatcher::new(parser_tools, std::sync::Arc::clone(&native_handlers));

    let composers = ComposerRegistry::from_kinds(&kinds, &native_handlers)
        .expect("composer registry derives from live bundle kinds");

    Engine::new(kinds, parser_dispatcher, vec![bundle_root])
        .with_trust_store(trust_store.clone())
        .with_node_trust_store(trust_store)
        .with_composers(composers)
}

#[test]
fn daemon_executes_python_hello_world_end_to_end() {
    ryeos_tracing::test::prime_callsites();
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

    let item = CanonicalRef::parse("tool:hello/hello").expect("canonical ref parses");

    // Mirror spawn_item: resolve → verify → build_plan → execute_plan
    let resolved = engine
        .resolve(&plan_ctx, &item)
        .expect("resolve hello.py from project space");

    // Sanity: the kind schema must have extracted executor_id from header metadata.
    assert_eq!(
        resolved.metadata.executor_id.as_deref(),
        Some("tool:ryeos/core/runtimes/python/script"),
        "extraction rules failed to pull executor_id from hello.py"
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

    // The chain MUST traverse: hello → tool:ryeos/core/runtimes/python/script
    //                         → @subprocess → tool:ryeos/core/subprocess/execute
    assert!(
        plan.executor_chain
            .iter()
            .any(|e| e == "tool:ryeos/core/runtimes/python/script"),
        "executor_chain missing python runtime: {:?}",
        plan.executor_chain
    );
    assert!(
        plan.executor_chain.iter().any(|e| e == "@subprocess"),
        "executor_chain missing @subprocess alias hop: {:?}",
        plan.executor_chain
    );

    let (app_root, sandbox) = sandbox_context();
    let engine_ctx = EngineContext {
        app_root,
        sandbox,
        sandbox_project_authority: ryeos_engine::sandbox::SandboxProjectAuthority::External,
        sandbox_state_root: None,
        sandbox_checkpoint_dir: None,
        sandbox_bundle_roots: Vec::new(),
        sandbox_operator_trusted_keys_dir: None,
        sandbox_verified_code: vec![ryeos_engine::sandbox::SandboxVerifiedCode {
            source_path: verified.resolved.source_path.clone(),
            content_hash: verified.resolved.content_hash.clone(),
        }],
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
        project_context: ProjectContext::LocalPath {
            path: project_dir.clone(),
        },
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

#[test]
fn python_script_runtime_supports_bundle_local_imports_without_pythonpath() {
    ryeos_tracing::test::prime_callsites();
    let engine = build_engine_against_bundle();
    let project_dir = synth_project_with_bundle_local_imports();

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

    let item = CanonicalRef::parse("tool:example/system/health").expect("canonical ref parses");
    let resolved = engine
        .resolve(&plan_ctx, &item)
        .expect("resolve direct Python tool from project space");
    assert_eq!(
        resolved.metadata.executor_id.as_deref(),
        Some("tool:ryeos/core/runtimes/python/script"),
        "extraction rules failed to pull executor_id from health.py"
    );

    let verified = engine
        .verify(&plan_ctx, resolved)
        .expect("verify health.py (unsigned is allowed)");
    let plan = engine
        .build_plan(
            &plan_ctx,
            &verified,
            &serde_json::Value::Null,
            &plan_ctx.execution_hints,
        )
        .expect("build_plan walks executor chain to subprocess terminal");

    let dispatch = plan
        .nodes
        .iter()
        .find_map(|node| match node {
            PlanNode::DispatchSubprocess { spec, .. } => Some(spec),
            _ => None,
        })
        .expect("plan should have a DispatchSubprocess node");
    assert!(
        !dispatch.env.contains_key("PYTHONPATH"),
        "python runtime must not emit PYTHONPATH env mutation"
    );
    assert!(
        !dispatch.env_sources.contains_key("PYTHONPATH"),
        "python runtime must not tag PYTHONPATH as RuntimePathMutation"
    );

    let (app_root, sandbox) = sandbox_context();
    let engine_ctx = EngineContext {
        app_root,
        sandbox,
        sandbox_project_authority: ryeos_engine::sandbox::SandboxProjectAuthority::External,
        sandbox_state_root: None,
        sandbox_checkpoint_dir: None,
        sandbox_bundle_roots: Vec::new(),
        sandbox_operator_trusted_keys_dir: None,
        sandbox_verified_code: vec![ryeos_engine::sandbox::SandboxVerifiedCode {
            source_path: verified.resolved.source_path.clone(),
            content_hash: verified.resolved.content_hash.clone(),
        }],
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
        project_context: ProjectContext::LocalPath {
            path: project_dir.clone(),
        },
        launch_mode: LaunchMode::Inline,
    };

    let completion = engine
        .execute_plan(&engine_ctx, plan)
        .expect("dispatch.execute_plan runs the subprocess");

    let _ = fs::remove_dir_all(&project_dir);

    assert_eq!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "subprocess did not complete cleanly: {completion:?}"
    );
    let result = completion.result.expect("captured stdout in result");
    assert_eq!(result["message"], "ok");
    assert_eq!(result["suffix"], "campaign");
}

#[test]
fn python_function_runtime_supports_bundle_local_imports_without_pythonpath() {
    ryeos_tracing::test::prime_callsites();
    let engine = build_engine_against_bundle();
    let project_dir = synth_project_with_function_bundle_local_imports();

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

    let item = CanonicalRef::parse("tool:example/function/health").expect("canonical ref parses");
    let resolved = engine
        .resolve(&plan_ctx, &item)
        .expect("resolve direct Python function tool from project space");
    assert_eq!(
        resolved.metadata.executor_id.as_deref(),
        Some("tool:ryeos/core/runtimes/python/function"),
        "extraction rules failed to pull executor_id from health.py"
    );

    let verified = engine
        .verify(&plan_ctx, resolved)
        .expect("verify health.py (unsigned is allowed)");
    let plan = engine
        .build_plan(
            &plan_ctx,
            &verified,
            &serde_json::Value::Null,
            &plan_ctx.execution_hints,
        )
        .expect("build_plan walks executor chain to subprocess terminal");

    let dispatch = plan
        .nodes
        .iter()
        .find_map(|node| match node {
            PlanNode::DispatchSubprocess { spec, .. } => Some(spec),
            _ => None,
        })
        .expect("plan should have a DispatchSubprocess node");
    assert!(
        !dispatch.env.contains_key("PYTHONPATH"),
        "python runtime must not emit PYTHONPATH env mutation"
    );
    assert!(
        !dispatch.env_sources.contains_key("PYTHONPATH"),
        "python runtime must not tag PYTHONPATH as RuntimePathMutation"
    );

    let (app_root, sandbox) = sandbox_context();
    let engine_ctx = EngineContext {
        app_root,
        sandbox,
        sandbox_project_authority: ryeos_engine::sandbox::SandboxProjectAuthority::External,
        sandbox_state_root: None,
        sandbox_checkpoint_dir: None,
        sandbox_bundle_roots: Vec::new(),
        sandbox_operator_trusted_keys_dir: None,
        sandbox_verified_code: vec![ryeos_engine::sandbox::SandboxVerifiedCode {
            source_path: verified.resolved.source_path.clone(),
            content_hash: verified.resolved.content_hash.clone(),
        }],
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
        project_context: ProjectContext::LocalPath {
            path: project_dir.clone(),
        },
        launch_mode: LaunchMode::Inline,
    };

    let completion = engine
        .execute_plan(&engine_ctx, plan)
        .expect("dispatch.execute_plan runs the subprocess");

    let _ = fs::remove_dir_all(&project_dir);

    assert_eq!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "subprocess did not complete cleanly: {completion:?}"
    );
    let result = completion.result.expect("captured stdout in result");
    assert_eq!(result["message"], "ok");
    assert_eq!(result["project_path_seen"], true);
}

/// Trace-capture test: the engine's resolve → verify → build_plan
/// path produces a structured `engine:*` span tree, and the runtime
/// handler pipeline running inside `build_plan` emits per-handler
/// spans (`engine:env_config`, `engine:config_resolve`, etc.) under it.
///
/// Sibling test [`daemon_executes_python_hello_world_end_to_end`]
/// exercises the same engine code paths without a capture subscriber;
/// without [`prime_callsites`] this test would flake when run in
/// parallel because callsite interest would cache as `Interest::never`
/// before the per-thread capture subscriber gets installed.
#[test]
fn engine_pipeline_emits_resolve_verify_build_plan_span_tree() {
    ryeos_tracing::test::prime_callsites();
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
    let item = CanonicalRef::parse("tool:hello/hello").expect("canonical ref parses");

    let (_, spans) = ryeos_tracing::test::capture_traces(|| {
        let resolved = engine.resolve(&plan_ctx, &item).expect("resolve");
        let verified = engine.verify(&plan_ctx, resolved).expect("verify");
        let _plan = engine
            .build_plan(
                &plan_ctx,
                &verified,
                &serde_json::Value::Null,
                &plan_ctx.execution_hints,
            )
            .expect("build_plan");
    });

    let _ = fs::remove_dir_all(&project_dir);

    fn collect_names(s: &ryeos_tracing::test::RecordedSpan, out: &mut Vec<String>) {
        out.push(s.name.clone());
        for c in &s.children {
            collect_names(c, out);
        }
    }
    let mut names: Vec<String> = Vec::new();
    for s in &spans {
        collect_names(s, &mut names);
    }

    let resolve_span = ryeos_tracing::test::find_span(&spans, "engine:resolve_ref")
        .unwrap_or_else(|| panic!("expected engine:resolve_ref in {:?}", names));
    assert_eq!(
        resolve_span.field("ref"),
        Some("tool:hello/hello"),
        "engine:resolve_ref should carry the original ref field"
    );

    let verify_span = ryeos_tracing::test::find_span(&spans, "engine:verify_item")
        .unwrap_or_else(|| panic!("expected engine:verify_item in {:?}", names));
    assert!(
        verify_span.field("canonical_ref").is_some(),
        "engine:verify_item should carry canonical_ref"
    );

    let build_plan_span = ryeos_tracing::test::find_span(&spans, "engine:build_plan")
        .unwrap_or_else(|| panic!("expected engine:build_plan in {:?}", names));
    assert!(
        build_plan_span.field("canonical_ref").is_some(),
        "engine:build_plan should carry canonical_ref"
    );

    // The python script chain declares config_resolve + env_config +
    // verify_deps + config (runtime_config). At least one per-handler
    // span must appear as a descendant of build_plan.
    let handler_present = ryeos_tracing::test::find_span(&spans, "engine:env_config")
        .or_else(|| ryeos_tracing::test::find_span(&spans, "engine:config_resolve"))
        .or_else(|| ryeos_tracing::test::find_span(&spans, "engine:runtime_config"))
        .or_else(|| ryeos_tracing::test::find_span(&spans, "engine:verify_deps"))
        .is_some();
    assert!(
        handler_present,
        "expected at least one engine:* handler span; got {:?}",
        names
    );
}
