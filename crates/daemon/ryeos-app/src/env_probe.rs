//! Shared tool import dry-run for `env-check` and `doctor`.
//!
//! Compiles the SAME plan a real launch would (`Engine::build_plan`), and for a
//! python-function tool runs a bounded subprocess that reproduces the runtime's
//! exact `sys.path` and imports the tool module WITHOUT calling `execute`.
//!
//! It needs only an `Engine` — never daemon state, secrets, or callback tokens
//! (an import probe doesn't use them) — so the daemon `tool/env-check` handler
//! and the offline `ryeos doctor` share one implementation. Execution is via
//! the synchronous, bounded `lillux::run`, so callers in either context (async
//! handler or sync CLI) use it the same way.

use serde_json::{json, Value};

use ryeos_engine::contracts::{ExecutionHints, PlanContext, PlanNode, VerifiedItem};
use ryeos_engine::engine::Engine;

/// Wall-clock cap (seconds) on the import dry-run subprocess.
const IMPORT_TIMEOUT_SECS: f64 = 20.0;
/// Char cap on captured stderr (tool top-level noise / tracebacks).
const STDERR_CAP: usize = 8 * 1024;

/// Import-only variant of the python-function runtime bootstrap
/// (`bundles/core/.ai/tools/ryeos/core/runtimes/python/function.yaml`). It
/// reproduces the runtime's exact `sys.path` (tool_dir, bundle_tool_root,
/// bundle_tool_root/lib, runtime_dir/lib) and imports the tool module, but does
/// NOT call `execute`. Importing still runs module top-level code — same as a
/// real launch's import step. The structured result is written to the saved
/// stdout fd while the module's own stdout is redirected to stderr, mirroring
/// the runtime's result-channel isolation so tool prints never corrupt it.
pub const IMPORT_PROBE: &str = r#"
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

/// Best-effort import dry-run for a verified item. Never errors: returns a JSON
/// object tagged with an `import_check` scope, and for a python-function tool
/// `interpreter` / `venv_populated` / `import_ok` / `import_error` /
/// `has_execute`. `required_secrets` are dropped from the probe env by name as
/// belt-and-suspenders (the plan-build env carries no secret values anyway).
pub fn import_dry_run(
    engine: &Engine,
    plan_ctx: &PlanContext,
    verified: &VerifiedItem,
    required_secrets: &[String],
) -> Value {
    let plan = match engine.build_plan(plan_ctx, verified, &json!({}), &ExecutionHints::default()) {
        Ok(p) => p,
        Err(e) => {
            return json!({
                "import_check": "unavailable",
                "import_check_reason": format!("could not build execution plan: {e}"),
            });
        }
    };

    let Some(spec) = plan.nodes.iter().find_map(|n| match n {
        PlanNode::DispatchSubprocess { spec, .. } => Some(spec),
        _ => None,
    }) else {
        return json!({ "import_check": "n/a" });
    };

    // Detect the python-function runtime by its bootstrap signature, which
    // fixes the arg layout `[-I, -u, -c, <script>, tool_path, runtime_lib,
    // project_path]`. Anything else is out of scope for the import probe.
    let dash_c = spec.args.iter().position(|a| a == "-c");
    let is_python_function = dash_c.is_some_and(|i| {
        spec.args
            .get(i + 1)
            .is_some_and(|s| s.contains("spec_from_file_location"))
    });
    if !is_python_function {
        return json!({
            "import_check": "n/a",
            "interpreter": spec.cmd,
            "note": "import dry-run covers the python function runtime only; this item uses a different runtime and was not import-checked",
        });
    }
    let dash_c = dash_c.expect("checked by is_python_function");

    // Swap only the bootstrap `-c` payload for the import-only probe; the
    // trailing tool_path / runtime_lib / project_path args are preserved so the
    // probe's `sys.path` matches the real launch exactly.
    let mut args = spec.args.clone();
    args[dash_c + 1] = IMPORT_PROBE.to_string();

    let envs: Vec<(String, String)> = spec
        .env
        .iter()
        .filter(|(k, _)| !required_secrets.iter().any(|s| s == *k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    run_probe(
        &spec.cmd,
        args,
        spec.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
        envs,
    )
}

/// Run the prepared import probe in `interpreter` and shape the result. `args`
/// must already carry the probe as the `-c` payload followed by
/// tool_path / runtime_lib / project_path.
fn run_probe(
    interpreter: &str,
    args: Vec<String>,
    cwd: Option<String>,
    envs: Vec<(String, String)>,
) -> Value {
    let result = lillux::run(lillux::SubprocessRequest {
        cmd: interpreter.to_string(),
        args,
        cwd,
        envs,
        stdin_data: None,
        timeout: IMPORT_TIMEOUT_SECS,
    });
    shape_probe_result(interpreter, result)
}

fn shape_probe_result(interpreter: &str, result: lillux::SubprocessResult) -> Value {
    if result.timed_out {
        return json!({
            "import_check": "python_function",
            "interpreter": interpreter,
            "import_ok": false,
            "import_error": format!("import timed out after {IMPORT_TIMEOUT_SECS}s"),
        });
    }

    let probe: Value = serde_json::from_str(result.stdout.trim()).unwrap_or(Value::Null);
    let Some(obj) = probe.as_object() else {
        let tail: String = result
            .stderr
            .chars()
            .rev()
            .take(STDERR_CAP)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return json!({
            "import_check": "python_function",
            "interpreter": interpreter,
            "import_ok": false,
            "import_error": format!("probe produced no result; stderr tail: {}", tail.trim()),
        });
    };

    let import_ok = obj.get("import_ok").and_then(Value::as_bool).unwrap_or(false);
    let in_venv = obj.get("in_venv").and_then(Value::as_bool).unwrap_or(false);
    let package_count = obj.get("package_count").and_then(Value::as_u64).unwrap_or(0);
    let resolved_interpreter = obj
        .get("interpreter")
        .and_then(Value::as_str)
        .unwrap_or(interpreter);

    let mut out = json!({
        "import_check": "python_function",
        "interpreter": resolved_interpreter,
        "venv_populated": in_venv && package_count > 0,
        "import_ok": import_ok,
    });
    if let Some(err) = obj.get("import_error").and_then(Value::as_str) {
        out["import_error"] = Value::String(err.to_string());
    }
    if let Some(has_exec) = obj.get("has_execute").and_then(Value::as_bool) {
        out["has_execute"] = Value::Bool(has_exec);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python3() -> Option<String> {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| "python3".to_string())
    }

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

    fn plant(root: &std::path::Path, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let tool = root.join(".ai/tools/arc/play.py");
        std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
        std::fs::write(&tool, body).unwrap();
        let lib = root.join("runtime/lib");
        std::fs::create_dir_all(&lib).unwrap();
        (tool, lib)
    }

    #[test]
    fn probe_names_missing_module() {
        let Some(py) = python3() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let (tool, lib) = plant(
            tmp.path(),
            "import definitely_not_a_real_module_xyz\ndef execute(p, pr):\n    return {}\n",
        );
        let out = run_probe(&py, probe_args(&tool, &lib, tmp.path()), None, vec![]);
        assert_eq!(out["import_check"], "python_function");
        assert_eq!(out["import_ok"], false, "{out}");
        let err = out["import_error"].as_str().unwrap_or("");
        assert!(err.contains("ModuleNotFoundError"), "{err}");
        assert!(err.contains("definitely_not_a_real_module_xyz"), "{err}");
    }

    #[test]
    fn probe_succeeds_and_detects_execute() {
        let Some(py) = python3() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let (tool, lib) = plant(tmp.path(), "import os\ndef execute(p, pr):\n    return {}\n");
        let out = run_probe(&py, probe_args(&tool, &lib, tmp.path()), None, vec![]);
        assert_eq!(out["import_ok"], true, "{out}");
        assert_eq!(out["has_execute"], true, "{out}");
    }

    #[test]
    fn probe_isolates_tool_stdout_noise() {
        let Some(py) = python3() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let (tool, lib) = plant(
            tmp.path(),
            "print('noise')\nimport sys; sys.stdout.write('more')\ndef execute(p, pr):\n    return {}\n",
        );
        let out = run_probe(&py, probe_args(&tool, &lib, tmp.path()), None, vec![]);
        assert_eq!(out["import_ok"], true, "noise corrupted result: {out}");
    }

    #[test]
    fn probe_does_not_call_execute() {
        let Some(py) = python3() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let (tool, lib) = plant(
            tmp.path(),
            "def execute(p, pr):\n    raise RuntimeError('must not run')\n",
        );
        let out = run_probe(&py, probe_args(&tool, &lib, tmp.path()), None, vec![]);
        assert_eq!(out["import_ok"], true, "{out}");
        assert!(out.get("import_error").is_none(), "{out}");
    }
}
