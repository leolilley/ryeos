# rye:signed:2026-04-06T04:14:25Z:00ba8d755a621a68dad0a2dbc219f73d28b1ffc7332b48391da3aef48eefb5f2:IgG2WejvqKx3br52DH26FpF-96_nS_syIEeLEILcErLRwLC6dn0Vc2ewwmFaP7tKFn1qNids2GRTNHnGNevvDw:4b987fd4e40303ac
__version__ = "2.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/agent/threads"
__tool_description__ = "Execute a directive in a managed thread with LLM loop"
__execution_owner__ = "callee"
__native_async__ = True
__native_resume__ = True

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
            item_type="directive",
            item_id=parent_id,
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

    loader = CapRiskLoader()
    try:
        config = loader.load(project_path)
    except Exception:
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

    # 1. Resolve parent context — explicit param > env var > no parent
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

    # 2. Register thread
    thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)
    registry = thread_registry.get_registry(proj_path)
    if not pre_registered:
        registry.register(thread_id, directive_name, parent_id=parent_thread_id)

    # 3. Load and parse directive
    from rye.utils.resolvers import get_user_space
    user_space = str(get_user_space())

    if params.get("resume_messages"):
        # Handoff/resume: use resolve_item (no input validation) then parse manually
        from rye.actions._resolve import resolve_item
        from rye.utils.parser_router import ParserRouter
        result = await resolve_item(
            user_space,
            item_type="directive",
            item_id=directive_name,
            project_path=project_path,
        )
        if result["status"] != "success":
            registry.update_status(thread_id, "error")
            return result
        directive = ParserRouter().parse("markdown_xml", result["content"])
    else:
        # Data-driven: parse via ParserRouter, validate+interpolate via ProcessorRouter
        from rye.utils.parser_router import ParserRouter
        from rye.utils.processor_router import ProcessorRouter
        from rye.actions.execute import ExecuteTool

        exec_tool = ExecuteTool(user_space=user_space)
        file_path = exec_tool._find_item(project_path, "directive", directive_name)
        if not file_path:
            registry.update_status(thread_id, "error")
            return {"success": False, "error": f"Directive not found: {directive_name}", "thread_id": thread_id}

        content = file_path.read_text(encoding="utf-8")
        directive = ParserRouter().parse("markdown/xml", content)
        if "error" in directive:
            registry.update_status(thread_id, "error")
            return {"success": False, "error": directive.get("error", "Directive parse failed"), "thread_id": thread_id}

        proj_path_obj = Path(project_path) if project_path else None
        processor_router = ProcessorRouter(proj_path_obj)
        validation = processor_router.run("inputs/validate", directive, inputs)
        if validation.get("status") == "error":
            registry.update_status(thread_id, "error")
            return {"success": False, "error": validation.get("error", "Directive validation failed"), "thread_id": thread_id}
        processor_router.run("inputs/interpolate", directive, validation["inputs"])

    # 4. Resolve extends — fire resolve_extends hooks to route into extends chains
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

    # 5. Resolve extends chain and compose context
    system_prompt = ""
    directive_context = {"before": "", "after": "", "suppress": []}
    if directive.get("extends") or directive.get("context"):
        try:
            chain_result = await _resolve_directive_chain(
                directive_name, directive, project_path
            )
            # Execute knowledge items for all context positions
            # (execute parses frontmatter and returns body only, unlike load
            # which returns raw content with YAML metadata and signatures)
            from rye.actions.execute import ExecuteTool
            from rye.utils.resolvers import get_user_space
            exec_tool = ExecuteTool(user_space=str(get_user_space()))
            suppressed = set(chain_result["context"].get("suppress", []))
            for position in ("system", "before", "after"):
                parts = []
                for kid in chain_result["context"].get(position, []):
                    if kid in suppressed:
                        continue
                    kr = await exec_tool.handle(
                        item_type="knowledge", item_id=kid, project_path=project_path,
                    )
                    if kr.get("status") != "success":
                        error = kr.get("error", "unknown error")
                        # Integrity failures must abort — untrusted/tampered
                        # context must never be silently injected or skipped.
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
                    data = kr.get("data", {})
                    content = data.get("body", "") if isinstance(data, dict) else ""
                    if content:
                        parts.append(content.strip())
                if parts:
                    if position == "system":
                        system_prompt = "\n\n".join(parts)
                    else:
                        directive_context[position] = "\n\n".join(parts)
            directive_context["suppress"] = list(suppressed)

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
            registry.update_status(thread_id, "error")
            return {"success": False, "error": str(e), "thread_id": thread_id}

    # 6. Reconstruct resume messages from previous thread transcript
    if params.get("previous_thread_id") and not params.get("resume_messages"):
        prev_tid = params["previous_thread_id"]

        # Verify transcript integrity before trusting JSONL content
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

        # Resolve continuation directive — per-directive override or system default
        cont_directive_id = directive.get("continuation_directive", "rye/agent/continuation")
        cont_message = continuation_message or (
            "Pick up where the previous thread left off. "
            "Continue executing the directive's instructions."
        )

        # Parse continuation directive (just parse + interpolate, don't spawn a thread)
        from rye.utils.parser_router import ParserRouter
        from rye.utils.processor_router import ProcessorRouter
        from rye.actions.execute import ExecuteTool
        cont_exec_tool = ExecuteTool(user_space=user_space)
        cont_file = cont_exec_tool._find_item(project_path, "directive", cont_directive_id)
        cont_prompt = cont_message
        if cont_file:
            cont_content = cont_file.read_text(encoding="utf-8")
            cont_parsed = ParserRouter().parse("markdown/xml", cont_content)
            if "error" not in cont_parsed:
                cont_params = {
                    "original_directive": directive_name,
                    "original_directive_body": directive.get("body", ""),
                    "previous_thread_id": prev_tid,
                    "continuation_message": cont_message,
                }
                proj_path_obj = Path(project_path) if project_path else None
                cont_processor = ProcessorRouter(proj_path_obj)
                cont_validation = cont_processor.run("inputs/validate", cont_parsed, cont_params)
                if cont_validation.get("status") != "error":
                    cont_processor.run("inputs/interpolate", cont_parsed, cont_validation["inputs"])
                    cont_prompt = cont_parsed.get("body", cont_message)

        trailing.append({"role": "user", "content": cont_prompt})

        params["resume_messages"] = trailing

    # 7. Build limits
    parent_limits = parent_meta.get("limits") if parent_meta else None
    limits = _resolve_limits(
        directive.get("limits", {}), params.get("limit_overrides", {}),
        project_path, parent_limits=parent_limits,
    )

    # 8. Check depth limit
    if limits.get("depth", 10) < 0:
        registry.update_status(thread_id, "error")
        return {
            "success": False,
            "error": f"Depth limit exhausted (resolved depth={limits['depth']})",
            "thread_id": thread_id,
        }

    # 9. Check spawns limit
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

    # 10. Build hooks, harness
    hooks = _merge_hooks(directive.get("hooks", []), project_path)

    SafetyHarness = load_module("safety_harness", anchor=_ANCHOR).SafetyHarness
    permissions = directive.get("permissions", [])
    parent_capabilities = parent_meta.get("capabilities", []) if parent_meta else []
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
        registry.update_status(thread_id, "error")
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
        registry.update_status(thread_id, "error")
        return {
            "success": False,
            "error": (
                f"No tool schemas found in {_PRIMARY_ACTIONS_DIR}. "
                "Thread cannot execute without tools."
            ),
            "thread_id": thread_id,
        }

    # 11. Reserve budget
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
    try:
        resolved_model, provider_item_id, provider_config = provider_resolver.resolve_provider(
            model, project_path=proj_path, provider=provider_hint
        )
    except Exception as e:
        registry.update_status(thread_id, "error")
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

    # 12. Write initial thread.json
    registry.update_status(thread_id, "running")
    _write_thread_meta(
        proj_path, thread_id, directive_name, "running",
        thread_created_at, thread_created_at, model=resolved_model,
        limits=limits, capabilities=harness._capabilities,
    )

    # 13. Async spawn — child re-enters execute() and runs through the sync path.
    # Parent thread ID is forwarded explicitly in child_params so the child
    # discovers its actual parent, not itself (RYE_PARENT_THREAD_ID env is
    # only set for the sync path below, where in-process grandchildren need it).
    if params.get("async"):
        child_params = {"directive_id": directive_name, "inputs": inputs}
        if params.get("model"):
            child_params["model"] = params["model"]
        if params.get("limit_overrides"):
            child_params["limit_overrides"] = params["limit_overrides"]
        if params.get("previous_thread_id"):
            child_params["previous_thread_id"] = params["previous_thread_id"]
        # Forward resolved parent (explicit param or env) so the child
        # doesn't resolve itself as its own parent.
        if parent_thread_id:
            child_params["parent_thread_id"] = parent_thread_id

        thread_dir = proj_path / AI_DIR / "agent" / "threads" / thread_id

        # Write params to thread dir for execution tracing
        params_json = json.dumps(child_params)
        trace_file = thread_dir / "spawn_params.json"
        trace_file.parent.mkdir(parents=True, exist_ok=True)
        with open(trace_file, "w") as f:
            f.write(params_json)

        from rye.utils.detached import launch_detached
        spawn_result = await launch_detached(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "--project-path", str(proj_path),
                "--thread-id", thread_id,
                "--pre-registered",
            ],
            thread_id=thread_id,
            log_dir=thread_dir,
            input_data=params_json,
        )

        if spawn_result.get("success"):
            registry.update_pid(thread_id, spawn_result["pid"])
        else:
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

    # 14. Set env var so in-process children discover this thread as parent
    # Only needed for sync execution — async children get parent via child_params.
    _prev_parent_env = os.environ.get("RYE_PARENT_THREAD_ID")
    os.environ["RYE_PARENT_THREAD_ID"] = thread_id

    # 15. Run thread synchronously
    # resume_messages is an internal param from handoff_thread — not in CONFIG_SCHEMA
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
            resume_messages=params.get("resume_messages"),
            directive_body=clean_directive_text,
            previous_thread_id=params.get("previous_thread_id"),
            inputs=inputs,
            system_prompt=system_prompt,
            directive_context=directive_context,
        )

        # Ensure non-empty error message on failure
        if not result.get("success") and not result.get("error"):
            result["error"] = result.get("status", "unknown error (no message from runner)")

        # 15. Report spend and finalize
        actual_spend = result.get("cost", {}).get("spend", 0.0)
        try:
            ledger.report_actual(thread_id, actual_spend)
        except Exception:
            pass  # overspend is logged but shouldn't block finalization
        if parent_thread_id:
            ledger.cascade_spend(thread_id, parent_thread_id, actual_spend)
        status = result.get("status", "completed")
        ledger.release(thread_id, final_status=status)

        # Update registry with final status
        status = result.get("status", "completed")
        registry.update_status(thread_id, status)
        result_data = {"cost": result.get("cost")}
        if result.get("outputs"):
            result_data["outputs"] = result["outputs"]
        registry.set_result(thread_id, result_data)

        # Write final thread.json
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
    finally:
        # Restore previous env to prevent leaking into subsequent top-level spawns
        if _prev_parent_env is not None:
            os.environ["RYE_PARENT_THREAD_ID"] = _prev_parent_env
        else:
            os.environ.pop("RYE_PARENT_THREAD_ID", None)


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
