# rye:validated:2026-02-10T00:53:52Z:35bb62a1d528901465e5f1d56b75843ad7a3691c0eab2d641eb26514f4a25692
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

    Permissions are parsed from hierarchical XML format
    (<execute><tool>...</tool></execute>) and normalized to
    {"tag": "cap", "content": "rye.execute.tool.X"} by the parser.
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


def _write_thread_meta_atomic(
    thread_dir: Path,
    thread_id: str,
    directive_name: str,
    status: str,
    created_at: str,
    updated_at: str,
    model: Optional[str] = None,
    cost: Optional[Dict[str, Any]] = None,
) -> None:
    """
    Write thread.json metadata atomically using tmp -> rename pattern.
    
    Args:
        thread_dir: .ai/threads directory
        thread_id: Thread identifier
        directive_name: Name of directive that spawned the thread
        status: Thread status (running, completed, error)
        created_at: ISO 8601 timestamp of thread creation
        updated_at: ISO 8601 timestamp of last update
        model: Model ID used for this thread
        cost: Cost dict with keys: tokens, spend, turns, duration_seconds
    """
    thread_path = thread_dir / thread_id
    thread_path.mkdir(parents=True, exist_ok=True)
    
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
    
    # Atomic write: tmp -> final rename
    meta_path = thread_path / "thread.json"
    tmp_path = meta_path.with_suffix(".json.tmp")
    
    try:
        with open(tmp_path, "w", encoding="utf-8") as f:
            json.dump(meta, f, indent=2)
        tmp_path.rename(meta_path)
        logger.debug(f"Wrote thread metadata to {meta_path}")
    except Exception as e:
        # Clean up temp file if rename failed
        if tmp_path.exists():
            tmp_path.unlink()
        logger.error(f"Failed to write thread metadata: {e}")
        raise


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


def _strip_rye_signature(text: str) -> str:
    """Strip rye:validated signature comments from directive body."""
    import re
    return re.sub(r'<!--\s*rye:validated:[^>]*-->\s*', '', text).strip()


def _resolve_input_refs(value: str, inputs: Optional[Dict]) -> str:
    """Resolve {input:name} placeholders with actual input values."""
    if not inputs or "{input:" not in value:
        return value
    import re
    def replacer(m):
        key = m.group(1)
        if key in inputs:
            return str(inputs[key])
        return m.group(0)
    return re.sub(r"\{input:(\w+)\}", replacer, value)


def _render_action(action: Dict, inputs: Optional[Dict]) -> str:
    """Render a parsed action as a canonical primary-tool call block.

    Maps the 4 action tags to their primary tools:
    - <execute> → rye_execute (item_type + item_id + parameters)
    - <search>  → rye_search  (query + item_type + ...)
    - <load>    → rye_load    (item_type + item_id + ...)
    - <sign>    → rye_sign    (item_type + item_id + ...)
    """
    primary = action.get("primary", "execute")
    params = {}
    for k, v in (action.get("params") or {}).items():
        params[k] = _resolve_input_refs(v, inputs)

    if primary == "execute":
        item_type = action.get("item_type", "tool")
        item_id = _resolve_input_refs(action.get("item_id", ""), inputs)
        call = {"item_type": item_type, "item_id": item_id}
        if params:
            if item_type == "directive":
                call["parameters"] = {"directive_name": item_id, "inputs": params}
            else:
                call["parameters"] = params
        return f"  Call tool: rye_execute\n  {json.dumps(call)}"

    tool_name = f"rye_{primary}"
    call = {}
    for attr in ("item_type", "item_id", "query", "source", "limit"):
        if attr in action:
            call[attr] = _resolve_input_refs(action[attr], inputs)
    call.update(params)
    return f"  Call tool: {tool_name}\n  {json.dumps(call)}"


def _format_steps_block(directive: Dict, inputs: Optional[Dict] = None) -> str:
    """Format directive steps as a labeled block for the system prompt.

    If steps contain <execute> actions, renders canonical tool-call
    JSON blocks targeting the appropriate primary tool.
    """
    steps = directive.get("steps", [])
    if not steps:
        return ""
    lines = ["Steps:"]
    for i, step in enumerate(steps, 1):
        name = step.get("name", "")
        desc = step.get("description", "")
        actions = step.get("actions", [])

        if actions:
            lines.append(f"{i}) {name} — {desc}" if desc else f"{i}) {name}")
            for action in actions:
                lines.append(_render_action(action, inputs))
        else:
            lines.append(f"- {name}: {desc}")
    return "\n".join(lines)


def _format_inputs_block(inputs: Optional[Dict]) -> str:
    """Format directive inputs as a labeled block for the system prompt."""
    if not inputs:
        return ""
    return f"Inputs: {json.dumps(inputs)}"


def _build_system_prompt(
    directive: Dict,
    inputs: Optional[Dict],
    tool_defs: Optional[List[Dict]] = None,
    provider_config: Optional[Dict] = None,
    directive_name: str = "",
) -> str:
    """Build the system prompt from the provider's template.

    The provider YAML owns the system_template (data-driven).
    This function renders placeholders with directive metadata and tool names.
    Raises ValueError if no template is configured — providers MUST define one.
    """
    if provider_config is None:
        raise ValueError(
            f"Cannot build system prompt for directive '{directive_name}': "
            f"no provider config loaded. Provider YAML must be resolved before building prompts."
        )

    prompts = provider_config.get("prompts") or {}
    template = prompts.get("system_template")
    if not template:
        raise ValueError(
            f"Cannot build system prompt for directive '{directive_name}': "
            f"provider config missing 'prompts.system_template'. "
            f"The provider YAML must define a system_template under the prompts key."
        )

    tool_names = ", ".join(t["name"] for t in tool_defs) if tool_defs else "(none)"
    description = directive.get("description", "")
    steps_block = _format_steps_block(directive, inputs)
    inputs_block = _format_inputs_block(inputs)

    return template.format(
        tool_names=tool_names,
        directive_name=directive_name,
        directive_description=description,
        directive_steps=steps_block,
        directive_inputs=inputs_block,
    ).strip()


def _build_user_prompt(directive: Dict, inputs: Optional[Dict]) -> str:
    """Build the user message from the directive body.

    The user prompt contains the full directive body (the task to execute).
    The system prompt already has the agent framing and tool instructions.
    """
    body = directive.get("body", "")
    if body:
        cleaned = _strip_rye_signature(body)
        if cleaned:
            return cleaned
    description = directive.get("description", "")
    if description:
        return f"Execute this directive: {description}"
    return "Execute the directive."


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

    tool_input = tool_call["input"]
    if isinstance(tool_input, dict):
        params = tool_input
    elif isinstance(tool_input, str):
        try:
            params = json.loads(tool_input)
        except (json.JSONDecodeError, TypeError):
            params = {"raw_input": tool_input}
    else:
        params = {}

    executor = PrimitiveExecutor(project_path=project_path)
    result = await executor.execute(
        item_id=item_id,
        parameters=params,
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
    directive_name: str = "",
    thread_id: str = "",
    transcript: Optional[Any] = None,
) -> Dict[str, Any]:
    """Run the LLM tool-use loop with rich transcript events."""
    formatted_tools = _format_tool_defs(tool_defs, provider_config) if tool_defs else []

    messages = [{"role": "user", "content": user_prompt}]
    all_tool_results = []
    final_text = ""
    
    # Emit user message event
    if transcript and thread_id:
        try:
            transcript.write_event(thread_id, "user_message", {
                "directive": directive_name,
                "text": user_prompt,
                "role": "user",
            })
        except Exception as e:
            logger.warning(f"Failed to write user_message event: {e}")

    for turn in range(MAX_TOOL_ROUNDTRIPS):
        # Emit step_start event
        if transcript and thread_id:
            try:
                transcript.write_event(thread_id, "step_start", {
                    "directive": directive_name,
                    "turn_number": turn + 1,
                })
            except Exception as e:
                logger.warning(f"Failed to write step_start event: {e}")
        
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

        # Emit assistant_text event
        if transcript and thread_id and parsed.get("text"):
            try:
                transcript.write_event(thread_id, "assistant_text", {
                    "directive": directive_name,
                    "text": parsed["text"],
                })
            except Exception as e:
                logger.warning(f"Failed to write assistant_text event: {e}")
        
        # Emit assistant_reasoning if available
        if transcript and thread_id and parsed.get("reasoning"):
            try:
                transcript.write_event(thread_id, "assistant_reasoning", {
                    "directive": directive_name,
                    "text": parsed["reasoning"],
                })
            except Exception as e:
                logger.warning(f"Failed to write assistant_reasoning event: {e}")

        limit_event = harness.check_limits()
        if limit_event:
            harness.evaluate_hooks(limit_event)

        messages.append({"role": "assistant", "content": parsed["content_blocks"]})

        if not parsed["has_tool_use"] or not parsed["tool_calls"]:
            final_text = parsed["text"]
            
            # Emit step_finish event
            if transcript and thread_id:
                try:
                    step_cost = harness.cost.per_turn[-1]["spend"] if harness.cost.per_turn else 0
                    step_tokens = (harness.cost.per_turn[-1].get("prompt_tokens", 0) + 
                                 harness.cost.per_turn[-1].get("completion_tokens", 0)) if harness.cost.per_turn else 0
                    transcript.write_event(thread_id, "step_finish", {
                        "directive": directive_name,
                        "cost": step_cost,
                        "tokens": step_tokens,
                        "finish_reason": "end_turn",
                    })
                except Exception as e:
                    logger.warning(f"Failed to write step_finish event: {e}")
            break

        # Emit tool_call events and execute
        tool_results = []
        for tc in parsed["tool_calls"]:
            call_id = tc.get("id", "")
            
            # Emit tool_call_start
            if transcript and thread_id:
                try:
                    transcript.write_event(thread_id, "tool_call_start", {
                        "directive": directive_name,
                        "tool": tc["name"],
                        "call_id": call_id,
                        "input": tc["input"],
                    })
                except Exception as e:
                    logger.warning(f"Failed to write tool_call_start event: {e}")
            
            import time
            start_time = time.time()
            tr = await _execute_tool_call(tc, tool_map, project_path)
            duration_ms = int((time.time() - start_time) * 1000)
            
            # Emit tool_call_result
            if transcript and thread_id:
                try:
                    error_str = tr.get("error") if tr.get("is_error") else None
                    transcript.write_event(thread_id, "tool_call_result", {
                        "directive": directive_name,
                        "call_id": call_id,
                        "output": str(tr.get("result", ""))[:1000],  # Truncate long outputs
                        "error": error_str,
                        "duration_ms": duration_ms,
                    })
                except Exception as e:
                    logger.warning(f"Failed to write tool_call_result event: {e}")
            
            tool_results.append(tr)
            all_tool_results.append({
                "tool": tc["name"],
                "input": tc["input"],
                "result": tr["result"],
                "is_error": tr.get("is_error", False),
            })

        # Emit step_finish event
        if transcript and thread_id:
            try:
                step_cost = harness.cost.per_turn[-1]["spend"] if harness.cost.per_turn else 0
                step_tokens = (harness.cost.per_turn[-1].get("prompt_tokens", 0) + 
                             harness.cost.per_turn[-1].get("completion_tokens", 0)) if harness.cost.per_turn else 0
                transcript.write_event(thread_id, "step_finish", {
                    "directive": directive_name,
                    "cost": step_cost,
                    "tokens": step_tokens,
                    "finish_reason": "tool_use",
                })
            except Exception as e:
                logger.warning(f"Failed to write step_finish event: {e}")
        
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
    
    # Thread creation timestamp (ISO 8601)
    thread_created_at = datetime.now(timezone.utc).isoformat()

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
        "thread_mode": "single",
    })
    
    # Write thread.json with initial metadata (status: running)
    try:
        thread_dir = project_path / ".ai" / "threads"
        _write_thread_meta_atomic(
            thread_dir=thread_dir,
            thread_id=thread_id,
            directive_name=directive_name,
            status="running",
            created_at=thread_created_at,
            updated_at=thread_created_at,
            model=model_id,
        )
    except Exception as e:
        logger.warning(f"Failed to write thread.json: {e}")

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

    system_prompt = _build_system_prompt(
        directive, inputs, tool_defs, provider_config, directive_name
    )
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
        directive_name=directive_name,
        thread_id=thread_id,
        transcript=transcript,
    )

    if not llm_result["success"]:
        error_event = {"name": "error", "code": "llm_call_failed",
                       "detail": {"error": llm_result.get("error", "")}}
        hook_result = harness.evaluate_hooks(error_event)
        try:
            registry.update_status(thread_id, "error", {"usage": harness.cost.to_dict()})
            transcript.write_event(thread_id, "thread_error", {
                "directive": directive_name,
                "error_code": "llm_call_failed",
                "detail": llm_result.get("error", ""),
            })
            # Update thread.json with error status
            thread_dir = project_path / ".ai" / "threads"
            _write_thread_meta_atomic(
                thread_dir=thread_dir,
                thread_id=thread_id,
                directive_name=directive_name,
                status="error",
                created_at=thread_created_at,
                updated_at=datetime.now(timezone.utc).isoformat(),
                model=model_id,
                cost=harness.cost.to_dict(),
            )
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
            "directive": directive_name,
            "usage": llm_result["usage"],
            "cost": harness.cost.to_dict(),
            "tool_results_count": len(llm_result.get("tool_results", [])),
        })
        # Update thread.json with completed status
        thread_dir = project_path / ".ai" / "threads"
        _write_thread_meta_atomic(
            thread_dir=thread_dir,
            thread_id=thread_id,
            directive_name=directive_name,
            status="completed",
            created_at=thread_created_at,
            updated_at=datetime.now(timezone.utc).isoformat(),
            model=model_id,
            cost=harness.cost.to_dict(),
        )
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
