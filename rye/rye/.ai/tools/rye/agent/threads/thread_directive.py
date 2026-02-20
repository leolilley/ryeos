# rye:signed:2026-02-20T01:18:04Z:387f0b19879a068a773ea83e1bf55c197d8e85fba7b020daa70fc73c67b25e15:Eh9lz85MDQMad4mMRXbIByO9tt-8N9GmsPWm1Y3jC_p6gen9wj0cfLbLrdZ0ZEIcvesqSIuzBoPkxZc2Bc-9AA==:440443d0858f0199
__version__ = "1.6.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/agent/threads"
__tool_description__ = "Execute a directive in a managed thread with LLM loop"

import argparse
import asyncio
import json
import os
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import AI_DIR
from module_loader import load_module

_ANCHOR = Path(__file__).parent


CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "directive_id": {
            "type": "string",
            "description": "Directive item_id to execute",
        },
        "async_exec": {
            "type": "boolean",
            "default": False,
            "description": "Return immediately with thread_id",
        },
        "inputs": {
            "type": "object",
            "default": {},
            "description": "Input parameters for the directive",
        },
        "model": {"type": "string", "description": "Override LLM model"},
        "limit_overrides": {
            "type": "object",
            "description": "Override default limits (turns, tokens, spend, spawns, duration_seconds, depth)",
        },
    },
    "required": ["directive_id"],
}


_PRIMARY_TOOLS_DIR = Path(__file__).resolve().parent.parent.parent / "primary"


def _build_tool_schemas() -> list:
    """Build generic tool schemas from primary tools.

    Uses generic keys (name, description, schema) that the provider
    adapter remaps via tool_use.tool_definition config.

    Tool names must be API-safe (alphanumeric, _, -). The full item_id
    is stored in _item_id for dispatcher resolution.
    """
    schemas = []
    for py_file in sorted(_PRIMARY_TOOLS_DIR.glob("rye_*.py")):
        mod = load_module(py_file)
        config_schema = getattr(mod, "CONFIG_SCHEMA", None)
        desc = getattr(mod, "__tool_description__", "")
        category = getattr(mod, "__category__", "")
        if config_schema:
            item_id = f"{category}/{py_file.stem}" if category else py_file.stem
            schemas.append({
                "name": py_file.stem,
                "description": desc,
                "schema": config_schema,
                "_item_id": item_id,
            })
    return schemas


def _generate_thread_id(directive_name: str) -> str:
    epoch = int(time.time())
    return f"{directive_name}-{epoch}"


def _write_thread_meta(
    project_path: Path,
    thread_id: str,
    directive_name: str,
    status: str,
    created_at: str,
    updated_at: str,
    model: Optional[str] = None,
    cost: Optional[Dict[str, Any]] = None,
    limits: Optional[Dict] = None,
    capabilities: Optional[list] = None,
    outputs: Optional[Dict[str, Any]] = None,
) -> None:
    """Write thread.json metadata atomically.

    Stores resolved limits (including depth) and capabilities so child
    threads can look up parent context from the filesystem.
    """
    thread_dir = project_path / AI_DIR / "threads" / thread_id
    thread_dir.mkdir(parents=True, exist_ok=True)

    meta = {
        "thread_id": thread_id,
        "directive": directive_name,
        "status": status,
        "created_at": created_at,
        "updated_at": updated_at,
    }
    if model:
        meta["model"] = model
    if cost:
        meta["cost"] = cost
    if limits:
        meta["limits"] = limits
    if capabilities:
        meta["capabilities"] = capabilities
    if outputs:
        meta["outputs"] = outputs

    # Sign thread.json for integrity (protects capabilities/limits)
    transcript_signer_mod = load_module("persistence/transcript_signer", anchor=_ANCHOR)
    meta = transcript_signer_mod.sign_json(meta)

    meta_path = thread_dir / "thread.json"
    tmp_path = meta_path.with_suffix(".json.tmp")
    with open(tmp_path, "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=2)
    tmp_path.rename(meta_path)


def _read_thread_meta(project_path: Path, thread_id: str) -> Optional[Dict]:
    """Read a thread's thread.json. Returns None if not found."""
    meta_path = project_path / AI_DIR / "threads" / thread_id / "thread.json"
    if meta_path.exists():
        with open(meta_path, "r", encoding="utf-8") as f:
            return json.load(f)
    return None


def _build_prompt(directive: Dict) -> str:
    """Build the LLM prompt from the directive content.

    Only sends what the LLM needs to execute the directive:
      1. Execute instruction
      2. Directive name + description
      3. Body (process steps with resolved input values)
      4. Returns section (from <outputs>)
    """
    from rye.constants import DIRECTIVE_INSTRUCTION
    parts = [DIRECTIVE_INSTRUCTION]

    # Directive name + description
    name = directive.get("name", "")
    desc = directive.get("description", "")
    if name and desc:
        parts.append(f"<directive name=\"{name}\">\n<description>{desc}</description>")
    elif name:
        parts.append(f"<directive name=\"{name}\">")
    elif desc:
        parts.append(f"<directive>\n<description>{desc}</description>")

    # Body (process steps — the actual instructions, already pseudo-XML)
    body = directive.get("body", "").strip()
    if body:
        parts.append(body)

    # Returns instruction — if the directive declares <outputs>, instruct
    # the LLM to call directive_return via rye_execute when done.
    outputs = directive.get("outputs", [])
    if outputs:
        output_fields = {}
        if isinstance(outputs, list):
            for o in outputs:
                oname = o.get("name", "")
                if oname:
                    output_fields[oname] = o.get("description", "")
        elif isinstance(outputs, dict):
            output_fields = dict(outputs)

        if output_fields:
            params_obj = ", ".join(f'"{k}": "<{v or k}>"' for k, v in output_fields.items())
            parts.append(
                "When you have completed all steps, return structured results:\n"
                f'`rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", '
                f"parameters={{{params_obj}}})`"
            )

    # Close directive tag if opened
    if name or desc:
        parts.append("</directive>")

    return "\n".join(parts)


def _resolve_limits(directive_limits: Dict, overrides: Dict, project_path: str, parent_limits: Optional[Dict] = None) -> Dict:
    """Resolve limits: defaults → directive → overrides → parent upper bounds.

    Parent limits cap all values via min(). Depth decrements by 1 per
    level — it represents remaining spawnable depth, not a fixed max.
    """
    resilience_loader = load_module("loaders/resilience_loader", anchor=_ANCHOR)
    defaults = (
        resilience_loader.load(Path(project_path)).get("limits", {}).get("defaults", {})
    )
    resolved = {**defaults, **directive_limits, **overrides}

    if parent_limits:
        for key in ("turns", "tokens", "spend", "spawns", "duration_seconds"):
            if key in parent_limits and key in resolved:
                resolved[key] = min(resolved[key], parent_limits[key])
        if "depth" in parent_limits:
            resolved["depth"] = min(resolved.get("depth", 10), parent_limits["depth"] - 1)

    return resolved


def _merge_hooks(directive_hooks: list, project_path: str) -> list:
    hooks_loader = load_module("loaders/hooks_loader", anchor=_ANCHOR)
    loader = hooks_loader.get_hooks_loader()
    builtin = loader.get_builtin_hooks(Path(project_path))
    infra = loader.get_infra_hooks(Path(project_path))

    for h in directive_hooks:
        h.setdefault("layer", 1)
    for h in builtin:
        h.setdefault("layer", 2)
    for h in infra:
        h.setdefault("layer", 3)

    return sorted(directive_hooks + builtin + infra, key=lambda h: h.get("layer", 2))


async def execute(params: Dict, project_path: str) -> Dict:
    directive_name = params["directive_id"]
    thread_id = _generate_thread_id(directive_name)
    inputs = params.get("inputs", {})
    thread_created_at = datetime.now(timezone.utc).isoformat()
    proj_path = Path(project_path)

    # 1. Resolve parent context
    #    Explicit param (from handoff/resume) takes precedence, then env var
    #    (subprocess inheritance), then no parent (root thread).
    parent_thread_id = params.get("parent_thread_id") or os.environ.get("RYE_PARENT_THREAD_ID")
    parent_meta = None
    if parent_thread_id:
        parent_meta = _read_thread_meta(proj_path, parent_thread_id)
        if not parent_meta:
            return {
                "success": False,
                "error": (
                    f"Parent thread '{parent_thread_id}' declared via "
                    f"RYE_PARENT_THREAD_ID but thread.json not found. "
                    f"Misaligned parent thread data."
                ),
                "thread_id": thread_id,
            }

    # 2. Register thread in registry
    thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
    registry = thread_registry.get_registry(proj_path)
    registry.register(thread_id, directive_name, parent_id=parent_thread_id)

    # 3. Load directive
    from rye.utils.resolvers import get_user_space
    user_space = str(get_user_space())

    if params.get("resume_messages"):
        # Handoff/resume: use LoadTool (no input validation) then parse manually
        from rye.tools.load import LoadTool
        from rye.utils.parser_router import ParserRouter
        load_tool = LoadTool(user_space=user_space)
        result = await load_tool.handle(
            item_type="directive",
            item_id=directive_name,
            project_path=project_path,
        )
        if result["status"] != "success":
            registry.update_status(thread_id, "error")
            return result
        directive = ParserRouter().parse("markdown_xml", result["content"])
    else:
        from rye.tools.execute import ExecuteTool
        exec_tool = ExecuteTool(user_space=user_space)
        result = await exec_tool.handle(
            item_type="directive",
            item_id=directive_name,
            project_path=project_path,
            parameters=inputs,
        )
        if result["status"] != "success":
            registry.update_status(thread_id, "error")
            return result
        directive = result["data"]

    # 4. Build limits with parent as upper bound (depth decrements automatically)
    parent_limits = parent_meta.get("limits") if parent_meta else None
    limits = _resolve_limits(
        directive.get("limits", {}), params.get("limit_overrides", {}),
        project_path, parent_limits=parent_limits,
    )

    # 5. Check depth limit — depth < 0 means parent's depth was exhausted
    if limits.get("depth", 10) < 0:
        registry.update_status(thread_id, "error")
        return {
            "success": False,
            "error": f"Depth limit exhausted (resolved depth={limits['depth']})",
            "thread_id": thread_id,
        }

    # 6. Check spawns limit for parent
    orchestrator_mod = load_module("orchestrator", anchor=_ANCHOR)
    if parent_thread_id:
        spawns_limit = parent_limits.get("spawns", 10) if parent_limits else limits.get("spawns", 10)
        spawn_exceeded = orchestrator_mod.check_spawn_limit(parent_thread_id, spawns_limit)
        if spawn_exceeded:
            registry.update_status(thread_id, "error")
            return {
                "success": False,
                "error": f"Spawn limit exceeded for parent {parent_thread_id}: {spawn_exceeded['current_value']}/{spawn_exceeded['current_max']}",
                "thread_id": thread_id,
            }
        orchestrator_mod.increment_spawn_count(parent_thread_id)

    # 7. Build hooks, harness
    hooks = _merge_hooks(directive.get("hooks", []), project_path)

    SafetyHarness = load_module("safety_harness", anchor=_ANCHOR).SafetyHarness
    permissions = directive.get("permissions", [])
    parent_capabilities = parent_meta.get("capabilities", []) if parent_meta else []
    harness = SafetyHarness(
        thread_id, limits, hooks, proj_path,
        directive_name=directive_name, permissions=permissions,
        parent_capabilities=parent_capabilities,
    )
    harness.available_tools = _build_tool_schemas()

    # Grant directive_return capability and set output field names if directive declares <outputs>
    directive_outputs = directive.get("outputs", [])
    if directive_outputs:
        harness._capabilities.append("rye.execute.tool.rye.agent.threads.directive_return")
        if isinstance(directive_outputs, list):
            harness.output_fields = [o["name"] for o in directive_outputs if o.get("name")]
        elif isinstance(directive_outputs, dict):
            harness.output_fields = list(directive_outputs.keys())

    if not harness.available_tools:
        registry.update_status(thread_id, "error")
        return {
            "success": False,
            "error": (
                f"No tool schemas found in {_PRIMARY_TOOLS_DIR}. "
                "Thread cannot execute without tools."
            ),
            "thread_id": thread_id,
        }

    # 8. Reserve budget
    budgets = load_module("persistence/budgets", anchor=_ANCHOR)
    ledger = budgets.get_ledger(proj_path)
    spend_limit = limits.get("spend", 1.0)
    if parent_thread_id:
        try:
            ledger.reserve(thread_id, spend_limit, parent_thread_id)
        except Exception as e:
            registry.update_status(thread_id, "error")
            return {
                "success": False,
                "error": f"Budget reservation failed: {e}",
                "thread_id": thread_id,
            }
    else:
        ledger.register(thread_id, max_spend=spend_limit)

    user_prompt = _build_prompt(directive)

    ToolDispatcher = load_module("adapters/tool_dispatcher", anchor=_ANCHOR).ToolDispatcher
    dispatcher = ToolDispatcher(proj_path)

    EventEmitter = load_module("events/event_emitter", anchor=_ANCHOR).EventEmitter
    emitter = EventEmitter(proj_path)

    Transcript = load_module("persistence/transcript", anchor=_ANCHOR).Transcript
    transcript = Transcript(thread_id, proj_path)

    model = (
        params.get("model")
        or directive.get("model", {}).get("id")
        or directive.get("model", {}).get("tier", "general")
    )
    provider_resolver = load_module("adapters/provider_resolver", anchor=_ANCHOR)
    resolved_model, provider_item_id, provider_config = provider_resolver.resolve_provider(
        model, project_path=proj_path
    )
    provider_type = provider_config.get("tool_type", "http")
    if provider_type == "http":
        HttpProvider = load_module("adapters/http_provider", anchor=_ANCHOR).HttpProvider
        provider = HttpProvider(
            model=resolved_model,
            provider_config=provider_config,
            dispatcher=dispatcher,
            provider_item_id=provider_item_id,
        )
    else:
        raise ValueError(
            f"Unsupported provider type '{provider_type}' for model '{model}'. "
            f"Only 'http' providers are currently supported."
        )

    runner = load_module("runner", anchor=_ANCHOR)

    # 9. Write initial thread.json (with limits/depth/caps for child lookup)
    registry.update_status(thread_id, "running")
    _write_thread_meta(
        proj_path, thread_id, directive_name, "running",
        thread_created_at, thread_created_at, model=resolved_model,
        limits=limits, capabilities=harness._capabilities,
    )

    # 10. Set env var so children discover this thread as their parent
    os.environ["RYE_PARENT_THREAD_ID"] = thread_id

    if params.get("async_exec"):
        # Fork: child process runs the thread, parent returns immediately.
        # os.fork() duplicates the process — child gets pid=0, parent gets child pid.
        # The child daemonizes (new session) so it survives parent exit.
        child_pid = os.fork()
        if child_pid == 0:
            # Child process — run the thread to completion
            try:
                os.setsid()  # detach from parent's process group
                # Redirect stdout/stderr to devnull so we don't corrupt parent's output
                devnull = os.open(os.devnull, os.O_RDWR)
                os.dup2(devnull, 0)
                os.dup2(devnull, 1)
                os.dup2(devnull, 2)
                os.close(devnull)

                result = asyncio.run(runner.run(
                    thread_id, user_prompt, harness, provider,
                    dispatcher, emitter, transcript, proj_path,
                    resume_messages=params.get("resume_messages"),
                ))

                # Finalize: report spend, update registry
                actual_spend = result.get("cost", {}).get("spend", 0.0)
                try:
                    ledger.report_actual(thread_id, actual_spend)
                except Exception:
                    pass
                if parent_thread_id:
                    ledger.cascade_spend(thread_id, parent_thread_id, actual_spend)
                status = result.get("status", "completed")
                ledger.release(thread_id, final_status=status)
                registry.update_status(thread_id, status)
                result_data = {"cost": result.get("cost")}
                if result.get("outputs"):
                    result_data["outputs"] = result["outputs"]
                registry.set_result(thread_id, result_data)
                _write_thread_meta(
                    proj_path, thread_id, directive_name, status,
                    thread_created_at, datetime.now(timezone.utc).isoformat(),
                    model=resolved_model, cost=result.get("cost"),
                    limits=limits, capabilities=harness._capabilities,
                    outputs=result.get("outputs"),
                )
            except Exception:
                registry.update_status(thread_id, "error")
            finally:
                os._exit(0)
        else:
            # Parent process — return immediately
            return {
                "success": True,
                "thread_id": thread_id,
                "status": "running",
                "directive": directive_name,
                "pid": child_pid,
            }

    # 11. Run thread synchronously
    # resume_messages is an internal param from handoff_thread — not in CONFIG_SCHEMA
    result = await runner.run(
        thread_id,
        user_prompt,
        harness,
        provider,
        dispatcher,
        emitter,
        transcript,
        proj_path,
        resume_messages=params.get("resume_messages"),
    )

    # Ensure non-empty error message on failure
    if not result.get("success") and not result.get("error"):
        result["error"] = result.get("status", "unknown error (no message from runner)")

    # 12. Report spend + cascade to parent + release
    actual_spend = result.get("cost", {}).get("spend", 0.0)
    try:
        ledger.report_actual(thread_id, actual_spend)
    except Exception:
        pass  # overspend is logged but shouldn't block finalization
    if parent_thread_id:
        ledger.cascade_spend(thread_id, parent_thread_id, actual_spend)
    status = result.get("status", "completed")
    ledger.release(thread_id, final_status=status)

    # 13. Update registry with final status
    status = result.get("status", "completed")
    registry.update_status(thread_id, status)
    result_data = {"cost": result.get("cost")}
    if result.get("outputs"):
        result_data["outputs"] = result["outputs"]
    registry.set_result(thread_id, result_data)

    # 14. Write final thread.json
    _write_thread_meta(
        proj_path, thread_id, directive_name, status,
        thread_created_at, datetime.now(timezone.utc).isoformat(),
        model=resolved_model, cost=result.get("cost"),
        limits=limits, capabilities=harness._capabilities,
        outputs=result.get("outputs"),
    )

    # Write per-thread diagnostics file on error for debugging
    if not result.get("success") and os.environ.get("RYE_DEBUG"):
        diag_path = proj_path / ".ai" / "threads" / thread_id.replace("/", os.sep) / "diagnostics.json"
        try:
            import json as _json
            diag_path.parent.mkdir(parents=True, exist_ok=True)
            diag_data = {
                "thread_id": thread_id,
                "directive": directive_name,
                "model": resolved_model,
                "error": result.get("error", ""),
                "cost": result.get("cost", {}),
                "provider_item_id": provider_item_id,
                "limits": limits,
                "timestamp": datetime.now(timezone.utc).isoformat(),
            }
            diag_path.write_text(_json.dumps(diag_data, indent=2, default=str))
        except Exception:
            pass  # diagnostics are best-effort

    # Trim result text to prevent context bloat in parent threads
    MAX_RESULT_CHARS = 4000
    if isinstance(result.get("result"), str) and len(result["result"]) > MAX_RESULT_CHARS:
        result["result"] = result["result"][:MAX_RESULT_CHARS] + "\n\n[... truncated]"
        result["result_truncated"] = True

    return {**result, "directive": directive_name}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()

    # Initialize debug logging if RYE_DEBUG is set
    if os.environ.get("RYE_DEBUG"):
        import logging
        logging.basicConfig(
            level=logging.DEBUG,
            format="[%(name)s] %(levelname)s: %(message)s",
            stream=sys.stderr,
        )

    result = asyncio.run(execute(json.loads(args.params), args.project_path))
    print(json.dumps(result))
