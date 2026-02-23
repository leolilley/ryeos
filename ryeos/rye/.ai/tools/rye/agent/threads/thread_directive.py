# rye:signed:2026-02-22T09:00:56Z:96e5bb8c20e65323c2cbc6c9a6d230a2bba3fa5745a1104bf18cfde95cd57ac3:wT8mI5KXgyWFfDqrYwnC9wM_-fkn9FWhpKkZDQvf5A6e8rTlwLdUZOKPLdrsi4VbAckC40MoEE8xdQ_ZqhrkBw==:9fbfabe975fa5a7f
__version__ = "1.6.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
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
        "async": {
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
    "additionalProperties": False,
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
    bare_name = directive_name.rsplit("/", 1)[-1]
    return f"{directive_name}/{bare_name}-{epoch}"


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
    thread_dir = project_path / AI_DIR / "agent" / "threads" / thread_id
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
    meta_path = project_path / AI_DIR / "agent" / "threads" / thread_id / "thread.json"
    if meta_path.exists():
        with open(meta_path, "r", encoding="utf-8") as f:
            return json.load(f)
    return None


def _build_prompt(directive: Dict) -> str:
    """Build the LLM prompt from the directive content.

    Only sends what the LLM needs to execute the directive:
      1. Execute instruction
      2. Directive name + description
      3. Permissions (raw XML from directive metadata)
      4. Body (process steps with resolved input values)
      5. Returns section (from <outputs>)
    """
    import re as _re
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

    # Permissions — extract raw XML from directive content as-is
    content = directive.get("content", "")
    if content:
        m = _re.search(r"(<permissions>.*?</permissions>)", content, _re.DOTALL)
        if m:
            parts.append(m.group(1))

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
                    otype = o.get("type", "string")
                    required = o.get("required", False)
                    desc = o.get("description", "")
                    label = f"{desc} ({otype})" if desc else otype
                    if required:
                        label += " [required]"
                    output_fields[oname] = label
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

    # Validate directive/override keys against resilience defaults.
    for source_name, source in (("directive <limits>", directive_limits), ("limit overrides", overrides)):
        for k in source:
            if k not in defaults:
                raise ValueError(
                    f"Unknown limit '{k}' in {source_name}. "
                    f"Valid limits: {', '.join(sorted(defaults))}"
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
    # Pop internal-only params before validation (set by subprocess spawn path)
    thread_id_override = params.pop("_thread_id", None)
    pre_registered = params.pop("_pre_registered", False)
    continuation_message = params.pop("_continuation_message", None)

    allowed = set(CONFIG_SCHEMA["properties"].keys())
    unknown = set(params.keys()) - allowed
    if unknown:
        raise ValueError(f"Unknown parameters: {unknown}. Valid: {allowed}")

    directive_name = params["directive_id"]
    thread_id = thread_id_override or _generate_thread_id(directive_name)
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

    # 2. Register thread in registry (skip if pre-registered by parent process)
    thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
    registry = thread_registry.get_registry(proj_path)
    if not pre_registered:
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

    # 3.5. Reconstruct resume_messages from previous thread's transcript JSONL
    if params.get("previous_thread_id") and not params.get("resume_messages"):
        prev_tid = params["previous_thread_id"]

        # Verify transcript integrity before trusting JSONL content
        from rye.constants import AI_DIR
        transcript_signer_mod = load_module("persistence/transcript_signer", anchor=_ANCHOR)
        signer = transcript_signer_mod.TranscriptSigner(
            prev_tid, proj_path / AI_DIR / "agent" / "threads" / prev_tid
        )
        coordination_loader = load_module("loaders/coordination_loader", anchor=_ANCHOR)
        cont_config = coordination_loader.get_coordination_loader().get_continuation_config(proj_path)
        integrity_policy = cont_config.get("transcript_integrity", "strict")
        integrity = signer.verify(allow_unsigned_trailing=(integrity_policy == "lenient"))
        if not integrity["valid"]:
            registry.update_status(thread_id, "error")
            return {
                "success": False,
                "error": f"Transcript integrity check failed: {integrity['error']}. "
                         f"Cannot resume from tampered transcript.",
                "thread_id": thread_id,
            }

        transcript_mod = load_module("persistence/transcript", anchor=_ANCHOR)
        prev_transcript = transcript_mod.Transcript(prev_tid, proj_path)
        full_messages = prev_transcript.reconstruct_messages()
        if not full_messages:
            registry.update_status(thread_id, "error")
            return {
                "success": False,
                "error": f"Cannot reconstruct messages for thread: {prev_tid}",
                "thread_id": thread_id,
            }

        resume_ceiling = cont_config.get("resume_ceiling_tokens", 16000)

        # Trim trailing messages to ceiling
        trailing: list = []
        trailing_tokens = 0
        for msg in reversed(full_messages):
            msg_tokens = len(str(msg.get("content", ""))) // 4
            if trailing_tokens + msg_tokens > resume_ceiling:
                break
            trailing.insert(0, msg)
            trailing_tokens += msg_tokens

        if not trailing and full_messages:
            trailing = [full_messages[-1]]

        # Ensure starts with user message (providers require it)
        while trailing and trailing[0].get("role") != "user":
            trailing.pop(0)

        if continuation_message:
            trailing.append({"role": "user", "content": continuation_message})
        else:
            trailing.append({
                "role": "user",
                "content": "Continue executing the directive. Pick up where the previous thread left off.",
            })

        params["resume_messages"] = trailing

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
    provider_hint = directive.get("model", {}).get("provider")
    provider_resolver = load_module("adapters/provider_resolver", anchor=_ANCHOR)
    resolved_model, provider_item_id, provider_config = provider_resolver.resolve_provider(
        model, project_path=proj_path, provider=provider_hint
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

    # Build clean directive intent text for hook context
    directive_body = directive.get("body", "").strip()
    directive_desc = directive.get("description", "")
    clean_directive_text = "\n".join(filter(None, [
        directive_name, directive_desc, directive_body
    ]))

    # 9. Write initial thread.json (with limits/depth/caps for child lookup)
    registry.update_status(thread_id, "running")
    _write_thread_meta(
        proj_path, thread_id, directive_name, "running",
        thread_created_at, thread_created_at, model=resolved_model,
        limits=limits, capabilities=harness._capabilities,
    )

    # 10. Set env var so children discover this thread as their parent
    os.environ["RYE_PARENT_THREAD_ID"] = thread_id

    if params.get("async"):
        # Spawn detached subprocess that re-executes this script.
        # Child rebuilds all state via execute() and runs through the sync path.
        child_params = {"directive_id": directive_name, "inputs": inputs}
        if params.get("model"):
            child_params["model"] = params["model"]
        if params.get("limit_overrides"):
            child_params["limit_overrides"] = params["limit_overrides"]
        if params.get("previous_thread_id"):
            child_params["previous_thread_id"] = params["previous_thread_id"]
        if params.get("parent_thread_id"):
            child_params["parent_thread_id"] = params["parent_thread_id"]

        orchestrator_mod = load_module("orchestrator", anchor=_ANCHOR)
        spawn_result = await orchestrator_mod.spawn_detached(
            cmd=sys.executable,
            args=[
                str(Path(__file__).resolve()),
                "--params", json.dumps(child_params),
                "--project-path", str(proj_path),
                "--thread-id", thread_id,
                "--pre-registered",
            ],
            envs={"RYE_PARENT_THREAD_ID": thread_id},
        )

        if not spawn_result.get("success"):
            registry.update_status(thread_id, "error")
            return {
                "success": False,
                "error": f"Failed to spawn async thread: {spawn_result.get('error')}",
                "thread_id": thread_id,
            }

        return {
            "success": True,
            "thread_id": thread_id,
            "status": "running",
            "directive": directive_name,
            "pid": spawn_result["pid"],
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
        directive_body=clean_directive_text,
        previous_thread_id=params.get("previous_thread_id"),
        inputs=inputs,
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
        diag_path = proj_path / ".ai" / "agent" / "threads" / thread_id.replace("/", os.sep) / "diagnostics.json"
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
    parser.add_argument("--thread-id", default=None)
    parser.add_argument("--pre-registered", action="store_true")
    args = parser.parse_args()

    # Initialize debug logging if RYE_DEBUG is set
    if os.environ.get("RYE_DEBUG"):
        import logging
        logging.basicConfig(
            level=logging.DEBUG,
            format="[%(name)s] %(levelname)s: %(message)s",
            stream=sys.stderr,
        )

    params = json.loads(args.params)
    if args.thread_id:
        params["_thread_id"] = args.thread_id
    if args.pre_registered:
        params["_pre_registered"] = True

    result = asyncio.run(execute(params, args.project_path))
    print(json.dumps(result))
