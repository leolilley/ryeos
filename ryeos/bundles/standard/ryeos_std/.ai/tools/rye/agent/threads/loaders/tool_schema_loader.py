# rye:signed:2026-03-04T02:00:49Z:f631b34d11347a78cdf0286aacd7ea3bcb4a7cff4cf39e8aa1dbc4e8e7f17408:v_W83YUKgf_SzTm0lGYOy-fOxRl6JrXVE36vVlJuw335oDJ5CjFQhfN5VyUllndt1dZ_kiN_C9Gd-NnRtcfPAg==:4b987fd4e40303ac
__version__ = "1.5.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Capability-driven context preload for thread system prompt"

import logging
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from rye.utils.extensions import get_tool_extensions
from rye.utils.parser_router import ParserRouter
from rye.utils.resolvers import ToolResolver

logger = logging.getLogger(__name__)

_PRIMARY_PREFIX = "rye/primary/"
_CHARS_PER_TOKEN = 4

# Capability prefixes → (primary_action, sub_type_or_none)
# Longer (more specific) prefixes MUST come before shorter ones so that
# e.g. "rye.search.directive.*" matches the directive entry, not the
# generic "rye.search." wildcard.  sub_type=None means "all types".
_CAP_PREFIXES = {
    "rye.execute.tool.": ("execute", "tool"),
    "rye.execute.directive.": ("execute", "directive"),
    "rye.execute.knowledge.": ("execute", "knowledge"),
    "rye.search.directive.": ("search", "directive"),
    "rye.search.tool.": ("search", "tool"),
    "rye.search.knowledge.": ("search", "knowledge"),
    "rye.search.": ("search", None),
    "rye.load.directive.": ("load", "directive"),
    "rye.load.tool.": ("load", "tool"),
    "rye.load.knowledge.": ("load", "knowledge"),
    "rye.load.": ("load", None),
    "rye.sign.directive.": ("sign", "directive"),
    "rye.sign.tool.": ("sign", "tool"),
    "rye.sign.knowledge.": ("sign", "knowledge"),
    "rye.sign.": ("sign", None),
}

# Map capability actions to primary tool API names
_ACTION_TO_PRIMARY_NAME = {
    "execute": "rye_execute",
    "search": "rye_search",
    "load": "rye_load",
    "sign": "rye_sign",
}


def _classify_capability(cap: str) -> Optional[dict]:
    """Classify a capability string into action, sub_type, and pattern.

    'rye.execute.tool.rye.file-system.read' → {action: 'execute', sub_type: 'tool', pattern: 'rye/file-system/read'}
    'rye.search.*'                          → {action: 'search', sub_type: None, pattern: '*'}
    'rye.load.knowledge.*'                  → {action: 'load', sub_type: None, pattern: 'knowledge.*'}
    """
    for prefix, (action, sub_type) in _CAP_PREFIXES.items():
        if cap.startswith(prefix):
            remainder = cap[len(prefix) :]
            if not remainder:
                continue
            pattern = remainder.replace(".", "/") if sub_type == "tool" else remainder
            return {"action": action, "sub_type": sub_type, "pattern": pattern}
    return None


def _pattern_specificity(pattern: str) -> int:
    """Score pattern specificity — higher = more specific."""
    if "*" not in pattern:
        return 1000
    parts = pattern.rstrip("/*").split("/")
    return len(parts)


# Extension → parser name mapping (mirrors tool_extractor.yaml parsers)
_EXT_PARSERS = {
    ".py": "python/ast",
    ".yaml": "yaml/yaml",
    ".yml": "yaml/yaml",
    ".js": "javascript/javascript",
    ".ts": "javascript/javascript",
    ".mjs": "javascript/javascript",
    ".cjs": "javascript/javascript",
}


def _extract_tool_metadata(file_path: Path, router: ParserRouter) -> Optional[dict]:
    """Extract schema and description from a tool file via ParserRouter.

    Uses the same parser infrastructure as the rest of Rye (python/ast,
    yaml/yaml, javascript/javascript) so any supported file type works.
    Returns {"schema": dict, "description": str} or None.
    """
    ext = file_path.suffix.lower()
    parser_name = _EXT_PARSERS.get(ext)
    if not parser_name:
        return None

    try:
        source = file_path.read_text()
        parsed = router.parse(parser_name, source)
        if parsed.get("error"):
            return None

        # Normalize keys: parsers use different conventions.
        # python/ast: CONFIG_SCHEMA, __tool_description__ at top level
        # yaml/yaml:  config_schema, description under "data" sub-dict
        schema = (
            parsed.get("CONFIG_SCHEMA")
            or parsed.get("config_schema")
            or (parsed.get("data", {}) or {}).get("config_schema")
        )
        description = (
            parsed.get("__tool_description__", "")
            or parsed.get("description", "")
            or (parsed.get("data", {}) or {}).get("description", "")
        )

        # YAML tools may use a parameters list instead of JSON Schema
        if not schema:
            params = (parsed.get("data", {}) or {}).get("parameters")
            if isinstance(params, list):
                props = {}
                required = []
                for p in params:
                    if not isinstance(p, dict):
                        continue
                    pname = p.get("name", "")
                    if not pname:
                        continue
                    props[pname] = {
                        "type": p.get("type", "string"),
                        "description": p.get("description", ""),
                    }
                    if p.get("required"):
                        required.append(pname)
                if props:
                    schema = {"type": "object", "properties": props, "required": required}

        if schema:
            return {"schema": schema, "description": description}
    except Exception:
        logger.debug("Failed to extract metadata from %s", file_path, exc_info=True)
    return None


def _tool_id_from_path(file_path: Path, search_dir: Path) -> str:
    """Derive the tool item_id from its file path relative to a search dir."""
    rel = file_path.relative_to(search_dir)
    return str(rel.with_suffix(""))


def _strip_xml_markup(text: str) -> str:
    """Strip XML/HTML tags and truncate to first sentence.

    Primary tool CONFIG_SCHEMA descriptions contain nested markup like
    <description>...</description><examples>...</examples><rules>...</rules>.
    Extract just the first sentence for compact capabilities output.
    """
    import re

    plain = re.sub(r"<[^>]+>", "", text).strip()
    # First sentence only — split on ". " or ".\n" but not on "e.g."
    for end in (". ", ".\n"):
        idx = plain.find(end)
        if idx > 0:
            return plain[: idx + 1]
    return plain


def _format_tool_signature(
    tool_id: str, metadata: dict, display_name: Optional[str] = None
) -> str:
    """Format a tool schema as a compact function signature.

    Output:
      read(path*, offset, limit) — Read file content

    display_name overrides the name shown (e.g. bare "read" instead of full id).
    * marks required params.
    """
    schema = metadata["schema"]
    desc = _strip_xml_markup(metadata.get("description", ""))
    props = schema.get("properties", {})
    required = set(schema.get("required", []))

    name = display_name or tool_id
    params = []
    for param_name in props:
        suffix = "*" if param_name in required else ""
        params.append(f"{param_name}{suffix}")
    sig = f"{name}({', '.join(params)})"
    if desc:
        sig += f" — {desc}"
    return sig


def _resolve_tools_for_pattern(
    pattern: str,
    search_paths: List[Tuple[Path, str]],
    extensions: List[str],
    seen: set,
    router: ParserRouter,
) -> List[dict]:
    """Find all tool files matching a namespace pattern across 3-tier space.

    Returns list of {"tool_id": str, "metadata": dict} for unseen tools.
    Uses first-match semantics (project > user > system).
    Skips primary tools (rye/primary/*) — those are handled separately
    via the primary_tools argument to preload_tool_schemas.
    """
    results = []
    resolved_ids = set()

    for search_dir, _space in search_paths:
        if pattern.endswith("/*"):
            glob_dir = search_dir / pattern[:-2]
            if not glob_dir.exists():
                continue
            for ext in extensions:
                for fp in glob_dir.rglob(f"*{ext}"):
                    if not fp.is_file():
                        continue
                    tool_id = _tool_id_from_path(fp, search_dir)
                    if tool_id in resolved_ids or tool_id in seen:
                        continue
                    if tool_id.startswith(_PRIMARY_PREFIX):
                        continue
                    resolved_ids.add(tool_id)
                    meta = _extract_tool_metadata(fp, router)
                    if meta:
                        results.append({"tool_id": tool_id, "metadata": meta})
        else:
            if pattern in resolved_ids or pattern in seen:
                continue
            if pattern.startswith(_PRIMARY_PREFIX):
                continue
            for ext in extensions:
                fp = search_dir / f"{pattern}{ext}"
                if fp.is_file():
                    resolved_ids.add(pattern)
                    meta = _extract_tool_metadata(fp, router)
                    if meta:
                        results.append({"tool_id": pattern, "metadata": meta})
                    break
            if pattern in resolved_ids:
                break

    return results


def preload_tool_schemas(
    capabilities: list,
    project_path: Path,
    max_tokens: int = 2000,
    primary_tools: Optional[List[dict]] = None,
) -> dict:
    """Build a <capabilities> block from resolved capability strings.

    Produces a flat list of <tool> elements covering:
      - Primary tools (rye_execute, rye_search, rye_load, rye_sign) when their
        corresponding action is granted — schemas come from primary_tools arg
        since primary tool CONFIG_SCHEMAs use variable refs that AST can't eval.
      - Non-primary tools (rye/file-system/*, etc.) resolved from execute.tool
        capability patterns via filesystem search.

    Returns:
        {"schemas": formatted_xml_string, "preloaded_tools": [tool_item_ids]}
    """
    # Step 1: Classify capabilities — collect tool patterns, granted actions,
    # and which item types each action can operate on.
    tool_patterns = []
    granted_actions: set = set()
    action_item_types: Dict[str, set] = {}
    _ALL_TYPES = {"directive", "tool", "knowledge"}

    for cap in capabilities:
        classified = _classify_capability(cap)
        if not classified:
            continue
        action = classified["action"]
        sub_type = classified["sub_type"]
        granted_actions.add(action)
        if sub_type:
            action_item_types.setdefault(action, set()).add(sub_type)
        else:
            # sub_type=None → wildcard (e.g. rye.search.*), grant all types
            action_item_types.setdefault(action, set()).update(_ALL_TYPES)
        if sub_type == "tool":
            tool_patterns.append(classified["pattern"])

    if not tool_patterns and not granted_actions:
        return {"schemas": "", "preloaded_tools": []}

    # Step 2: Select primary tools for granted actions.
    # Primary tool schemas are already loaded (via load_module) in
    # harness.available_tools — we just pick the ones whose action is granted.
    primary_entries: List[dict] = []
    if primary_tools:
        for action in ("execute", "search", "load", "sign"):
            if action not in granted_actions:
                continue
            name = _ACTION_TO_PRIMARY_NAME[action]
            for t in primary_tools:
                if t["name"] == name:
                    tool_id = t.get("_item_id", name)
                    primary_entries.append(
                        {
                            "tool_id": tool_id,
                            "metadata": {
                                "schema": t["schema"],
                                "description": t.get("description", ""),
                            },
                        }
                    )
                    break

    # Step 3: Resolve non-primary tool schemas for execute.tool patterns
    resolver = ToolResolver(project_path)
    search_paths = resolver.get_search_paths()
    extensions = get_tool_extensions(project_path)
    router = ParserRouter(project_path)
    tool_patterns.sort(key=_pattern_specificity, reverse=True)
    seen: set = {t["tool_id"] for t in primary_entries}
    non_primary_tools: List[dict] = []

    for pattern in tool_patterns:
        matches = _resolve_tools_for_pattern(pattern, search_paths, extensions, seen, router)
        for m in matches:
            seen.add(m["tool_id"])
        non_primary_tools.extend(matches)

    all_tools = primary_entries + non_primary_tools

    if not all_tools:
        return {"schemas": "", "preloaded_tools": []}

    # Step 4: Build compact <capabilities> within token budget.
    #
    # Each primary tool gets a sub-tree showing which item types it can
    # operate on (derived from granted capabilities).  For rye_execute's
    # "tool" type, the resolved namespace tree is nested underneath.
    # This makes the LLM see *only* what's available — if "directive"
    # isn't listed, it won't try item_type="directive".
    _TYPE_ORDER = ["directive", "tool", "knowledge"]
    max_chars = max_tokens * _CHARS_PER_TOKEN
    current_chars = len("<capabilities>\n</capabilities>")
    preloaded = []

    # Reverse map: api_name → action
    _NAME_TO_ACTION = {v: k for k, v in _ACTION_TO_PRIMARY_NAME.items()}

    # Pre-group non-primary tools by namespace for the execute tree
    from collections import OrderedDict

    tool_groups: OrderedDict[str, List[Tuple[str, dict]]] = OrderedDict()
    for entry in non_primary_tools:
        tool_id = entry["tool_id"]
        if "/" in tool_id:
            prefix = tool_id.rsplit("/", 1)[0]
            bare_name = tool_id.rsplit("/", 1)[1]
        else:
            prefix = ""
            bare_name = tool_id
        tool_groups.setdefault(prefix, []).append((bare_name, entry))

    blocks: List[str] = []

    for entry in primary_entries:
        tool_id = entry["tool_id"]
        api_name = tool_id.rsplit("/", 1)[-1] if "/" in tool_id else tool_id
        action = _NAME_TO_ACTION.get(api_name)
        accessible = action_item_types.get(action, set())
        types_in_order = [t for t in _TYPE_ORDER if t in accessible]

        sig = _format_tool_signature(tool_id, entry["metadata"], display_name=api_name)
        sig_chars = len(sig) + 1
        if current_chars + sig_chars > max_chars:
            continue

        sub_lines: List[str] = []

        for ti, item_type in enumerate(types_in_order):
            is_last_type = ti == len(types_in_order) - 1

            if action == "execute" and item_type == "tool" and non_primary_tools:
                # Resolved tool namespace tree under "tools:" header
                header = "  tools:"
                h_chars = len(header) + 1
                if (
                    current_chars
                    + sig_chars
                    + sum(len(l) + 1 for l in sub_lines)
                    + h_chars
                    > max_chars
                ):
                    break
                sub_lines.append(header)

                group_keys = list(tool_groups.keys())
                for gi, prefix in enumerate(group_keys):
                    tools = tool_groups[prefix]
                    is_last_group = gi == len(group_keys) - 1
                    if prefix:
                        branch = "  └─" if is_last_group else "  ├─"
                        gh = f"{branch} {prefix}:"
                        if (
                            current_chars
                            + sig_chars
                            + sum(len(l) + 1 for l in sub_lines)
                            + len(gh)
                            + 1
                            > max_chars
                        ):
                            break
                        sub_lines.append(gh)
                    trunk = "     " if is_last_group else "  │  "
                    for tti, (bare_name, tool_entry) in enumerate(tools):
                        tool_sig = _format_tool_signature(
                            tool_entry["tool_id"],
                            tool_entry["metadata"],
                            display_name=bare_name,
                        )
                        is_last_tool = tti == len(tools) - 1
                        if prefix:
                            twig = "└─" if is_last_tool else "├─"
                            line = f"{trunk}{twig} {tool_sig}"
                        else:
                            twig = "└─" if (is_last_tool and is_last_group) else "├─"
                            line = f"  {twig} {tool_sig}"
                        if (
                            current_chars
                            + sig_chars
                            + sum(len(l) + 1 for l in sub_lines)
                            + len(line)
                            + 1
                            > max_chars
                        ):
                            break
                        sub_lines.append(line)
                        preloaded.append(tool_entry["tool_id"])
            else:
                # Flat type label (directives, tools, knowledge)
                branch = "  └─" if is_last_type else "  ├─"
                label = f"{branch} {item_type}s"
                if (
                    current_chars
                    + sig_chars
                    + sum(len(l) + 1 for l in sub_lines)
                    + len(label)
                    + 1
                    > max_chars
                ):
                    break
                sub_lines.append(label)

        block_chars = sig_chars + sum(len(l) + 1 for l in sub_lines)
        current_chars += block_chars
        preloaded.append(tool_id)
        if sub_lines:
            blocks.append(sig + "\n" + "\n".join(sub_lines))
        else:
            blocks.append(sig)

    # Fallback: non-primary tools without a primary rye_execute entry
    # (e.g. when primary_tools is not passed). Render them as flat signatures.
    if not blocks and non_primary_tools:
        for entry in non_primary_tools:
            sig = _format_tool_signature(entry["tool_id"], entry["metadata"])
            sig_chars = len(sig) + 1
            if current_chars + sig_chars > max_chars:
                break
            blocks.append(sig)
            preloaded.append(entry["tool_id"])
            current_chars += sig_chars

    if not blocks:
        return {"schemas": "", "preloaded_tools": []}

    schemas = "<capabilities>\n" + "\n".join(blocks) + "\n</capabilities>"
    return {"schemas": schemas, "preloaded_tools": preloaded}