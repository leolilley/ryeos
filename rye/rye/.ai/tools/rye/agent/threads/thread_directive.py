# rye:validated:2026-02-09T02:05:23Z:291c457b482c7305b8ff27a28192fd61d0fedd4134b9b72c5fa63e0227ef9fca
"""
Thread Directive Tool.

User-facing tool that spawns a thread and executes a directive
with full SafetyHarness enforcement.

This is the primary entry point for running directives with:
- Cost tracking and limits
- Permission enforcement via CapabilityToken
- Hook-based error handling
- Checkpoint-based control flow
- Data-driven tool-use loop (format defined by provider YAML)
"""

import json
import logging
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

import yaml
import importlib.util
from pathlib import Path as PathLib

_harness_path = PathLib(__file__).parent / "safety_harness.py"
_spec = importlib.util.spec_from_file_location("safety_harness", _harness_path)
_harness_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_harness_module)
SafetyHarness = _harness_module.SafetyHarness
HarnessAction = _harness_module.HarnessAction
HarnessResult = _harness_module.HarnessResult

_registry_path = PathLib(__file__).parent / "thread_registry.py"
_registry_spec = importlib.util.spec_from_file_location("thread_registry", _registry_path)
_registry_module = importlib.util.module_from_spec(_registry_spec)
_registry_spec.loader.exec_module(_registry_module)
ThreadRegistry = _registry_module.ThreadRegistry
TranscriptWriter = _registry_module.TranscriptWriter

logger = logging.getLogger(__name__)

__version__ = "3.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads"
__tool_description__ = "Spawn a thread and execute a directive with safety harness enforcement"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "directive_name": {
            "type": "string",
            "description": "Name of the directive to execute",
        },
        "inputs": {
            "type": "object",
            "description": "Inputs to pass to the directive",
            "default": {},
        },
        "dry_run": {
            "type": "boolean",
            "description": "Validate without executing",
            "default": False,
        },
    },
    "required": ["directive_name"],
}

MODEL_MAP = {
    "haiku": "claude-3-5-haiku-20241022",
    "sonnet": "claude-sonnet-4-20250514",
    "opus": "claude-3-opus-20240229",
}

PROVIDER_MAP = {
    "anthropic": "rye/agent/providers/anthropic_messages",
}

SYSTEM_PROVIDER_FALLBACK = "rye/agent/providers/anthropic_messages"

_cap_tokens_path = PathLib(__file__).parent.parent / "permissions" / "capability_tokens" / "capability_tokens.py"
_cap_spec = importlib.util.spec_from_file_location("capability_tokens", _cap_tokens_path)
_cap_module = importlib.util.module_from_spec(_cap_spec)
_cap_spec.loader.exec_module(_cap_module)
CapabilityToken = _cap_module.CapabilityToken
get_primary_tools_for_caps = _cap_module.get_primary_tools_for_caps
expand_capabilities = _cap_module.expand_capabilities


def _extract_caps_from_permissions(permissions: List) -> List[str]:
    """Extract capability strings from parsed directive permissions.

    Handles both legacy <cap> tags and new hierarchical format
    (normalized to cap entries by the parser).
    """
    caps = []
    for perm in permissions:
        tag = perm.get("tag", "")
        if tag == "cap":
            content = perm.get("content", "")
            if content:
                caps.append(content)
    return caps


def _mint_token_from_permissions(permissions: List, directive_name: str):
    try:
        caps = _extract_caps_from_permissions(permissions)
        if not caps:
            return None
        from datetime import datetime, timedelta, timezone
        return CapabilityToken(
            caps=caps,
            aud="rye-execute",
            exp=datetime.now(timezone.utc) + timedelta(hours=1),
            directive_id=directive_name,
            thread_id=f"{directive_name}-root",
        )
    except Exception as e:
        logger.warning(f"Failed to mint token: {e}")
        return None

MAX_TOOL_ROUNDTRIPS = 10


def _generate_thread_id(directive_name: str) -> str:
    """Generate thread ID using Unix timestamp (UTC).
    
    Format: {directive_name}-{epoch_seconds}
    where epoch_seconds is seconds since 1970-01-01 UTC
    
    Raises ValueError if thread ID already exists in registry.
    """
    # Get UTC timestamp (seconds since 1970-01-01)
    now = datetime.now(timezone.utc)
    epoch_seconds = int(now.timestamp())
    
    thread_id = f"{directive_name}-{epoch_seconds}"
    
    # Check if thread_id already exists
    try:
        project_path = Path.cwd()
        db_path = project_path / ".ai" / "threads" / "registry.db"
        
        # If DB exists, check for clash
        if db_path.exists():
            registry = ThreadRegistry(db_path)
            existing = registry.get_status(thread_id)
            
            if existing:
                raise ValueError(
                    f"Thread ID clash: '{thread_id}' already exists in registry. "
                    f"Cannot spawn thread with same epoch day for same directive. "
                    f"Existing thread created at: {existing.get('created_at')}"
                )
    except ValueError:
        # Re-raise ValueError (clash detected)
        raise
    except Exception as e:
        # Log other errors as warnings but continue (best-effort clash detection)
        logger.warning(f"Error checking thread ID clash: {e}")
    
    return thread_id


def _get_registry(project_path: Path) -> ThreadRegistry:
    db_path = project_path / ".ai" / "threads" / "registry.db"
    return ThreadRegistry(db_path)


def _get_transcript_writer(project_path: Path) -> TranscriptWriter:
    transcript_dir = project_path / ".ai" / "threads"
    return TranscriptWriter(transcript_dir)


def _validate_directive_metadata(directive: Dict, directive_name: str) -> Optional[Dict]:
    missing = []
    if "limits" not in directive:
        missing.append("limits")
    elif not directive["limits"] or isinstance(directive["limits"], str):
        missing.append("limits (declared but empty — must contain max_turns/max_tokens)")
    if "model" not in directive:
        missing.append("model")
    elif not directive["model"] or isinstance(directive["model"], str):
        missing.append("model (declared but empty — must contain tier)")
    if "permissions" not in directive:
        missing.append("permissions (use <permissions /> if none needed)")
    if missing:
        return {
            "status": "failed",
            "error": f"Directive '{directive_name}' missing required thread metadata: {', '.join(missing)}. "
                     f"Thread execution requires limits, model, and permissions to be declared.",
        }
    return None


def _validate_hooks(hooks: List[Dict], directive_name: str) -> Optional[Dict]:
    for i, hook in enumerate(hooks):
        errs = []
        if not hook.get("event") and not hook.get("when"):
            errs.append(f"hook[{i}] missing 'event' or 'when' field")
        if not hook.get("directive"):
            errs.append(f"hook[{i}] missing 'directive' field")
        if errs:
            return {
                "status": "failed",
                "error": f"Directive '{directive_name}' has malformed hooks: {'; '.join(errs)}. "
                         f"Each hook must have at minimum 'event' (or 'when') and 'directive' fields.",
            }
    return None


def _resolve_model_id(model_config: Dict) -> str:
    if model_config.get("id"):
        return model_config["id"]
    tier = model_config.get("tier", "haiku")
    return MODEL_MAP.get(tier, MODEL_MAP["haiku"])


def _resolve_provider(model_config: Dict, project_path: Path) -> str:
    explicit = model_config.get("provider")
    if explicit:
        if "/" in explicit:
            return explicit
        if explicit in PROVIDER_MAP:
            return PROVIDER_MAP[explicit]

    config_path = project_path / ".ai" / "tools" / "agent" / "providers" / "config.yaml"
    if config_path.exists():
        try:
            data = yaml.safe_load(config_path.read_text())
            if isinstance(data, dict):
                default = data.get("default_provider")
                if default:
                    if "/" in default:
                        return default
                    project_provider_path = (
                        project_path / ".ai" / "tools" / "agent" / "providers" / f"{default}.yaml"
                    )
                    if project_provider_path.exists():
                        return f"agent/providers/{default}"
                    if default in PROVIDER_MAP:
                        return PROVIDER_MAP[default]
        except Exception as e:
            logger.warning(f"Failed to load provider config: {e}")

    return SYSTEM_PROVIDER_FALLBACK


def _load_provider_config(provider_id: str, project_path: Path) -> Dict:
    from rye.executor import PrimitiveExecutor
    executor = PrimitiveExecutor(project_path=project_path)
    resolved = executor._resolve_tool_path(provider_id, "project")
    if resolved:
        path, _ = resolved
        content = path.read_text()
        return yaml.safe_load(content)
    return {}


def _load_primary_tool_paths() -> Dict[str, str]:
    """Load primary tool paths from primary.yaml (data-driven)."""
    primary_yaml = PathLib(__file__).parent.parent / "permissions" / "capabilities" / "primary.yaml"
    try:
        data = yaml.safe_load(primary_yaml.read_text())
        return data.get("primary_tools", {})
    except Exception as e:
        logger.warning(f"Failed to load primary.yaml: {e}")
        return {}


def _resolve_tools_for_permissions(permissions: List, project_path: Path) -> List[Dict]:
    """Resolve permissions to primary tools only.

    Capabilities use rye.{primary}.{item_type}.{specifics} format.
    The primary tool name is extracted directly from the capability string.
    All tool execution routes through the 4 primary tools.
    """
    from rye.executor import PrimitiveExecutor
    executor = PrimitiveExecutor(project_path=project_path)

    caps = _extract_caps_from_permissions(permissions)
    if not caps:
        return []

    primaries = get_primary_tools_for_caps(caps)
    if not primaries:
        return []

    primary_tool_paths = _load_primary_tool_paths()

    tool_defs = []
    for primary_name in sorted(primaries):
        tool_id = primary_tool_paths.get(primary_name)
        if not tool_id:
            continue

        resolved = executor._resolve_tool_path(tool_id, "project")
        if not resolved:
            continue
        path, _ = resolved
        metadata = executor._load_metadata_cached(path)
        schema = metadata.get("config_schema")
        if not schema:
            continue

        description = metadata.get("tool_description") or ""
        if not description:
            content = path.read_text()
            for line in content.splitlines():
                if line.strip().startswith('__tool_description__'):
                    description = line.split("=", 1)[1].strip().strip('"').strip("'")
                    break

        tool_defs.append({
            "item_id": tool_id,
            "name": f"rye_{primary_name}",
            "description": description,
            "schema": schema,
        })

    return tool_defs


def _format_tool_defs(tool_defs: List[Dict], provider_config: Dict) -> List[Dict]:
    tool_use_config = provider_config.get("tool_use", {})
    template = tool_use_config.get("tool_definition", {})
    if not template:
        return [
            {"name": t["name"], "description": t["description"], "input_schema": t["schema"]}
            for t in tool_defs
        ]

    formatted = []
    for t in tool_defs:
        entry = {}
        for key, tmpl in template.items():
            if tmpl == "{name}":
                entry[key] = t["name"]
            elif tmpl == "{description}":
                entry[key] = t["description"]
            elif tmpl == "{schema}":
                entry[key] = t["schema"]
            else:
                entry[key] = tmpl
        formatted.append(entry)
    return formatted


def _extract_response(raw: Dict, provider_config: Dict) -> Dict:
    resp_config = provider_config.get("tool_use", {}).get("response", {})

    content_field = resp_config.get("content_field", "content")
    stop_field = resp_config.get("stop_reason_field", "stop_reason")
    stop_tool = resp_config.get("stop_reason_tool_use", "tool_use")
    text_type = resp_config.get("text_block_type", "text")
    text_field = resp_config.get("text_field", "text")
    tu_type = resp_config.get("tool_use_block_type", "tool_use")
    tu_id = resp_config.get("tool_use_id_field", "id")
    tu_name = resp_config.get("tool_use_name_field", "name")
    tu_input = resp_config.get("tool_use_input_field", "input")

    content_blocks = raw.get(content_field, [])
    stop_reason = raw.get(stop_field, "")
    has_tool_use = stop_reason == stop_tool

    text_parts = []
    tool_calls = []
    for block in content_blocks:
        if not isinstance(block, dict):
            continue
        block_type = block.get("type", "")
        if block_type == text_type:
            text_parts.append(block.get(text_field, ""))
        elif block_type == tu_type:
            tool_calls.append({
                "id": block.get(tu_id, ""),
                "name": block.get(tu_name, ""),
                "input": block.get(tu_input, {}),
            })

    return {
        "text": "".join(text_parts),
        "tool_calls": tool_calls,
        "has_tool_use": has_tool_use,
        "content_blocks": content_blocks,
        "stop_reason": stop_reason,
        "usage": raw.get("usage", {}),
        "model": raw.get("model", ""),
    }


def _build_tool_result_message(tool_results: List[Dict], provider_config: Dict) -> Dict:
    tr_config = provider_config.get("tool_use", {}).get("tool_result", {})
    role = tr_config.get("role", "user")
    block_type = tr_config.get("block_type", "tool_result")
    id_field = tr_config.get("id_field", "tool_use_id")
    content_field = tr_config.get("content_field", "content")
    error_field = tr_config.get("error_field", "is_error")

    blocks = []
    for tr in tool_results:
        block = {
            "type": block_type,
            id_field: tr["id"],
            content_field: json.dumps(tr["result"]) if isinstance(tr["result"], dict) else str(tr["result"]),
        }
        if tr.get("is_error"):
            block[error_field] = True
        blocks.append(block)

    return {"role": role, "content": blocks}


def _build_system_prompt(directive: Dict, inputs: Optional[Dict]) -> str:
    parts = [directive.get("description", "")]
    steps = directive.get("steps", [])
    if steps:
        parts.append("\nSteps:")
        for step in steps:
            name = step.get("name", "")
            desc = step.get("description", "")
            parts.append(f"- {name}: {desc}")
    if inputs:
        parts.append(f"\nInputs: {json.dumps(inputs)}")
    return "\n".join(parts)


def _build_user_prompt(directive: Dict, inputs: Optional[Dict]) -> str:
    body = directive.get("body", "")
    if body:
        return body
    return directive.get("description", "Execute this directive.")


async def _call_llm(
    project_path: Path,
    model: str,
    system_prompt: str,
    messages: List[Dict],
    max_tokens: int = 1024,
    provider_id: Optional[str] = None,
    tools: Optional[List[Dict]] = None,
) -> Dict[str, Any]:
    from rye.executor import PrimitiveExecutor

    if provider_id is None:
        provider_id = SYSTEM_PROVIDER_FALLBACK

    executor = PrimitiveExecutor(project_path=project_path)

    params = {
        "model": model,
        "max_tokens": max_tokens,
        "system_prompt": system_prompt,
        "messages": messages,
    }
    if tools:
        params["tools"] = tools

    result = await executor.execute(
        item_id=provider_id,
        parameters=params,
    )

    if not result.success:
        return {"success": False, "error": result.error}

    body = result.data
    if isinstance(body, dict) and "body" in body:
        body = body["body"]

    return {"success": True, "raw": body}


async def _execute_tool_call(
    tool_call: Dict,
    tool_map: Dict[str, str],
    project_path: Path,
) -> Dict:
    from rye.executor import PrimitiveExecutor

    name = tool_call["name"]
    item_id = tool_map.get(name)
    if not item_id:
        return {
            "id": tool_call["id"],
            "result": {"success": False, "error": f"Unknown tool: {name}"},
            "is_error": True,
        }

    executor = PrimitiveExecutor(project_path=project_path)
    result = await executor.execute(
        item_id=item_id,
        parameters=tool_call["input"],
        use_lockfile=True,
    )

    return {
        "id": tool_call["id"],
        "result": result.data if result.success else {"success": False, "error": result.error},
        "is_error": not result.success,
    }


async def _run_tool_use_loop(
    project_path: Path,
    model_id: str,
    system_prompt: str,
    user_prompt: str,
    max_tokens: int,
    provider_id: str,
    provider_config: Dict,
    tool_defs: List[Dict],
    tool_map: Dict[str, str],
    harness: SafetyHarness,
) -> Dict[str, Any]:
    formatted_tools = _format_tool_defs(tool_defs, provider_config) if tool_defs else []

    messages = [{"role": "user", "content": user_prompt}]
    all_tool_results = []
    final_text = ""

    for turn in range(MAX_TOOL_ROUNDTRIPS):
        llm_result = await _call_llm(
            project_path=project_path,
            model=model_id,
            system_prompt=system_prompt,
            messages=messages,
            max_tokens=max_tokens,
            provider_id=provider_id,
            tools=formatted_tools if formatted_tools else None,
        )

        if not llm_result["success"]:
            return llm_result

        raw = llm_result["raw"]
        parsed = _extract_response(raw, provider_config)

        harness.update_cost_after_turn({"usage": parsed["usage"]}, parsed["model"] or model_id)

        limit_event = harness.check_limits()
        if limit_event:
            harness.evaluate_hooks(limit_event)

        messages.append({"role": "assistant", "content": parsed["content_blocks"]})

        if not parsed["has_tool_use"] or not parsed["tool_calls"]:
            final_text = parsed["text"]
            break

        tool_results = []
        for tc in parsed["tool_calls"]:
            tr = await _execute_tool_call(tc, tool_map, project_path)
            tool_results.append(tr)
            all_tool_results.append({
                "tool": tc["name"],
                "input": tc["input"],
                "result": tr["result"],
                "is_error": tr.get("is_error", False),
            })

        result_msg = _build_tool_result_message(tool_results, provider_config)
        messages.append(result_msg)
    else:
        final_text = parsed.get("text", "")

    return {
        "success": True,
        "text": final_text,
        "usage": parsed["usage"],
        "model": parsed["model"] or model_id,
        "raw": raw,
        "tool_results": all_tool_results,
    }


async def _execute_hook(
    hook_directive_name: str,
    hook_inputs: Dict,
    project_path: Path,
    parent_token: Any,
    parent_thread_id: Optional[str] = None,
) -> Dict[str, Any]:
    from rye.executor import PrimitiveExecutor

    if parent_token is None:
        return {
            "status": "permission_denied",
            "error": f"Hook '{hook_directive_name}' rejected: no parent capability token. "
                     f"Hooks cannot self-mint tokens — capabilities must be inherited from the parent directive.",
        }

    if hasattr(parent_token, "to_dict"):
        token_data = parent_token.to_dict()
    elif isinstance(parent_token, dict):
        token_data = parent_token
    else:
        return {
            "status": "permission_denied",
            "error": f"Hook '{hook_directive_name}' rejected: parent token is not serializable.",
        }

    params = {
        "directive_name": hook_directive_name,
        "inputs": hook_inputs,
        "dry_run": False,
        "_token": token_data,
        "_parent_thread_id": parent_thread_id,
    }

    executor = PrimitiveExecutor(project_path=project_path)
    result = await executor.execute(
        item_id="rye/agent/threads/thread_directive",
        parameters=params,
        use_lockfile=True,
    )

    if result.success and isinstance(result.data, dict):
        return result.data
    return {
        "status": "failed",
        "error": result.error or "Hook execution failed via tool chain",
    }


async def execute(
    directive_name: str,
    inputs: Optional[Dict] = None,
    project_path: Optional[str] = None,
    dry_run: bool = False,
    **params
) -> Dict[str, Any]:
    if project_path is None:
        project_path = Path.cwd()
    else:
        project_path = Path(project_path)

    directive = await _load_directive(directive_name, project_path)
    if directive is None:
        return {"status": "failed", "error": f"Directive not found: {directive_name}"}

    err = _validate_directive_metadata(directive, directive_name)
    if err:
        return err

    limits = directive["limits"]
    model_config = directive["model"]
    permissions = directive["permissions"]
    hooks = directive.get("hooks", []) or []

    if hooks:
        err = _validate_hooks(hooks, directive_name)
        if err:
            return err

    async def directive_loader(name: str) -> Optional[Dict]:
        return await _load_directive(name, project_path)

    harness = SafetyHarness(
        project_path=project_path,
        limits=limits,
        hooks=hooks,
        directive_name=directive_name,
        directive_inputs=inputs or {},
        parent_token=params.get("_token"),
        required_permissions=permissions,
        directive_loader=directive_loader,
    )

    token = params.get("_token")
    if token is None:
        token = _mint_token_from_permissions(permissions, directive_name)
        harness.parent_token = token

    perm_event = harness.check_permissions()
    if perm_event:
        hook_result = harness.evaluate_hooks(perm_event)
        if hook_result.context and "hook_directive" in hook_result.context:
            return {
                "status": "hook_triggered",
                "hook": {"directive": hook_result.context["hook_directive"],
                         "inputs": hook_result.context["hook_inputs"]},
                "event": perm_event,
                "cost": harness.cost.to_dict(),
            }
        return {
            "status": "permission_denied",
            "error": perm_event,
            "cost": harness.cost.to_dict(),
        }

    limit_event = harness.check_limits()
    if limit_event:
        hook_result = harness.evaluate_hooks(limit_event)
        if hook_result.context and "hook_directive" in hook_result.context:
            return {
                "status": "hook_triggered",
                "hook": {"directive": hook_result.context["hook_directive"],
                         "inputs": hook_result.context["hook_inputs"]},
                "event": limit_event,
                "cost": harness.cost.to_dict(),
            }

    thread_id = _generate_thread_id(directive_name)
    model_id = _resolve_model_id(model_config)
    provider_id = _resolve_provider(model_config, project_path)
    provider_config = _load_provider_config(provider_id, project_path)

    tool_defs = _resolve_tools_for_permissions(permissions, project_path)
    tool_map = {t["name"]: t["item_id"] for t in tool_defs}

    registry = _get_registry(project_path)
    transcript = _get_transcript_writer(project_path)

    parent_thread_id = params.get("_parent_thread_id")
    try:
        registry.register(
            thread_id=thread_id,
            directive_id=directive_name,
            parent_thread_id=parent_thread_id,
            permission_context={"auto_minted": token is not None},
            cost_budget=limits,
        )
    except Exception as e:
        logger.warning(f"Failed to register thread {thread_id}: {e}")

    transcript.write_event(thread_id, "thread_start", {
        "directive": directive_name,
        "inputs": inputs or {},
        "model": model_id,
        "provider": provider_id,
    })

    if dry_run:
        return {
            "status": "ready",
            "thread_id": thread_id,
            "directive": {
                "name": directive_name,
                "content": directive.get("content", ""),
                "inputs": inputs or {},
            },
            "harness": harness.get_status(),
            "harness_state": harness.to_state_dict(),
            "model": {"id": model_id, **model_config},
            "provider": provider_id,
            "tools": [t["name"] for t in tool_defs],
        }

    system_prompt = _build_system_prompt(directive, inputs)
    user_prompt = _build_user_prompt(directive, inputs)
    max_tokens = limits.get("max_tokens", 1024)

    llm_result = await _run_tool_use_loop(
        project_path=project_path,
        model_id=model_id,
        system_prompt=system_prompt,
        user_prompt=user_prompt,
        max_tokens=max_tokens,
        provider_id=provider_id,
        provider_config=provider_config,
        tool_defs=tool_defs,
        tool_map=tool_map,
        harness=harness,
    )

    if not llm_result["success"]:
        error_event = {"name": "error", "code": "llm_call_failed",
                       "detail": {"error": llm_result.get("error", "")}}
        hook_result = harness.evaluate_hooks(error_event)
        try:
            registry.update_status(thread_id, "error", {"usage": harness.cost.to_dict()})
            transcript.write_event(thread_id, "thread_error", {"error": llm_result.get("error", "")})
        except Exception as e:
            logger.warning(f"Failed to update registry on error: {e}")
        if hook_result.context and "hook_directive" in hook_result.context:
            return {
                "status": "hook_triggered",
                "hook": {"directive": hook_result.context["hook_directive"],
                         "inputs": hook_result.context["hook_inputs"]},
                "error": llm_result.get("error", ""),
                "cost": harness.cost.to_dict(),
            }
        return {
            "status": "failed",
            "thread_id": thread_id,
            "error": llm_result.get("error", ""),
            "cost": harness.cost.to_dict(),
        }

    complete_event = {
        "name": "after_complete",
        "thread_id": thread_id,
        "assistant_text": llm_result["text"],
        "usage": llm_result["usage"],
        "cost": harness.cost.to_dict(),
    }
    hook_result = harness.evaluate_hooks(complete_event)

    hooks_executed = []
    if hook_result.context and "hook_directive" in hook_result.context:
        hook_directive = hook_result.context["hook_directive"]
        hook_inputs_resolved = hook_result.context.get("hook_inputs", {})

        hook_inputs_resolved.setdefault("output_path",
            (inputs or {}).get("output_path", ""))
        hook_inputs_resolved.setdefault("cost", harness.cost.to_dict())
        hook_inputs_resolved.setdefault("usage", llm_result["usage"])
        hook_inputs_resolved.setdefault("thread_id", thread_id)

        hook_exec = await _execute_hook(
            hook_directive_name=hook_directive,
            hook_inputs=hook_inputs_resolved,
            project_path=project_path,
            parent_token=token,
            parent_thread_id=thread_id,
        )
        hooks_executed.append({
            "directive": hook_directive,
            "status": hook_exec.get("status", "unknown"),
            "tool_results": hook_exec.get("tool_results", []),
            "cost": hook_exec.get("cost"),
        })

    result = {
        "status": "completed",
        "thread_id": thread_id,
        "model": llm_result["model"],
        "assistant": {"text": llm_result["text"]},
        "usage": llm_result["usage"],
        "cost": harness.cost.to_dict(),
        "tool_results": llm_result.get("tool_results", []),
        "hooks_executed": hooks_executed,
    }

    try:
        registry.update_status(thread_id, "completed", {"usage": harness.cost.to_dict()})
        transcript.write_event(thread_id, "thread_complete", {
            "usage": llm_result["usage"],
            "cost": harness.cost.to_dict(),
            "tool_results_count": len(llm_result.get("tool_results", [])),
        })
    except Exception as e:
        logger.warning(f"Failed to update registry on completion: {e}")

    return result


async def update_turn(
    harness_state: Dict,
    llm_response: Dict,
    model: str,
    project_path: Optional[str] = None,
    parent_token: Optional[Any] = None,
) -> Dict[str, Any]:
    if project_path is None:
        project_path = Path.cwd()
    else:
        project_path = Path(project_path)

    async def directive_loader(name: str) -> Optional[Dict]:
        return await _load_directive(name, project_path)

    harness = SafetyHarness.from_state_dict(
        state=harness_state,
        project_path=project_path,
        parent_token=parent_token,
        directive_loader=directive_loader,
    )

    harness.update_cost_after_turn(llm_response, model)

    limit_event = harness.check_limits()
    if limit_event:
        hook_result = harness.evaluate_hooks(limit_event)
        if hook_result.context and "hook_directive" in hook_result.context:
            return {
                "status": "hook_triggered",
                "hook": {
                    "directive": hook_result.context["hook_directive"],
                    "inputs": hook_result.context["hook_inputs"],
                },
                "event": limit_event,
                "harness_state": harness.to_state_dict(),
            }

    return {
        "status": "continue",
        "harness_state": harness.to_state_dict(),
    }


async def handle_error(
    harness_state: Dict,
    error_code: str,
    error_detail: Optional[Dict] = None,
    project_path: Optional[str] = None,
    parent_token: Optional[Any] = None,
) -> Dict[str, Any]:
    if project_path is None:
        project_path = Path.cwd()
    else:
        project_path = Path(project_path)

    async def directive_loader(name: str) -> Optional[Dict]:
        return await _load_directive(name, project_path)

    harness = SafetyHarness.from_state_dict(
        state=harness_state,
        project_path=project_path,
        parent_token=parent_token,
        directive_loader=directive_loader,
    )

    result = harness.checkpoint_on_error(error_code, error_detail)

    if result.context and "hook_directive" in result.context:
        return {
            "status": "hook_triggered",
            "hook": {
                "directive": result.context["hook_directive"],
                "inputs": result.context["hook_inputs"],
            },
            "error": {"code": error_code, "detail": error_detail},
            "harness_state": harness.to_state_dict(),
        }

    return {
        "status": "error",
        "error": {"code": error_code, "detail": error_detail},
        "action": "fail",
        "harness_state": harness.to_state_dict(),
    }


async def handle_hook_result(
    harness_state: Dict,
    hook_output: Dict,
    project_path: Optional[str] = None,
    parent_token: Optional[Any] = None,
) -> Dict[str, Any]:
    if project_path is None:
        project_path = Path.cwd()
    else:
        project_path = Path(project_path)

    async def directive_loader(name: str) -> Optional[Dict]:
        return await _load_directive(name, project_path)

    harness = SafetyHarness.from_state_dict(
        state=harness_state,
        project_path=project_path,
        parent_token=parent_token,
        directive_loader=directive_loader,
    )

    action_str = hook_output.get("action", "fail")
    result = harness.handle_hook_action(action_str, hook_output)

    return {
        "status": result.action.value,
        "success": result.success,
        "error": result.error,
        "output": result.output,
        "harness_state": harness.to_state_dict(),
    }


async def _load_directive(name: str, project_path: Path) -> Optional[Dict]:
    try:
        from rye.handlers.directive.handler import DirectiveHandler
        handler = DirectiveHandler(str(project_path))
        file_path = handler.resolve(name)
        if file_path:
            result = handler.parse(file_path)
            return result
        else:
            return None
    except Exception as e:
        logger.error(f"Failed to load directive {name}: {e}")
        return None


async def _fallback_load_directive(name: str, project_path: Path) -> Optional[Dict]:
    import glob

    patterns = [
        project_path / ".ai" / "directives" / f"{name}.md",
        project_path / ".ai" / "directives" / "**" / f"{name}.md",
    ]

    for pattern in patterns:
        matches = glob.glob(str(pattern), recursive=True)
        if matches:
            from pathlib import Path as P
            file_path = P(matches[0])

            parser_path = project_path / ".ai" / "parsers" / "markdown_xml.py"
            if parser_path.exists():
                spec = importlib.util.spec_from_file_location("md_xml_parser", parser_path)
                parser = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(parser)

                content = file_path.read_text()
                return parser.parse(content)

    return None


if __name__ == "__main__":
    import argparse
    import json as _json
    import asyncio as _asyncio

    _parser = argparse.ArgumentParser()
    _parser.add_argument("--params", required=True)
    _parser.add_argument("--project-path", required=True)
    _args = _parser.parse_args()

    _params = _json.loads(_args.params)
    _result = _asyncio.run(execute(
        directive_name=_params.get("directive_name", ""),
        inputs=_params.get("inputs"),
        project_path=_args.project_path,
        dry_run=_params.get("dry_run", False),
        _token=_params.get("_token"),
        _parent_thread_id=_params.get("_parent_thread_id"),
    ))
    print(_json.dumps(_result))
