//! `tool/env-check` — report which source would satisfy each declared secret
//! of an item, WITHOUT running it. Names and sources only, never values.
//!
//! Resolves the item against the live engine + the caller's project, reads its
//! declared `required_secrets`, and reports per-secret provenance
//! (vault / host env / which `.env` / missing) via
//! `vault::resolve_secret_sources` — which mirrors the real launch precedence.
//!
//! DaemonOnly: the authoritative host-env source is the daemon's process
//! environment, so the report must come from the daemon, not an offline CLI.
//!
//! Scope (v1): item `required_secrets`. Runtime-derived launch envelope
//! requirements such as provider auth are resolved separately at launch and
//! are not yet enumerated here — a follow-up will add them via the same
//! resolver.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_app::vault::SecretSource;
use ryeos_engine::contracts::{ExecutionHints, PlanContext, PlanNode};
use ryeos_executor::executor::ServiceAvailability;

/// Wall-clock cap on the import dry-run subprocess.
const IMPORT_CHECK_TIMEOUT: Duration = Duration::from_secs(20);
/// Byte cap on captured stderr (tool top-level noise / tracebacks).
const IMPORT_CHECK_STDERR_CAP: usize = 8 * 1024;

/// Import-only variant of the python-function runtime bootstrap
/// (`bundles/core/.ai/tools/ryeos/core/runtimes/python/function.yaml`). It
/// reproduces the runtime's exact `sys.path` (tool_dir, bundle_tool_root,
/// bundle_tool_root/lib, runtime_dir/lib) and imports the tool module, but does
/// NOT call `execute`. Importing still runs module top-level code — same as a
/// real launch's import step. The structured result is written to the saved
/// stdout fd while the module's own stdout is redirected to stderr, mirroring
/// the runtime's result-channel isolation so tool prints never corrupt it.
const IMPORT_PROBE: &str = r#"
import os,sys,json,importlib.util
from pathlib import Path
_saved=os.dup(1)
os.set_inheritable(_saved,False)
os.dup2(2,1)
sys.stdout=sys.stderr
def prepend(path):
    value=str(path)
    if value and value not in sys.path:sys.path.insert(0,value)
result={"import_ok":False}
try:
    tool_path=Path(sys.argv[1]).resolve()
    runtime_lib=Path(sys.argv[2]).resolve()
    tools_root=None
    for candidate in [tool_path.parent,*tool_path.parents]:
        if candidate.name=="tools" and candidate.parent.name==".ai":
            tools_root=candidate;break
    if tools_root is None:
        raise RuntimeError("tool is not under .ai/tools: "+str(tool_path))
    rel=tool_path.relative_to(tools_root)
    bundle_tool_root=tools_root/rel.parts[0]
    tool_dir=tool_path.parent
    for path in reversed([tool_dir,bundle_tool_root,bundle_tool_root/"lib",runtime_lib]):prepend(path)
    spec=importlib.util.spec_from_file_location("tool",str(tool_path))
    mod=importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    result["import_ok"]=True
    result["has_execute"]=callable(getattr(mod,"execute",None))
except ModuleNotFoundError as e:
    name=getattr(e,"name",None)
    result["import_error"]=("ModuleNotFoundError: missing module '"+name+"'") if name else ("ModuleNotFoundError: "+str(e))
except Exception as e:
    result["import_error"]=type(e).__name__+": "+str(e)
result["in_venv"]=sys.prefix!=sys.base_prefix
result["interpreter"]=sys.executable
pkgs=set()
try:
    import site
    for sp in (site.getsitepackages() if hasattr(site,"getsitepackages") else []):
        try:
            for entry in os.listdir(sp):
                if entry.endswith((".dist-info",".egg-info")):pkgs.add(entry.split("-")[0].lower())
        except Exception:pass
except Exception:pass
result["package_count"]=len(pkgs)
os.write(_saved,json.dumps(result).encode())
"#;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The item whose declared secrets to check (e.g. `tool:foo/bar`).
    pub item_ref: String,
    /// Project root the item resolves against (also the `.env` overlay root).
    /// Bound by the CLI from the discovered project; absent when run outside
    /// a project.
    #[serde(default)]
    pub project_path: Option<String>,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;

    let project_path = req.project_path.ok_or_else(|| {
        HandlerError::BadRequest(
            "env-check requires a project: run inside a project directory".into(),
        )
    })?;

    let canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&req.item_ref).map_err(|e| {
            HandlerError::BadRequest(format!("invalid item_ref `{}`: {e}", req.item_ref))
        })?;

    // The report leaks secret presence/source, so the caller must hold the same
    // execute capability for the TARGET item that a real launch requires — being
    // allowed to call env-check is not enough on its own.
    let required_cap =
        ryeos_runtime::authorizer::canonical_cap(&canonical.kind, &canonical.bare_id, "execute");
    let policy =
        ryeos_runtime::authorizer::AuthorizationPolicy::require_all(&[required_cap.as_str()]);
    state
        .authorizer
        .authorize(&ctx.scopes, &policy)
        .map_err(|_| {
            HandlerError::Forbidden(format!("missing required capability: {required_cap}"))
        })?;

    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        project_context: ProjectContext::LocalPath {
            path: std::path::PathBuf::from(&project_path),
        },
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: Default::default(),
        validate_only: true,
    };

    // Resolve AND verify the trust chain, exactly like a real launch, before
    // reading declared metadata.
    let verified = ryeos_executor::executor::resolve_and_verify(
        &state.engine,
        &plan_ctx,
        &req.item_ref,
        Some("env-check target"),
    )
    .map_err(|e| HandlerError::BadRequest(format!("could not verify `{}`: {e:#}", req.item_ref)))?;

    let names = verified.resolved.metadata.required_secrets.clone();
    let dotenv_dirs =
        ryeos_app::vault::dotenv_search_dirs(Some(std::path::Path::new(&project_path)));
    let report = ryeos_app::vault::resolve_secret_sources(
        state.vault.as_ref(),
        &ctx.fingerprint,
        &names,
        &dotenv_dirs,
    )
    .map_err(|e| HandlerError::Internal(e.to_string()))?;

    let secrets: Vec<Value> = report
        .iter()
        .map(|(name, source)| {
            let mut obj = serde_json::json!({ "name": name, "source": source.label() });
            if let SecretSource::Dotenv(dir) = source {
                obj["dotenv_dir"] = Value::String(dir.display().to_string());
            }
            obj
        })
        .collect();
    let missing: Vec<&str> = report
        .iter()
        .filter(|(_, s)| matches!(s, SecretSource::Missing))
        .map(|(n, _)| n.as_str())
        .collect();

    // Import dry-run: for a python tool, reproduce the launch interpreter +
    // sys.path and attempt the import (without calling `execute`), so an empty
    // `.venv` or a `ModuleNotFoundError` surfaces here rather than at first run.
    let import_report = import_dry_run(&state, &plan_ctx, &verified, &names).await;

    let mut response = serde_json::json!({
        "item_ref": req.item_ref,
        "kind": canonical.kind,
        "secrets": secrets,
        "missing": missing,
        // v1 reports declared `required_secrets` only. A directive's provider
        // `auth.env_var` is resolved at launch (preflight) and is not yet
        // enumerated here — surfaced so clients don't assume it was checked.
        "provider_auth_checked": false,
    });
    if let (Some(obj), Some(extra)) = (response.as_object_mut(), import_report.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    Ok(response)
}

/// Best-effort import dry-run for the verified item, reproducing the launch
/// interpreter + `sys.path` without creating a thread or minting tokens.
///
/// Never fails the env-check: returns a JSON object with an `import_check`
/// scope tag, and for python-function tools `interpreter` / `venv_populated` /
/// `import_ok` / `import_error`.
async fn import_dry_run(
    state: &AppState,
    plan_ctx: &PlanContext,
    verified: &ryeos_engine::contracts::VerifiedItem,
    required_secrets: &[String],
) -> Value {
    // Compile the SAME plan a real launch would — this resolves the
    // interpreter, the `sys.path` prefixes, and the env, with no side effects.
    let plan = match state.engine.build_plan(
        plan_ctx,
        verified,
        &serde_json::json!({}),
        &ExecutionHints::default(),
    ) {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({
                "import_check": "unavailable",
                "import_check_reason": format!("could not build execution plan: {e}"),
            });
        }
    };

    let Some(spec) = plan.nodes.iter().find_map(|n| match n {
        PlanNode::DispatchSubprocess { spec, .. } => Some(spec),
        _ => None,
    }) else {
        // Not a subprocess tool (directive/graph/etc.) — nothing to import.
        return serde_json::json!({ "import_check": "n/a" });
    };

    // Detect the python-function runtime by its bootstrap signature so we know
    // the arg layout `[-I, -u, -c, <script>, tool_path, runtime_lib,
    // project_path]`. Other subprocess shapes are out of scope for the import
    // probe (reported `n/a`).
    let dash_c = spec.args.iter().position(|a| a == "-c");
    let is_python_function = dash_c.is_some_and(|i| {
        spec.args
            .get(i + 1)
            .is_some_and(|s| s.contains("spec_from_file_location"))
    });

    if !is_python_function {
        return serde_json::json!({
            "import_check": "n/a",
            "interpreter": spec.cmd,
        });
    }
    let dash_c = dash_c.expect("checked by is_python_function");

    // Swap only the bootstrap `-c` payload for the import-only probe; the
    // trailing tool_path / runtime_lib / project_path args are preserved, so
    // the probe's `sys.path` matches the real launch exactly.
    let mut probe_args = spec.args.clone();
    probe_args[dash_c + 1] = IMPORT_PROBE.to_string();

    run_import_probe(
        &spec.cmd,
        &probe_args,
        &spec.env,
        required_secrets,
        spec.cwd.as_deref(),
    )
    .await
}

/// Spawn the bounded import probe and shape its structured result.
async fn run_import_probe(
    interpreter: &str,
    args: &[String],
    spec_env: &HashMap<String, String>,
    required_secrets: &[String],
    cwd: Option<&std::path::Path>,
) -> Value {
    let mut cmd = tokio::process::Command::new(interpreter);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // The resolved spec env carries no secret values — secrets and callback
    // tokens are injected only at launch, after `build_plan`. Pass it through
    // (so PATH mutations / interpreter vars match launch), dropping anything
    // named like a declared secret as belt-and-suspenders, and ensure PATH.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in spec_env {
        if required_secrets.iter().any(|s| s == k) {
            continue;
        }
        cmd.env(k, v);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({
                "import_check": "python_function",
                "interpreter": interpreter,
                "import_ok": false,
                "import_error": format!("could not spawn interpreter `{interpreter}`: {e}"),
            });
        }
    };

    let out = match tokio::time::timeout(IMPORT_CHECK_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return serde_json::json!({
                "import_check": "python_function",
                "interpreter": interpreter,
                "import_ok": false,
                "import_error": format!("interpreter failed: {e}"),
            });
        }
        Err(_) => {
            return serde_json::json!({
                "import_check": "python_function",
                "interpreter": interpreter,
                "import_ok": false,
                "import_error": format!("import timed out after {}s", IMPORT_CHECK_TIMEOUT.as_secs()),
            });
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let probe: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    let Some(probe) = probe.as_object() else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .chars()
            .rev()
            .take(IMPORT_CHECK_STDERR_CAP)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return serde_json::json!({
            "import_check": "python_function",
            "interpreter": interpreter,
            "import_ok": false,
            "import_error": format!("probe produced no result; stderr tail: {}", tail.trim()),
        });
    };

    let import_ok = probe.get("import_ok").and_then(Value::as_bool).unwrap_or(false);
    let in_venv = probe.get("in_venv").and_then(Value::as_bool).unwrap_or(false);
    let package_count = probe.get("package_count").and_then(Value::as_u64).unwrap_or(0);
    let resolved_interpreter = probe
        .get("interpreter")
        .and_then(Value::as_str)
        .unwrap_or(interpreter);

    let mut obj = serde_json::json!({
        "import_check": "python_function",
        "interpreter": resolved_interpreter,
        "venv_populated": in_venv && package_count > 0,
        "import_ok": import_ok,
    });
    if let Some(err) = probe.get("import_error").and_then(Value::as_str) {
        obj["import_error"] = Value::String(err.to_string());
    }
    if let Some(has_exec) = probe.get("has_execute").and_then(Value::as_bool) {
        obj["has_execute"] = Value::Bool(has_exec);
    }
    obj
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:tool/env-check",
    endpoint: "tool.env-check",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.tool/env-check"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    /// `python3`, or `None` to skip the test in an environment without it.
    fn python3() -> Option<String> {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| "python3".to_string())
    }

    /// Build the probe argv exactly as `import_dry_run` does after swapping the
    /// bootstrap `-c` payload: `[-I, -u, -c, <probe>, tool, runtime_lib, proj]`.
    fn probe_args(tool: &std::path::Path, runtime_lib: &std::path::Path, project: &std::path::Path) -> Vec<String> {
        vec![
            "-I".into(),
            "-u".into(),
            "-c".into(),
            IMPORT_PROBE.to_string(),
            tool.to_string_lossy().into_owned(),
            runtime_lib.to_string_lossy().into_owned(),
            project.to_string_lossy().into_owned(),
        ]
    }

    /// Write a python tool under a `.ai/tools/<bundle>/` tree (so the probe's
    /// tools-root detection finds it) and return (tool_path, runtime_lib).
    fn plant_tool(root: &std::path::Path, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let tool = root.join(".ai/tools/arc/play.py");
        std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
        std::fs::write(&tool, body).unwrap();
        let runtime_lib = root.join("runtime/lib");
        std::fs::create_dir_all(&runtime_lib).unwrap();
        (tool, runtime_lib)
    }

    #[tokio::test]
    async fn import_probe_names_missing_module() {
        let Some(py) = python3() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let (tool, lib) = plant_tool(
            tmp.path(),
            "import definitely_not_a_real_module_xyz\ndef execute(p, pr):\n    return {}\n",
        );
        let report = run_import_probe(
            &py,
            &probe_args(&tool, &lib, tmp.path()),
            &HashMap::new(),
            &[],
            None,
        )
        .await;
        assert_eq!(report["import_check"], "python_function");
        assert_eq!(report["import_ok"], false, "report: {report}");
        let err = report["import_error"].as_str().unwrap_or("");
        assert!(err.contains("ModuleNotFoundError"), "err: {err}");
        assert!(
            err.contains("definitely_not_a_real_module_xyz"),
            "error must name the missing module: {err}"
        );
    }

    #[tokio::test]
    async fn import_probe_succeeds_and_detects_execute() {
        let Some(py) = python3() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let (tool, lib) = plant_tool(
            tmp.path(),
            "import os, sys, json\ndef execute(p, pr):\n    return {}\n",
        );
        let report = run_import_probe(
            &py,
            &probe_args(&tool, &lib, tmp.path()),
            &HashMap::new(),
            &[],
            None,
        )
        .await;
        assert_eq!(report["import_ok"], true, "report: {report}");
        assert_eq!(report["has_execute"], true, "report: {report}");
    }

    #[tokio::test]
    async fn import_probe_isolates_tool_stdout_noise() {
        // Top-level stdout prints must not corrupt the result channel.
        let Some(py) = python3() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let (tool, lib) = plant_tool(
            tmp.path(),
            "print('noise on stdout')\nimport sys; sys.stdout.write('more noise')\ndef execute(p, pr):\n    return {}\n",
        );
        let report = run_import_probe(
            &py,
            &probe_args(&tool, &lib, tmp.path()),
            &HashMap::new(),
            &[],
            None,
        )
        .await;
        assert_eq!(
            report["import_ok"], true,
            "tool stdout noise corrupted the result channel: {report}"
        );
    }

    #[tokio::test]
    async fn import_probe_does_not_call_execute() {
        // `execute` raising must NOT affect the import result — we import only.
        let Some(py) = python3() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let (tool, lib) = plant_tool(
            tmp.path(),
            "def execute(p, pr):\n    raise RuntimeError('execute must not run')\n",
        );
        let report = run_import_probe(
            &py,
            &probe_args(&tool, &lib, tmp.path()),
            &HashMap::new(),
            &[],
            None,
        )
        .await;
        assert_eq!(report["import_ok"], true, "report: {report}");
        assert_eq!(report["has_execute"], true, "report: {report}");
        assert!(report.get("import_error").is_none(), "report: {report}");
    }
}
