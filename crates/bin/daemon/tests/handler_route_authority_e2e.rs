//! End-to-end regression: public `response.mode: handler` routes must be able
//! to run their fixed same-bundle handler tool under route/bundle authority,
//! even when the HTTP request is `auth: none` (anonymous, no scopes).
//!
//! The original blocker (Agent Kiwi Google OAuth callback) failed before the
//! handler tool ran:
//!
//!   dispatch failed: subprocess run failed for 'tool:agent-kiwi/oauth/callback':
//!   plan build failed: insufficient scope: required `execute`, available: []
//!
//! That error comes from the engine's `build_plan` execution-scope gate
//! (`ryeos_engine::scope::check_execution_scope`). The fix is in
//! `handler_mode.rs`: instead of running the fixed handler under the anonymous
//! caller's (empty) scopes, it mints a fixed route-handler authority whose only
//! scope is the exact capability for that one tool, derived with
//! `ryeos_runtime::authorizer::canonical_cap(kind, subject, "execute")`.
//!
//! The compile-time test in `handler_mode.rs`
//! (`compile_uses_fixed_route_handler_authority`) proves the handler *constructs*
//! that scope. What was untested is the other half of the contract: that the
//! scope the handler computes actually satisfies the engine's execution
//! authorization at runtime, while empty/anonymous scopes still do not.
//!
//! This test pins both halves against the real engine pipeline
//! (resolve → verify → build_plan → execute_plan), using the same
//! `canonical_cap` derivation the handler uses, so the two cannot silently
//! drift apart.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::contracts::{
    EffectivePrincipal, EngineContext, ExecutionHints, LaunchMode, PlanContext, Principal,
    ProjectContext, ThreadTerminalStatus,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

fn isolation_app_root() -> PathBuf {
    let root = tempfile::tempdir().unwrap().keep();
    let node = root.join(".ai/node");
    fs::create_dir_all(&node).unwrap();
    fs::write(node.join("isolation.yaml"), "version: 1\nmode: disabled\nbackend: null\nfilesystem:\n  readable: [\"{verified_code}\"]\n  writable: [\"{project}\"]\nnetwork:\n  mode: isolated\nenvironment:\n  allow: [\"*\"]\nlimits:\n  open_files: 128\n  stdout_bytes: 8388608\n  stderr_bytes: 8388608\n  verified_artifact_file_bytes: 67108864\n  verified_artifact_total_bytes: 268435456\n  verified_artifact_files: 4096\n").unwrap();
    root
}

/// Mirrors the real Agent Kiwi OAuth callback handler ref.
const HANDLER_REF: &str = "tool:agent-kiwi/oauth/callback";
/// Bundle-qualified subject of the handler ref (everything after `tool:`).
const HANDLER_SUBJECT: &str = "agent-kiwi/oauth/callback";

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

/// The exact scope `handler_mode.rs` mints for this fixed handler. We derive it
/// the same way the daemon does so this test and the handler share one source of
/// truth for the capability string.
fn route_handler_scope() -> String {
    ryeos_runtime::authorizer::canonical_cap("tool", HANDLER_SUBJECT, "execute")
}

/// Synthesize a project bundle whose fixed handler tool emits an HTTP response
/// envelope, exactly like a real `response.mode: handler` OAuth callback target.
fn synth_project_with_handler_tool() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project_dir = std::env::temp_dir().join(format!(
        "ryeos_handler_authority_e2e_{}_{}",
        std::process::id(),
        nanos
    ));

    // `tool:agent-kiwi/oauth/callback` resolves to
    // `.ai/tools/agent-kiwi/oauth/callback.py`.
    let tool_path = project_dir
        .join(".ai")
        .join("tools")
        .join("agent-kiwi")
        .join("oauth")
        .join("callback.py");
    fs::create_dir_all(tool_path.parent().unwrap()).unwrap();

    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: agent-kiwi/oauth
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   description: "OAuth callback handler that returns an HTTP response envelope"

import json
import sys

print(json.dumps({
    "response": {
        "status": 302,
        "headers": {"Location": "https://agentkiwi.nz/?google=connected"},
    }
}))
sys.exit(0)
"#;
    fs::write(&tool_path, body).unwrap();
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

fn plan_ctx(project_dir: &Path, scopes: Vec<String>) -> PlanContext {
    PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "route-handler:agent-kiwi:agent-kiwi/google-callback".into(),
            scopes,
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

/// Negative case: this reproduces the original blocker. An `auth: none` route
/// principal has no scopes, so the generic execution-scope gate in `build_plan`
/// must reject it. This proves the fix did NOT turn handler dispatch into a
/// public generic-execute hole — the engine still fails closed for anonymous
/// callers that are not granted a route-handler authority.
#[test]
fn anonymous_route_principal_is_denied_at_build_plan() {
    let engine = build_engine_against_bundle();
    let project_dir = synth_project_with_handler_tool();
    let ctx = plan_ctx(&project_dir, Vec::new());

    let item = CanonicalRef::parse(HANDLER_REF).expect("canonical ref parses");
    let resolved = engine.resolve(&ctx, &item).expect("resolve handler tool");
    let verified = engine.verify(&ctx, resolved).expect("verify handler tool");

    let result = engine.build_plan(
        &ctx,
        &verified,
        &serde_json::Value::Null,
        &ctx.execution_hints,
    );

    let _ = fs::remove_dir_all(&project_dir);

    let err = result.expect_err("anonymous (no-scope) principal must be denied at build_plan");
    let msg = err.to_string();
    assert!(
        msg.contains("insufficient scope"),
        "expected insufficient-scope denial, got: {msg}"
    );
}

/// Positive case: the fixed route-handler authority (whose only scope is the
/// exact capability for this one tool) must satisfy the engine's execution-scope
/// gate AND run the handler subprocess, returning its HTTP response envelope.
///
/// This is the end-to-end proof that the `handler_mode.rs` fix actually unblocks
/// public OAuth/webhook callbacks: the route-handler scope derived from
/// `canonical_cap` flows through resolve → verify → build_plan → execute_plan
/// without the `insufficient scope` failure.
#[test]
fn route_handler_fixed_scope_executes_handler_end_to_end() {
    ryeos_tracing::test::prime_callsites();
    let engine = build_engine_against_bundle();
    let project_dir = synth_project_with_handler_tool();

    let scope = route_handler_scope();
    // Guard the contract this test depends on: the handler's derived scope is a
    // capability-style execute scope, not the bare `execute` or `*` wildcard.
    assert_eq!(
        scope, "ryeos.execute.tool.agent-kiwi/oauth/callback",
        "route-handler scope derivation drifted from canonical_cap output"
    );

    let ctx = plan_ctx(&project_dir, vec![scope.clone()]);
    let item = CanonicalRef::parse(HANDLER_REF).expect("canonical ref parses");

    let resolved = engine.resolve(&ctx, &item).expect("resolve handler tool");
    let verified = engine.verify(&ctx, resolved).expect("verify handler tool");
    let plan = engine
        .build_plan(
            &ctx,
            &verified,
            &serde_json::Value::Null,
            &ctx.execution_hints,
        )
        .expect("build_plan must succeed under fixed route-handler authority");

    let app_root = isolation_app_root();
    let isolation = Arc::new(
        ryeos_engine::isolation::IsolationRuntime::load(&app_root)
            .expect("load disabled isolation fixture"),
    );
    let engine_ctx = EngineContext {
        app_root,
        isolation,
        isolation_project_authority: ryeos_engine::isolation::IsolationProjectAuthority::External,
        isolation_live_access_authority: None,
        isolation_state_root: None,
        isolation_checkpoint_dir: None,
        isolation_daemon_socket_path: None,
        isolation_bundle_roots: Vec::new(),
        isolation_node_trusted_keys_dir: None,
        isolation_verified_code: vec![ryeos_engine::isolation::IsolationVerifiedCode {
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
            fingerprint: "route-handler:agent-kiwi:agent-kiwi/google-callback".into(),
            scopes: vec![scope],
        }),
        project_context: ProjectContext::LocalPath {
            path: project_dir.clone(),
        },
        launch_mode: LaunchMode::Wait,
    };

    let completion = engine
        .execute_plan(&engine_ctx, plan)
        .expect("execute_plan runs the handler subprocess");

    let _ = fs::remove_dir_all(&project_dir);

    assert_eq!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "handler subprocess did not complete cleanly: {completion:?}"
    );

    let result = completion
        .result
        .expect("handler emitted a response envelope");
    assert_eq!(
        result["response"]["status"], 302,
        "expected handler 302 redirect envelope, got {result:?}"
    );
    assert_eq!(
        result["response"]["headers"]["Location"], "https://agentkiwi.nz/?google=connected",
        "expected handler redirect Location, got {result:?}"
    );
}
