//! Pins the Python tool subprocess runtime contract: working directory,
//! `project_path` delivery, async `execute`, stdin JSON params, and the
//! missing-`execute` error surface.
//!
//! Interpreter resolution order (env var → project `.venv` → PATH) is
//! pinned separately as fast unit tests in
//! `ryeos_engine::runtime::handlers::env_config::interpreter_resolution_tests`.
//! sys.path isolation (project root NOT importable, bundle-local imports
//! work) is pinned in `hello_world_python.rs`.
//!
//! These are engine-level e2e tests (resolve → verify → build_plan →
//! execute_plan), mirroring `hello_world_python.rs`; each launches a real
//! `python3`. The full contract is documented in
//! `bundles/core/.ai/knowledge/ryeos/core/runtimes/python-runtime-contract.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::contracts::{
    EffectivePrincipal, EngineContext, ExecutionCompletion, ExecutionHints, LaunchMode,
    PlanContext, Principal, ProjectContext, ThreadTerminalStatus,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;
use serde_json::Value;

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
        .with_trust_store(trust_store)
        .with_composers(composers)
}

fn unique_project_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ryeos_pyrt_contract_{tag}_{}_{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a Python tool file with the given runtime executor_id, returning
/// the project root. `rel` is the path under `.ai/tools` (e.g.
/// `probe/probe`), `runtime` is the script/function runtime ref tail.
fn write_python_tool(project_dir: &Path, rel: &str, runtime: &str, body: &str) {
    let tool_path = project_dir
        .join(".ai")
        .join("tools")
        .join(format!("{rel}.py"));
    fs::create_dir_all(tool_path.parent().unwrap()).unwrap();
    let header = format!(
        "#!/usr/bin/env python3\n\
         # ryeos-tool:\n\
         #   category: probe\n\
         #   version: \"1.0.0\"\n\
         #   executor_id: \"tool:ryeos/core/runtimes/python/{runtime}\"\n\
         #   description: \"runtime contract probe\"\n\n"
    );
    fs::write(&tool_path, format!("{header}{body}")).unwrap();
}

/// resolve → verify → build_plan → execute_plan against the live bundle.
fn run_tool(project_dir: &Path, item_ref: &str, params: Value) -> ExecutionCompletion {
    ryeos_tracing::test::prime_callsites();
    let engine = build_engine_against_bundle();

    let plan_ctx = PlanContext {
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
    };

    let item = CanonicalRef::parse(item_ref).expect("canonical ref parses");
    let resolved = engine.resolve(&plan_ctx, &item).expect("resolve tool");
    let verified = engine
        .verify(&plan_ctx, resolved)
        .expect("verify tool (unsigned allowed)");
    let plan = engine
        .build_plan(&plan_ctx, &verified, &params, &plan_ctx.execution_hints)
        .expect("build_plan walks to subprocess terminal");

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
        project_context: ProjectContext::LocalPath {
            path: project_dir.to_path_buf(),
        },
        launch_mode: LaunchMode::Inline,
    };

    engine
        .execute_plan(&engine_ctx, plan)
        .expect("execute_plan runs the subprocess")
}

#[test]
fn cwd_is_project_root() {
    let project_dir = unique_project_dir("cwd");
    write_python_tool(
        &project_dir,
        "probe/cwd",
        "script",
        "import os, json\nprint(json.dumps({\"cwd\": os.getcwd()}))\n",
    );

    let completion = run_tool(&project_dir, "tool:probe/cwd", Value::Null);
    let expected = fs::canonicalize(&project_dir).unwrap();
    let _ = fs::remove_dir_all(&project_dir);

    assert_eq!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "{completion:?}"
    );
    let result = completion.result.expect("captured stdout");
    let cwd = result["cwd"].as_str().expect("cwd string");
    assert_eq!(
        Path::new(cwd),
        expected,
        "subprocess cwd must be the project root"
    );
}

#[test]
fn function_receives_project_path_as_arg_and_in_params() {
    // The function runtime gets `project_path` BOTH as the 2nd arg of
    // `execute(params, project_path)` AND injected into the params object
    // — the injection only fires when params is a JSON object (it does
    // `params.as_object_mut().entry("project_path").or_insert(...)`), so
    // we pass `{}` here, which is the normal CLI/dispatch shape.
    let project_dir = unique_project_dir("pp");
    write_python_tool(
        &project_dir,
        "probe/pp",
        "function",
        "def execute(params, project_path):\n\
         \x20   return {\"arg\": project_path, \"in_params\": params.get(\"project_path\")}\n",
    );

    let completion = run_tool(&project_dir, "tool:probe/pp", serde_json::json!({}));
    assert_eq!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "{completion:?}"
    );
    let result = completion.result.expect("captured stdout");
    // Canonicalize while the project dir still exists, then clean up.
    let expected = fs::canonicalize(&project_dir).unwrap();
    let arg = fs::canonicalize(result["arg"].as_str().expect("arg string")).unwrap();
    let in_params =
        fs::canonicalize(result["in_params"].as_str().expect("in_params string")).unwrap();
    let _ = fs::remove_dir_all(&project_dir);

    // Both point at the project root.
    assert_eq!(arg, expected);
    assert_eq!(in_params, expected);
}

#[test]
fn async_execute_is_supported() {
    // The function runtime awaits an `async def execute`.
    let project_dir = unique_project_dir("async");
    write_python_tool(
        &project_dir,
        "probe/aio",
        "function",
        "import asyncio\n\
         async def execute(params, project_path):\n\
         \x20   await asyncio.sleep(0)\n\
         \x20   return {\"async_ran\": True}\n",
    );

    let completion = run_tool(&project_dir, "tool:probe/aio", Value::Null);
    let _ = fs::remove_dir_all(&project_dir);

    assert_eq!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "{completion:?}"
    );
    let result = completion.result.expect("captured stdout");
    assert_eq!(result["async_ran"], true);
}

#[test]
fn stdin_json_params_delivered_to_function() {
    // Caller params arrive as a JSON object on stdin and are parsed into
    // the `params` dict.
    let project_dir = unique_project_dir("params");
    write_python_tool(
        &project_dir,
        "probe/echo",
        "function",
        "def execute(params, project_path):\n\
         \x20   return {\"echoed\": params.get(\"message\")}\n",
    );

    let completion = run_tool(
        &project_dir,
        "tool:probe/echo",
        serde_json::json!({"message": "ping"}),
    );
    let _ = fs::remove_dir_all(&project_dir);

    assert_eq!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "{completion:?}"
    );
    let result = completion.result.expect("captured stdout");
    assert_eq!(result["echoed"], "ping");
}

#[test]
fn function_missing_execute_fails_loudly() {
    // A function-runtime tool with no `execute` must fail, not silently
    // succeed — the bootstrap exits non-zero with a clear message.
    let project_dir = unique_project_dir("noexec");
    write_python_tool(
        &project_dir,
        "probe/noexec",
        "function",
        "x = 1  # no execute() defined\n",
    );

    let completion = run_tool(&project_dir, "tool:probe/noexec", Value::Null);
    let _ = fs::remove_dir_all(&project_dir);

    assert_ne!(
        completion.status,
        ThreadTerminalStatus::Completed,
        "missing execute() must not be a clean completion: {completion:?}"
    );
    // The failure envelope carries the bootstrap's stderr.
    let detail = serde_json::to_string(&completion.error).unwrap_or_default();
    assert!(
        detail.contains("execute"),
        "error should mention the missing execute(): {detail}"
    );
}
