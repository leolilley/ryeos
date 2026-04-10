# rye:signed:2026-04-10T00:57:19Z:f70e6728a0c48a5c6a5099bd139283cf9595868e7a02f85ce92d3e15b1b97653:VIrTMqUEi3JB_8W12zG-aAakyVdclYEarDkE166F9B5a2YhCwHLHqOUVwn7sQstjsoGmzCEjQh33VecQMFN2AQ:4b987fd4e40303ac
__version__ = "2.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Capability-driven dynamic tool registration for thread agents"

import logging
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from rye.utils.extensions import get_parsers_map, get_tool_extensions
from rye.utils.parser_router import ParserRouter
from rye.utils.resolvers import ToolResolver

logger = logging.getLogger(__name__)

_CHARS_PER_TOKEN = 4

# Capability prefixes → (primary_action, sub_type_or_none)
# Longer (more specific) prefixes MUST come before shorter ones so that
# e.g. "rye.fetch.directive.*" matches the directive entry, not the
# generic "rye.fetch." wildcard.  sub_type=None means "all types".
_CAP_PREFIXES = {
    "rye.execute.tool.": ("execute", "tool"),
    "rye.execute.directive.": ("execute", "directive"),
    "rye.execute.knowledge.": ("execute", "knowledge"),
    "rye.fetch.directive.": ("fetch", "directive"),
    "rye.fetch.tool.": ("fetch", "tool"),
    "rye.fetch.knowledge.": ("fetch", "knowledge"),
    "rye.fetch.": ("fetch", None),
    "rye.sign.directive.": ("sign", "directive"),
    "rye.sign.tool.": ("sign", "tool"),
    "rye.sign.knowledge.": ("sign", "knowledge"),
    "rye.sign.": ("sign", None),
}

# Actions that get registered as direct agent tools when granted.
_PRIMARY_ACTIONS = ("execute", "fetch", "sign")


def _classify_capability(cap: str) -> Optional[dict]:
    """Classify a capability string into action, sub_type, and pattern.

    'rye.execute.tool.rye.file-system.read' → {action: 'execute', sub_type: 'tool', pattern: 'rye/file-system/read'}
    'rye.fetch.*'                           → {action: 'fetch', sub_type: None, pattern: '*'}
    'rye.fetch.knowledge.*'                 → {action: 'fetch', sub_type: 'knowledge', pattern: '*'}
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


def _extract_tool_metadata(
    file_path: Path, router: ParserRouter, parsers_map: Dict[str, str]
) -> Optional[dict]:
    """Extract schema and description from a tool file via ParserRouter.

    Uses the same parser infrastructure as the rest of Rye (python/ast,
    yaml/yaml, javascript/javascript) so any supported file type works.
    parsers_map is the extension→parser name mapping from get_parsers_map().
    Returns {"schema": dict, "description": str} or None.
    """
    ext = file_path.suffix.lower()
    parser_name = parsers_map.get(ext)
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


def _tool_id_to_api_name(tool_id: str) -> str:
    """Flatten a tool_id into an API-safe tool name.

    rye/fetch → rye_fetch
    rye/file-system/ls → rye_file_system_ls
    rye/bash → rye_bash
    """
    return tool_id.replace("/", "_").replace("-", "_")


def _strip_xml_markup(text: str) -> str:
    """Strip XML/HTML tags and truncate to first sentence.

    Primary action CONFIG_SCHEMA descriptions contain nested markup like
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
    parsers_map: Dict[str, str],
) -> List[dict]:
    """Find all tool files matching a namespace pattern across 3-tier space.

    Returns list of {"tool_id": str, "metadata": dict} for unseen tools.
    Uses first-match semantics (project > user > system).
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
                    resolved_ids.add(tool_id)
                    meta = _extract_tool_metadata(fp, router, parsers_map)
                    if meta:
                        results.append({"tool_id": tool_id, "metadata": meta})
        else:
            if pattern in resolved_ids or pattern in seen:
                continue
            for ext in extensions:
                fp = search_dir / f"{pattern}{ext}"
                if fp.is_file():
                    resolved_ids.add(pattern)
                    meta = _extract_tool_metadata(fp, router, parsers_map)
                    if meta:
                        results.append({"tool_id": pattern, "metadata": meta})
                    break
            if pattern in resolved_ids:
                break

    return results


def _build_path_tree(item_ids: List[str]) -> dict:
    """Build nested dict from slash-separated item IDs.

    ["rye/bash", "rye/file-system/glob"] →
    {"rye": {"bash": {}, "file-system": {"glob": {}}}}
    """
    tree: dict = {}
    for item_id in item_ids:
        node = tree
        for part in item_id.split("/"):
            node = node.setdefault(part, {})
    return tree


def _render_tree_lines(tree: dict, prefix: str = "") -> List[str]:
    """Render nested dict as box-drawing tree lines."""
    lines = []
    keys = sorted(tree.keys())
    for i, key in enumerate(keys):
        last = i == len(keys) - 1
        branch = "└── " if last else "├── "
        child_prefix = prefix + ("    " if last else "│   ")
        lines.append(f"{prefix}{branch}{key}")
        if tree[key]:
            lines.extend(_render_tree_lines(tree[key], child_prefix))
    return lines


def _load_exclude_dirs(tool_search_paths: List[Tuple[Path, str]]) -> frozenset:
    """Load exclude_dirs from collect.yaml using 3-tier resolution.

    Same config the bundler uses — first match from project > user > system.
    Falls back to a minimal hardcoded set if no config found.
    """
    import yaml

    config_rel = Path("rye") / "core" / "bundler" / "collect.yaml"
    for search_dir, _space in tool_search_paths:
        config_path = search_dir / config_rel
        if config_path.exists():
            try:
                data = yaml.safe_load(config_path.read_text())
                if isinstance(data, dict) and "exclude_dirs" in data:
                    return frozenset(data["exclude_dirs"]) | {"__init__"}
            except Exception:
                continue

    return frozenset(("__pycache__", "node_modules", ".venv", ".git", "__init__"))


def _resolve_items_for_type(
    kind: str,
    patterns: List[str],
    search_paths: List[Tuple[Path, str]],
    extensions: List[str],
    exclude_dirs: frozenset = frozenset(),
) -> List[str]:
    """Resolve patterns to actual item IDs from the filesystem.

    Walks search_paths (project > user > system) with first-match semantics.
    Skips directories matching exclude_dirs (from collect.yaml).
    """
    seen: set = set()
    items: List[str] = []

    def _is_excluded(fp: Path) -> bool:
        return fp.stem in exclude_dirs or any(part in exclude_dirs for part in fp.parts)

    for pattern in patterns:
        for search_dir, _space in search_paths:
            if not search_dir.exists():
                continue
            if pattern == "*" or pattern.endswith("/*"):
                glob_dir = search_dir if pattern == "*" else search_dir / pattern[:-2]
                if not glob_dir.exists():
                    continue
                for ext in extensions:
                    for fp in glob_dir.rglob(f"*{ext}"):
                        if not fp.is_file() or _is_excluded(fp.relative_to(search_dir)):
                            continue
                        item_id = _tool_id_from_path(fp, search_dir)
                        if item_id not in seen:
                            seen.add(item_id)
                            items.append(item_id)
            else:
                if pattern in seen:
                    continue
                for ext in extensions:
                    fp = search_dir / f"{pattern}{ext}"
                    if fp.is_file():
                        seen.add(pattern)
                        items.append(pattern)
                        break
                if pattern in seen:
                    break

    return sorted(items)


def _format_capabilities_tree(capabilities: list, project_path: Path) -> str:
    """Resolve capabilities to actual filesystem items and format as a tree.

    Walks the 3-tier space (project > user > system) for each item type
    and renders every accessible item. This is metadata-only — not sent
    to the LLM — so full resolution is fine.

    Input:  ["rye.execute.tool.rye.file-system.*", "rye.fetch.*"]
    Output:
      ├── execute
      │   └── tool
      │       └── rye
      │           └── file-system
      │               ├── glob
      │               ├── grep
      │               ├── ls
      │               ├── read
      │               └── write
      └── fetch
          ├── directive
          │   ├── init
          │   └── ...
          ├── knowledge
          │   └── ...
          └── tool
              └── ...
    """
    _ALL_TYPES = ("directive", "knowledge", "tool")
    _TYPE_DIRS = {"directive": "directives", "knowledge": "knowledge", "tool": "tools"}

    # 1. Classify capabilities → action → {sub_type → [patterns]}
    action_types: Dict[str, Dict[str, List[str]]] = {}
    for cap in capabilities:
        classified = _classify_capability(cap)
        if not classified:
            continue
        action = classified["action"]
        sub_type = classified["sub_type"]
        pattern = classified["pattern"]

        action_types.setdefault(action, {})
        types = [sub_type] if sub_type else list(_ALL_TYPES)
        for st in types:
            action_types[action].setdefault(st, []).append(pattern)

    if not action_types:
        return ""

    # 2. Build search paths per item type from the tool resolver's paths.
    #    ToolResolver gives us .ai/tools dirs; we swap to sibling dirs.
    resolver = ToolResolver(project_path)
    tool_search_paths = resolver.get_search_paths()
    tool_extensions = get_tool_extensions(project_path)

    search_paths_by_type: Dict[str, List[Tuple[Path, str]]] = {"tool": tool_search_paths}
    extensions_by_type: Dict[str, List[str]] = {"tool": tool_extensions}

    for st in ("directive", "knowledge"):
        type_dir = _TYPE_DIRS[st]
        paths = []
        for tool_dir, space in tool_search_paths:
            sibling = tool_dir.parent / type_dir
            if sibling.exists():
                paths.append((sibling, space))
        search_paths_by_type[st] = paths
        extensions_by_type[st] = [".md"]

    exclude_dirs = _load_exclude_dirs(tool_search_paths)

    # 3. Resolve each action → sub_type → actual item IDs
    resolved: Dict[str, Dict[str, List[str]]] = {}
    for action, type_patterns in action_types.items():
        resolved[action] = {}
        for st, patterns in type_patterns.items():
            resolved[action][st] = _resolve_items_for_type(
                st, patterns,
                search_paths_by_type.get(st, []),
                extensions_by_type.get(st, [".md"]),
                exclude_dirs=exclude_dirs,
            )

    # 4. Render tree
    lines: List[str] = []
    actions = sorted(resolved.keys())
    for i, action in enumerate(actions):
        last_action = i == len(actions) - 1
        a_branch = "└── " if last_action else "├── "
        a_indent = "    " if last_action else "│   "

        lines.append(f"{a_branch}{action}")
        sub_types = sorted(resolved[action].keys())
        for j, st in enumerate(sub_types):
            last_st = j == len(sub_types) - 1
            st_branch = a_indent + ("└── " if last_st else "├── ")
            st_indent = a_indent + ("    " if last_st else "│   ")

            items = resolved[action][st]
            if not items:
                lines.append(f"{st_branch}{st} (none)")
            else:
                lines.append(f"{st_branch}{st}")
                path_tree = _build_path_tree(items)
                lines.extend(_render_tree_lines(path_tree, st_indent))

    return "\n".join(lines)


def preload_tool_schemas(
    capabilities: list,
    project_path: Path,
    max_tokens: int = 2000,
    primary_actions: Optional[List[dict]] = None,
) -> dict:
    """Build dynamic tool definitions from resolved capability strings.

    Resolves ALL tools uniformly — primary actions (fetch, sign) and
    resolved tools (file-system/ls, bash, etc.) are treated as peers.
    Each tool gets a flattened API name and a _primary field for dispatch routing.

    The _primary field comes from the capability action:
      - rye.fetch.*              → _primary: "fetch"
      - rye.sign.*               → _primary: "sign"
      - rye.execute.tool.*       → individual tool defs, _primary: "execute"
      - rye.execute.directive.*  → rye_execute registered, _primary: "execute"
      - rye.execute.knowledge.*  → rye_execute registered, _primary: "execute"

    Returns:
        {"tool_defs": [tool_def_dicts], "capabilities_summary": [str]}
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
            action_item_types.setdefault(action, set()).update(_ALL_TYPES)
        if sub_type == "tool":
            tool_patterns.append(classified["pattern"])

    if not tool_patterns and not granted_actions:
        return {"tool_defs": [], "capabilities_summary": []}

    # Step 2: Build tool defs for primary actions (fetch, sign).
    # rye_execute is NOT registered as an agent tool — its functionality
    # is exposed via individual tool defs with _primary="execute".
    tool_defs: List[dict] = []
    seen: set = set()
    capabilities_summary: List[str] = []

    if primary_actions:
        for action in _PRIMARY_ACTIONS:
            if action not in granted_actions:
                continue
            primary_name = f"rye_{action}"
            for t in primary_actions:
                if t["name"] == primary_name:
                    tool_id = t["_item_id"]
                    api_name = _tool_id_to_api_name(tool_id)
                    desc = t.get("description", "")
                    # Narrow description to granted item types
                    accessible = action_item_types.get(action, set())
                    if accessible and accessible != _ALL_TYPES:
                        type_labels = sorted(accessible)
                        desc = f"{action.capitalize()} a {' or '.join(type_labels)}"

                    tool_defs.append({
                        "name": api_name,
                        "description": desc,
                        "schema": t["schema"],
                        "_item_id": tool_id,
                        "_primary": action,
                    })
                    seen.add(tool_id)

                    # Build summary entry
                    type_info = sorted(action_item_types.get(action, set()))
                    if type_info:
                        capabilities_summary.append(
                            f"{api_name} ({', '.join(type_info)})"
                        )
                    else:
                        capabilities_summary.append(api_name)
                    break

    # Step 3: Resolve executable tool schemas for execute.tool patterns
    resolver = ToolResolver(project_path)
    search_paths = resolver.get_search_paths()
    extensions = get_tool_extensions(project_path)
    router = ParserRouter(project_path)
    parsers_map = get_parsers_map(project_path)
    tool_patterns.sort(key=_pattern_specificity, reverse=True)
    resolved_tools: List[dict] = []

    for pattern in tool_patterns:
        matches = _resolve_tools_for_pattern(
            pattern, search_paths, extensions, seen, router, parsers_map
        )
        for m in matches:
            seen.add(m["tool_id"])
        resolved_tools.extend(matches)

    # Step 4: Build tool defs for resolved tools within token budget.
    max_chars = max_tokens * _CHARS_PER_TOKEN
    current_chars = 0
    # Estimate chars already used by primary action defs
    for td in tool_defs:
        current_chars += len(str(td.get("schema", {}))) + len(td.get("description", ""))

    for entry in resolved_tools:
        tool_id = entry["tool_id"]
        api_name = _tool_id_to_api_name(tool_id)
        meta = entry["metadata"]

        # Estimate token cost of this tool def
        tool_chars = len(str(meta["schema"])) + len(meta.get("description", ""))
        if current_chars + tool_chars > max_chars:
            continue

        tool_defs.append({
            "name": api_name,
            "description": meta.get("description", ""),
            "schema": meta["schema"],
            "_item_id": tool_id,
            "_primary": "execute",
        })
        capabilities_summary.append(api_name)
        current_chars += tool_chars

    return {
        "tool_defs": tool_defs,
        "capabilities_summary": capabilities_summary,
        "capabilities_tree": _format_capabilities_tree(capabilities, project_path),
    }
