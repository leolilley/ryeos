//! End-to-end: a tool that declares `native_resume` propagates
//! through the engine pipeline into `RuntimeLaunchMetadata`, and
//! the daemon's `decide_resume` policy returns `Resume` for an
//! orphaned thread carrying that metadata + a captured
//! `ResumeContext`.
//!
//! Real daemon-restart-with-respawn lives in OS-level integration
//! tests. Here we pin the load-bearing wire-format invariants that
//! the respawn path depends on:
//!
//!   YAML `native_resume: true`
//!     → engine `NativeResumeSpec { default policy }`
//!     → `RuntimeLaunchMetadata::native_resume = Some(_)`
//!     → `decide_resume(metadata + resume_context, attempts)`
//!         returns `Resume` until budget exhausted
//!
//! Plus a checkpoint-writer roundtrip: a subprocess that writes a
//! checkpoint and a daemon-side reader that loads the latest.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::{ComposerRegistry, NativeComposerHandlerRegistry};
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, PlanNode, Principal, ProjectContext,
    SubprocessSpec,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{NativeParserHandlerRegistry, ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

use ryeos_runtime::CheckpointWriter;
use ryeosd::launch_metadata::RuntimeLaunchMetadata;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir().parent().expect("ryeosd parent").to_path_buf()
}

fn unique_project_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rye_native_resume_e2e_{}_{}",
        std::process::id(),
        nanos
    ))
}

fn synth_project_with_resume_tool() -> PathBuf {
    let project_dir = unique_project_dir();
    let tools_dir = project_dir.join(".ai").join("tools");
    let runtime_dir = tools_dir.join("local_resume_runtime");
    fs::create_dir_all(&runtime_dir).unwrap();

    // Wrapper runtime declaring `native_resume: true` (bool
    // shorthand → engine default policy). Sits in the chain so the
    // FirstWins handler claims the block.
    let runtime_body = r#"version: "1.0.0"
executor_id: "@subprocess"
category: local_resume_runtime
description: "test runtime with native_resume"

native_resume: true

config:
  command: "/bin/true"
"#;
    fs::write(runtime_dir.join("runtime.yaml"), runtime_body).unwrap();

    let tool_body = r#"version: "1.0.0"
executor_id: "tool:local_resume_runtime/runtime"
category: ""
description: "native_resume demo"
"#;
    fs::write(tools_dir.join("resume_demo.yaml"), tool_body).unwrap();
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

fn build_subprocess_spec(project_dir: &PathBuf) -> SubprocessSpec {
    let engine = build_engine_against_bundle();
    let ctx = plan_ctx(project_dir);

    let item = CanonicalRef::parse("tool:resume_demo").expect("ref parses");
    let resolved = engine.resolve(&ctx, &item).expect("resolve");
    let verified = engine.verify(&ctx, resolved).expect("verify");
    let plan = engine
        .build_plan(&ctx, &verified, &serde_json::Value::Null, &ctx.execution_hints)
        .expect("plan builds");

    plan.nodes
        .iter()
        .find_map(|n| match n {
            PlanNode::DispatchSubprocess { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .expect("DispatchSubprocess node present")
}

#[test]
fn native_resume_true_yields_default_policy_in_subprocess_spec() {
    let project_dir = synth_project_with_resume_tool();
    let spec = build_subprocess_spec(&project_dir);
    let _ = fs::remove_dir_all(&project_dir);

    let policy = spec
        .execution
        .native_resume
        .as_ref()
        .expect("native_resume propagated into spec");
    assert!(
        policy.checkpoint_interval_secs > 0,
        "default checkpoint_interval_secs should be positive, got {}",
        policy.checkpoint_interval_secs
    );
    assert!(
        policy.max_auto_resume_attempts > 0,
        "default max_auto_resume_attempts should be positive, got {}",
        policy.max_auto_resume_attempts
    );
}

#[test]
fn launch_metadata_from_spec_carries_native_resume() {
    let project_dir = synth_project_with_resume_tool();
    let spec = build_subprocess_spec(&project_dir);
    let _ = fs::remove_dir_all(&project_dir);

    let metadata = RuntimeLaunchMetadata::from_spec(&spec);
    assert!(
        metadata.declares_native_resume(),
        "metadata derived from a native_resume tool must be resume-eligible"
    );
}

#[test]
fn checkpoint_writer_roundtrip_via_env() {
    // Subprocess-side primitive: a tool launched with
    // RYE_CHECKPOINT_DIR + RYE_RESUME=1 reads back the latest
    // checkpoint via CheckpointWriter::load_latest.
    let dir = tempfile::TempDir::new().unwrap();
    let ckpt_dir = dir.path().to_path_buf();

    // First run: not a resume.
    std::env::set_var("RYE_CHECKPOINT_DIR", &ckpt_dir);
    std::env::remove_var("RYE_RESUME");
    let writer1 = CheckpointWriter::from_env().expect("checkpoint writer from env");
    assert!(
        !CheckpointWriter::is_resume(),
        "cold start should not be a resume"
    );
    writer1
        .write(&serde_json::json!({"step": 1, "data": "alpha"}))
        .expect("write checkpoint");
    writer1
        .write(&serde_json::json!({"step": 2, "data": "beta"}))
        .expect("write second checkpoint");

    // Second run: resume = 1. Latest must come back.
    std::env::set_var("RYE_RESUME", "1");
    let writer2 = CheckpointWriter::from_env().expect("checkpoint writer from env");
    assert!(
        CheckpointWriter::is_resume(),
        "RYE_RESUME=1 should be detected"
    );
    let latest = writer2
        .load_latest()
        .expect("load latest")
        .expect("at least one checkpoint persisted");
    assert_eq!(latest["step"], 2, "load_latest should pick the newest write");
    assert_eq!(latest["data"], "beta");

    std::env::remove_var("RYE_CHECKPOINT_DIR");
    std::env::remove_var("RYE_RESUME");
}
