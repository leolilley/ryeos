# rye:signed:2026-04-19T09:49:53Z:6a035d9a16d99164347d3e10e3ffbc0e64d2ddac6d0b69caad8218c800fa8572:XoAaJnO/RP0bE0pfbUHW57fHhV8QDvWY/UFmcQHQLkcfHMiH7MaB2EBzmm6s4mRsj2a61rKCs96accE54gyFAg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__version__ = "2.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/agent/threads"
__tool_description__ = "Execute a directive in a managed thread with LLM loop"
__execution_owner__ = "callee"
__native_async__ = True
__native_resume__ = True
__allowed_threads__ = ["inline", "fork"]
__allowed_targets__ = ["local", "remote"]

import argparse
import asyncio
import json
import os
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import AI_DIR, STATE_THREADS_REL
from module_loader import load_module

_ANCHOR = Path(__file__).parent


def _find_tools_root() -> Path:
    """Walk up from __file__ to find the .ai/tools boundary for this bundle."""
    current = Path(__file__).resolve().parent
    while current != current.parent:
        if current.name == "tools" and current.parent.name == ".ai":
            return current
        current = current.parent
    raise RuntimeError(
        f"Cannot find .ai/tools root from {__file__} — "
        "thread_directive.py must live under a .ai/tools/ directory"
    )

_TOOLS_ROOT = _find_tools_root()


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
        "parent_thread_id": {"type": "string", "description": "Parent thread for hierarchy tracking"},
        "parent_limits": {
            "type": "object",
            "description": "Resolved parent limits for child attenuation (internal)",
        },
        "parent_capabilities": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Resolved parent capabilities for child attenuation (internal)",
        },
        "previous_thread_id": {"type": "string", "description": "Previous thread for resume/continuation"},
        "resume_messages": {"type": "array", "description": "Messages to resume from (internal)"},
    },
    "required": ["directive_id"],
    "additionalProperties": False,
}


_PRIMARY_ACTION_NAMES = ("rye_execute", "rye_fetch", "rye_sign")


def _build_primary_actions() -> list:
    """Load primary action schemas for passing to tool_schema_loader.

    Primary actions use variable refs in CONFIG_SCHEMA (imported from
    primary_action_descriptions.py) that the AST parser can't evaluate,
    so we load them via load_module to get the resolved schemas.
    """
    schemas = []
    for name in _PRIMARY_ACTION_NAMES:
        # Strip rye_ prefix to get the file stem under rye/
        file_stem = name[4:]  # rye_execute → execute
        py_file = _TOOLS_ROOT / "rye" / f"{file_stem}.py"
        if not py_file.exists():
            continue
        mod = load_module(py_file)
        config_schema = getattr(mod, "CONFIG_SCHEMA", None)
        desc = getattr(mod, "__tool_description__", "")
        category = getattr(mod, "__category__", "")
        if config_schema:
            item_id = f"{category}/{file_stem}" if category else file_stem
            schemas.append({
                "name": name,
                "description": desc,
                "schema": config_schema,
                "_item_id": item_id,
            })
    return schemas



def _generate_thread_id(directive_name: str) -> str:
    from rye.utils.detached import generate_thread_id
    return generate_thread_id(directive_name)


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

    In v3 this is a derived export only. It is not runtime authority for
    parent lookup, status inspection, or continuation.
    """
    thread_dir = project_path / AI_DIR / STATE_THREADS_REL / thread_id
    thread_dir.mkdir(parents=True, exist_ok=True)

    meta = {
        "thread_id": thread_id,
        "directive": directive_name,
        "tool_id": "rye/agent/threads/thread_directive",
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


def _build_prompt(directive: Dict) -> str:
    """Build the LLM prompt from the directive content.

    Only sends what the LLM needs to execute the directive:
      1. Directive name + description
      2. Permissions (raw XML from directive metadata)
      3. Body (process steps with resolved input values)
      4. Returns section (from <outputs>)

    Note: DirectiveInstruction is primarily injected via thread_started
    context hook (see hook_conditions.yaml ctx_directive_instruction).
    The hardcoded backup lives in constants.DIRECTIVE_INSTRUCTION and is
    returned via execute.py's your_directions for in-thread mode.
    """
    import re as _re
    parts = []

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
            field_lines = "\n".join(f'  "{k}": "<{v or k}>"' for k, v in output_fields.items())
            parts.append(
                "When you have completed all steps, call the `directive_return` tool "
                "via the tool_use API with these fields:\n"
                f"{{{field_lines}\n}}\n\n"
                "If you are BLOCKED and cannot complete the directive, call "
                "`directive_return` with `status` set to `error` and `error_detail` "
                "describing what is missing or broken. Do NOT output directive_return "
                "as text — it MUST be a tool_use call."
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
    user = loader.get_user_hooks()
    builtin = loader.get_builtin_hooks(Path(project_path))
    context = loader.get_context_hooks(Path(project_path))
    project = loader.get_project_hooks(Path(project_path))
    infra = loader.get_infra_hooks(Path(project_path))

    for h in user:
        h.setdefault("layer", 0)
    for h in directive_hooks:
        h.setdefault("layer", 1)
    for h in builtin:
        h.setdefault("layer", 2)
    for h in context:
        h.setdefault("layer", 2)
    for h in project:
        h.setdefault("layer", 3)
    for h in infra:
        h.setdefault("layer", 4)

    return sorted(user + directive_hooks + builtin + context + project + infra, key=lambda h: h.get("layer", 2))


async def _resolve_directive_chain(
    directive_name: str,
    directive: Dict,
    project_path: str,
) -> Dict:
    """Walk the extends chain and compose context + capabilities.

    Resolution order: leaf → parent → ... → root.
    Context is collected root-first (base layers, then overlays).
    Capabilities are collected from the leaf (most restrictive).

    Returns:
        {
            "context": {"system": [], "before": [], "after": []},
            "chain": [root_name, ..., leaf_name],
        }
    """
    from rye.actions._resolve import resolve_item
    from rye.utils.parser_router import ParserRouter
    from rye.utils.resolvers import get_user_space

    chain = [directive]
    chain_names = [directive.get("name", directive_name)]
    seen = {directive_name}
    current = directive

    while current.get("extends"):
        parent_id = current["extends"]
        if parent_id in seen:
            raise ValueError(
                f"Circular extends chain: {parent_id} "
                f"(chain: {' → '.join(chain_names)})"
            )
        seen.add(parent_id)

        result = await resolve_item(
            str(get_user_space()),
            item_ref=f"directive:{parent_id}",
            project_path=project_path,
        )
        if result["status"] != "success":
            raise ValueError(
                f"Failed to load parent directive '{parent_id}': "
                f"{result.get('error', 'unknown error')}"
            )
        parent = ParserRouter().parse("markdown/xml", result["content"])
        chain.append(parent)
        chain_names.append(parent.get("name", parent_id))
        current = parent

    # Reverse: root first for context composition
    chain.reverse()
    chain_names.reverse()

    # Compose context: root layers first, then overlays
    context = {"system": [], "before": [], "after": [], "suppress": []}
    for d in chain:
        d_ctx = d.get("context", {})
        for position in ("system", "before", "after"):
            items = d_ctx.get(position, [])
            if isinstance(items, str):
                items = [items]
            for item in items:
                if item not in context[position]:
                    context[position].append(item)
        for s in d_ctx.get("suppress", []):
            if s not in context["suppress"]:
                context["suppress"].append(s)

    return {"context": context, "chain": chain_names, "chain_directives": chain}


def _assess_capability_risk(
    capabilities: list,
    acknowledged_risks: list,
    thread_id: str,
    project_path: Path,
) -> Optional[Dict]:
    """Check granted capabilities against risk classifications.

    Uses most-specific-first matching: for each capability, the classification
    with the longest matching pattern wins. This prevents broad patterns like
    "rye.*" from overriding specific ones like "rye.search.*".

    Returns an error dict if a blocked risk is detected, else None.
    Logs warnings for elevated capabilities.
    """
    import fnmatch as _fnmatch

    risk_loader = load_module("loaders/config_loader", anchor=_ANCHOR)

    class CapRiskLoader(risk_loader.ConfigLoader):
        def __init__(self):
            super().__init__("capability_risk.yaml")

    from rye.utils.integrity import IntegrityError

    loader = CapRiskLoader()
    try:
        config = loader.load(project_path)
    except IntegrityError:
        raise
    except Exception:
        import logging
        logging.getLogger(__name__).warning(
            "Failed to load capability_risk.yaml", exc_info=True,
        )
        return None

    risk_levels = config.get("risk_levels", {})
    classifications = config.get("classifications", [])
    ack_set = {a.get("risk", "") for a in (acknowledged_risks or [])}

    for cap in capabilities:
        # Find the most specific matching classification (longest pattern wins)
        best_match = None
        best_specificity = -1
        for classification in classifications:
            for pattern in classification.get("patterns", []):
                if _fnmatch.fnmatch(cap, pattern):
                    specificity = pattern.count(".")
                    if specificity > best_specificity:
                        best_specificity = specificity
                        best_match = classification

        if best_match is None:
            continue

        risk = best_match.get("risk", "safe")
        level_config = risk_levels.get(risk, {})
        policy = level_config.get("policy", "allow")

        if policy == "block" and risk not in ack_set:
            return {
                "error": (
                    f"Capability '{cap}' classified as '{risk}' "
                    f"({best_match.get('description', '')}). "
                    f"Add <acknowledge risk=\"{risk}\"> to the directive's "
                    f"<permissions> to explicitly allow this."
                ),
                "risk": risk,
                "capability": cap,
            }

        if policy == "acknowledge_required" and risk not in ack_set:
            import logging
            logging.getLogger(__name__).warning(
                "Thread %s: capability '%s' classified as '%s' — %s. "
                "Consider adding <acknowledge risk=\"%s\"> to the directive.",
                thread_id, cap, risk,
                best_match.get("description", ""),
                risk,
            )

    return None


async def execute(params: Dict, project_path: str) -> Dict:
    # Pop internal-only params before validation (set by subprocess spawn path)
    thread_id_override = params.pop("_thread_id", None)
    params.pop("_pre_registered", False)
    continuation_message = params.pop("_continuation_message", None)

    # Pop execution config params that may leak from graph/config layer
    params.pop("max_steps", None)
    params.pop("max_concurrency", None)
    params.pop("timeout", None)

    allowed = set(CONFIG_SCHEMA["properties"].keys())
    unknown = set(params.keys()) - allowed
    if unknown:
        raise ValueError(f"Unknown parameters: {unknown}. Valid: {allowed}")

    directive_name = params["directive_id"]
    thread_id = thread_id_override or _generate_thread_id(directive_name)
    inputs = params.get("inputs", {})
    thread_created_at = datetime.now(timezone.utc).isoformat()
    proj_path = Path(project_path)

    # 1. Resolve parent context from explicit internal params only.
    # thread.json is a derived export in v3 and must not be consulted.
    parent_thread_id = params.get("parent_thread_id")
    parent_limits = params.get("parent_limits")
    parent_capabilities = params.get("parent_capabilities") or []
    if parent_thread_id and parent_limits is None:
        return {
            "success": False,
            "error": (
                "parent_thread_id requires explicit parent_limits on the v3 path; "
                "thread.json parent lookup has been removed"
            ),
            "thread_id": thread_id,
        }

    if params.get("previous_thread_id") or params.get("resume_messages") or continuation_message:
        return {
            "success": False,
            "error": (
                "thread_directive continuation inputs are disabled until ryeosd owns "
                "successor creation and continued edges"
            ),
            "thread_id": thread_id,
        }

    # 2. Load and parse directive
    from rye.utils.resolvers import get_user_space
    user_space = str(get_user_space())

    # Data-driven: parse via ParserRouter, validate+interpolate via ProcessorRouter
    from rye.utils.parser_router import ParserRouter
    from rye.utils.processor_router import ProcessorRouter
    from rye.actions.execute import ExecuteTool

    exec_tool = ExecuteTool(user_space=user_space)
    file_path = exec_tool._find_item(project_path, "directive", directive_name)
    if not file_path:
        return {"success": False, "error": f"Directive not found: {directive_name}", "thread_id": thread_id}

    content = file_path.read_text(encoding="utf-8")
    directive = ParserRouter().parse("markdown/xml", content)
    if "error" in directive:
        return {"success": False, "error": directive.get("error", "Directive parse failed"), "thread_id": thread_id}

    proj_path_obj = Path(project_path) if project_path else None
    processor_router = ProcessorRouter(proj_path_obj)
    validation = processor_router.run("inputs/validate", directive, inputs)
    if validation.get("status") == "error":
        return {"success": False, "error": validation.get("error", "Directive validation failed"), "thread_id": thread_id}
    processor_router.run("inputs/interpolate", directive, validation["inputs"])

    # 3. Resolve extends — fire resolve_extends hooks to route into extends chains
    hooks_loader = load_module("loaders/hooks_loader", anchor=_ANCHOR)
    loader = hooks_loader.get_hooks_loader()
    resolve_hooks = (
        loader.get_project_hooks(proj_path)
        + loader.get_user_hooks()
        + loader.get_builtin_hooks(proj_path)
    )
    condition_eval = load_module("loaders/condition_evaluator", anchor=_ANCHOR)
    hook_ctx = {
        "directive": directive_name,
        "has_extends": bool(directive.get("extends")),
        "category": directive.get("category", ""),
        "inputs": inputs,
        "model": directive.get("model", {}),
    }
    for hook in resolve_hooks:
        if hook.get("event") != "resolve_extends":
            continue
        if not condition_eval.matches(hook_ctx, hook.get("condition", {})):
            continue
        action = hook.get("action", {})
        extends_target = action.get("set_extends")
        if extends_target:
            original_extends = directive.get("extends")
            directive["extends"] = extends_target
            import logging
            if original_extends:
                logging.getLogger(__name__).info(
                    "Thread %s: resolve_extends hook '%s' overrode extends "
                    "'%s' → '%s'",
                    thread_id, hook.get("id", "?"),
                    original_extends, extends_target,
                )
            else:
                logging.getLogger(__name__).info(
                    "Thread %s: resolve_extends hook '%s' set extends → '%s'",
                    thread_id, hook.get("id", "?"), extends_target,
                )
            break  # first match wins

    # 5. Resolve extends chain — collect raw context IDs for deferred materialization
    chain_context = {"system": [], "before": [], "after": [], "suppress": []}
    if directive.get("extends") or directive.get("context"):
        try:
            chain_result = await _resolve_directive_chain(
                directive_name, directive, project_path
            )
            chain_context = chain_result["context"]

            # Merge parent capabilities from extends chain.
            # The root directive provides the broadest capabilities;
            # each child narrows via the existing SafetyHarness attenuation.
            # If the leaf has no permissions, inherit from the nearest parent.
            chain_dirs = chain_result.get("chain_directives", [])
            if not directive.get("permissions") and len(chain_dirs) > 1:
                for parent_d in chain_dirs[:-1]:  # root → ... → parent (exclude leaf)
                    if parent_d.get("permissions"):
                        directive["permissions"] = parent_d["permissions"]
                        break
        except ValueError as e:
            return {"success": False, "error": str(e), "thread_id": thread_id}

    # 5. Build limits
    limits = _resolve_limits(
        directive.get("limits", {}), params.get("limit_overrides", {}),
        project_path, parent_limits=parent_limits,
    )

    # 6. Check depth limit
    if limits.get("depth", 10) < 0:
        return {
            "success": False,
            "error": f"Depth limit exhausted (resolved depth={limits['depth']})",
            "thread_id": thread_id,
        }

    # 7. Check spawns limit
    orchestrator_mod = load_module("orchestrator", anchor=_ANCHOR)
    if parent_thread_id:
        spawns_limit = parent_limits.get("spawns", 10) if parent_limits else limits.get("spawns", 10)
        spawn_exceeded = orchestrator_mod.check_spawn_limit(parent_thread_id, spawns_limit)
        if spawn_exceeded:
            return {
                "success": False,
                "error": f"Spawn limit exceeded for parent {parent_thread_id}: {spawn_exceeded['current_value']}/{spawn_exceeded['current_max']}",
                "thread_id": thread_id,
            }
        orchestrator_mod.increment_spawn_count(parent_thread_id)

    # 8. Build hooks, harness, preload tool schemas
    hooks = _merge_hooks(directive.get("hooks", []), project_path)

    SafetyHarness = load_module("safety_harness", anchor=_ANCHOR).SafetyHarness
    permissions = directive.get("permissions", [])
    harness = SafetyHarness(
        thread_id, limits, hooks, proj_path,
        directive_name=directive_name, permissions=permissions,
        parent_capabilities=parent_capabilities,
    )
    # Dynamic tool registration — resolve all granted tools as API-level
    # tool definitions so the LLM calls them directly (no rye_execute wrapper).
    resilience_loader = load_module("loaders/resilience_loader", anchor=_ANCHOR)
    preload_config = resilience_loader.get_resilience_loader().get_tool_preload_config(proj_path)
    tool_schema_loader = load_module("loaders/tool_schema_loader", anchor=_ANCHOR)
    primary_actions = _build_primary_actions()
    preload_result = tool_schema_loader.preload_tool_schemas(
        harness._capabilities, proj_path,
        max_tokens=preload_config.get("max_tokens", 2000),
        primary_actions=primary_actions,
    ) if preload_config.get("enabled", True) else {"tool_defs": [], "capabilities_summary": []}
    harness.available_tools = preload_result["tool_defs"]
    harness.capabilities_tree = preload_result.get("capabilities_tree", "")

    # 9. Materialize context — execute knowledge items declared by the
    # extends chain.  Deferred to after tool preloading so all capability
    # information is available; the harness just processes whatever context
    # the directive declares without injecting framework-specific docs.
    system_prompt = ""
    runtime = directive.get("runtime", {})
    directive_context = {"before": "", "after": "", "suppress": [], "runtime": runtime}
    if any(chain_context.get(pos) for pos in ("system", "before", "after")):
        from rye.actions.execute import ExecuteTool
        exec_tool = ExecuteTool(user_space=user_space)
        suppressed = set(chain_context.get("suppress", []))
        for position in ("system", "before", "after"):
            parts = []
            for kid in chain_context.get(position, []):
                if kid in suppressed:
                    continue
                kr = await exec_tool.handle(
                    item_id=f"knowledge:{kid}", project_path=project_path,
                )
                if kr.get("status") != "success":
                    error = kr.get("error", "unknown error")
                    if "integrity" in error.lower() or "untrusted" in error.lower():
                        raise ValueError(
                            f"Context knowledge '{kid}' (position={position}) "
                            f"integrity failure: {error}"
                        )
                    import logging
                    logging.getLogger(__name__).warning(
                        "Context knowledge '%s' (position=%s) failed: %s",
                        kid, position, error,
                    )
                    continue
                content = kr.get("content", "")
                if content:
                    parts.append(content.strip())
            if parts:
                if position == "system":
                    system_prompt = "\n\n".join(parts)
                else:
                    directive_context[position] = "\n\n".join(parts)
        directive_context["suppress"] = list(suppressed)

    # Grant directive_return as a direct tool if directive declares <outputs>
    directive_outputs = directive.get("outputs", [])
    if directive_outputs:
        harness._capabilities.append("rye.execute.tool.rye.agent.threads.directive_return")
        harness.has_outputs = True
        if isinstance(directive_outputs, list):
            harness.output_fields = [
                o["name"] for o in directive_outputs
                if o.get("name") and o.get("required")
            ]
        elif isinstance(directive_outputs, dict):
            harness.output_fields = list(directive_outputs.keys())

        # Build schema from output fields so LLM calls it directly
        props = {}
        required = []
        for o in (directive_outputs if isinstance(directive_outputs, list) else []):
            name = o.get("name", "")
            if not name:
                continue
            props[name] = {"type": o.get("type", "string"), "description": o.get("description", "")}
            if o.get("required"):
                required.append(name)
        if not props:
            props = {"result": {"type": "string", "description": "Result output"}}
        harness.available_tools.append({
            "name": "directive_return",
            "description": "Return structured results when the directive is complete",
            "schema": {"type": "object", "properties": props, "required": required},
            "_item_id": "rye/agent/threads/directive_return",
            "_primary": "execute",
        })

    # Assess capability risk
    acknowledged_risks = directive.get("acknowledged_risks", [])
    risk_result = _assess_capability_risk(
        harness._capabilities, acknowledged_risks, thread_id, proj_path
    )
    if risk_result:
        return {
            "success": False,
            "error": risk_result["error"],
            "thread_id": thread_id,
            "risk": risk_result.get("risk"),
        }

    # Broad capability warning
    broad_caps = [c for c in harness._capabilities if c.endswith(".*") and c.count(".") <= 2]
    if broad_caps:
        import logging
        logging.getLogger(__name__).warning(
            "Thread %s has broad capabilities: %s — consider narrowing permissions",
            thread_id, broad_caps,
        )

    if not harness.available_tools:
        return {
            "success": False,
            "error": (
                f"No tool schemas found in {_PRIMARY_ACTIONS_DIR}. "
                "Thread cannot execute without tools."
            ),
            "thread_id": thread_id,
        }

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
    try:
        resolved_model, provider_item_id, provider_config = provider_resolver.resolve_provider(
            model, project_path=proj_path, provider=provider_hint
        )
    except Exception as e:
        return {
            "success": False,
            "error": (
                f"Model resolution failed for directive '{directive_name}': "
                f"requested model/tier '{model}'"
                + (f" (provider: {provider_hint})" if provider_hint else "")
                + f". {e}"
            ),
            "thread_id": thread_id,
        }
    # Resolve env_config from provider YAML (loads .env, resolves ${VAR} refs)
    env_config = provider_config.get("env_config")
    if env_config:
        from rye.runtime.env_resolver import EnvResolver
        resolver = EnvResolver(project_path=proj_path)
        resolved_env = resolver.resolve(env_config=env_config)
        # Inject resolved vars into os.environ so http_client can find them
        for key in (env_config.get("env") or {}):
            if key in resolved_env and resolved_env[key]:
                os.environ[key] = resolved_env[key]

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

    # 10. Write initial derived thread.json export.
    _write_thread_meta(
        proj_path, thread_id, directive_name, "running",
        thread_created_at, thread_created_at, model=resolved_model,
        limits=limits, capabilities=harness._capabilities,
    )

    if params.get("async"):
        return {
            "success": False,
            "error": "async directive threads are disabled until detached execution is daemon-owned",
            "thread_id": thread_id,
        }

    # 11. Run thread synchronously
    try:
        result = await runner.run(
            thread_id,
            user_prompt,
            harness,
            provider,
            dispatcher,
            emitter,
            transcript,
            proj_path,
            directive_body=clean_directive_text,
            inputs=inputs,
            system_prompt=system_prompt,
            directive_context=directive_context,
        )

        # Ensure non-empty error message on failure
        if not result.get("success") and not result.get("error"):
            result["error"] = result.get("status", "unknown error (no message from runner)")

        # 12. Write final derived thread.json export.
        status = result.get("status", "completed")
        _write_thread_meta(
            proj_path, thread_id, directive_name, status,
            thread_created_at, datetime.now(timezone.utc).isoformat(),
            model=resolved_model, cost=result.get("cost"),
            limits=limits, capabilities=harness._capabilities,
            outputs=result.get("outputs"),
        )

        # Write per-thread diagnostics file on error for debugging
        if not result.get("success") and os.environ.get("RYE_DEBUG"):
            diag_path = proj_path / AI_DIR / STATE_THREADS_REL / thread_id.replace("/", os.sep) / "diagnostics.json"
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
    finally:
        pass


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
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

    # Params come via stdin (script runtime or async spawn input_data)
    params = json.loads(sys.stdin.read())
    if args.thread_id:
        params["_thread_id"] = args.thread_id
    if args.pre_registered:
        params["_pre_registered"] = True

    try:
        result = asyncio.run(execute(params, args.project_path))
    except Exception as exc:
        result = {"success": False, "error": str(exc)}
    print(json.dumps(result))
