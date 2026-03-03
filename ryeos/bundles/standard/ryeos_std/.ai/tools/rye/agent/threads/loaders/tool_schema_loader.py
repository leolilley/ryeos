# Signature line (will be signed later)
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Tool schema preload from directive capabilities"

import ast
import fnmatch
import logging
from pathlib import Path
from typing import Dict, List, Optional

from rye.utils.resolvers import ToolResolver
from rye.utils.extensions import get_tool_extensions

logger = logging.getLogger(__name__)

_PRIMARY_PREFIX = "rye/primary/"
_CAPABILITY_PREFIX = "rye.execute.tool."
_CHARS_PER_TOKEN = 4


def _parse_capability(cap: str) -> Optional[str]:
    """Extract tool namespace pattern from a capability string.

    'rye.execute.tool.rye.bash.*' -> 'rye/bash/*'
    'rye.execute.tool.rye.file-system.read' -> 'rye/file-system/read'
    Returns None for non-tool capabilities.
    """
    if not cap.startswith(_CAPABILITY_PREFIX):
        return None
    remainder = cap[len(_CAPABILITY_PREFIX):]
    if not remainder:
        return None
    return remainder.replace(".", "/")


def _pattern_specificity(pattern: str) -> int:
    """Score pattern specificity — higher = more specific.

    Exact paths score highest; broader wildcards score lower.
    """
    if "*" not in pattern:
        return 1000
    parts = pattern.rstrip("/*").split("/")
    return len(parts)


def _extract_tool_metadata(file_path: Path) -> Optional[dict]:
    """Extract CONFIG_SCHEMA and __tool_description__ from a tool file via AST."""
    try:
        source = file_path.read_text()
        tree = ast.parse(source)
        schema = None
        description = ""
        for node in tree.body:
            if isinstance(node, ast.Assign) and len(node.targets) == 1:
                name = getattr(node.targets[0], "id", None)
                if name == "CONFIG_SCHEMA":
                    segment = ast.get_source_segment(source, node.value)
                    if segment:
                        schema = ast.literal_eval(segment)
                elif name == "__tool_description__":
                    if isinstance(node.value, ast.Constant):
                        description = node.value.value
        if schema:
            return {"schema": schema, "description": description}
    except Exception:
        logger.debug("Failed to extract metadata from %s", file_path, exc_info=True)
    return None


def _tool_id_from_path(file_path: Path, search_dir: Path) -> str:
    """Derive the tool item_id from its file path relative to a search dir."""
    rel = file_path.relative_to(search_dir)
    return str(rel.with_suffix(""))


def _format_schema_block(tool_id: str, metadata: dict) -> str:
    """Format a tool schema as an rye_execute call example.

    Output matches the calling convention the agent actually uses:
      rye_execute(item_type="tool", item_id="rye/bash/bash", parameters={"command": "<string>", ...})
        Execute shell commands
        command (string) [required]: Shell command to execute
        timeout (integer): Timeout in seconds (default: 120)
    """
    schema = metadata["schema"]
    desc = metadata.get("description", "")
    props = schema.get("properties", {})
    required = set(schema.get("required", []))

    # Build the parameters dict preview with type placeholders
    param_parts = []
    for param_name, param_def in props.items():
        ptype = param_def.get("type", "any")
        param_parts.append(f'"{param_name}": "<{ptype}>"')
    params_str = "{" + ", ".join(param_parts) + "}" if param_parts else "{}"

    lines = [f'rye_execute(item_type="tool", item_id="{tool_id}", parameters={params_str})']
    if desc:
        lines.append(f"  {desc}")
    for param_name, param_def in props.items():
        ptype = param_def.get("type", "any")
        pdesc = param_def.get("description", "")
        req_tag = " [required]" if param_name in required else ""
        suffix = f": {pdesc}" if pdesc else ""
        lines.append(f"  {param_name} ({ptype}){req_tag}{suffix}")
    return "\n".join(lines)


def _resolve_tools_for_pattern(
    pattern: str,
    resolver: ToolResolver,
    extensions: List[str],
    seen: set,
) -> List[dict]:
    """Find all tool files matching a namespace pattern across 3-tier space.

    Returns list of {"tool_id": str, "metadata": dict} for unseen tools.
    Uses first-match semantics (project > user > system).
    """
    results = []
    resolved_ids = set()

    for search_dir, _space in resolver.get_search_paths():
        # Convert pattern to a directory glob
        # 'rye/bash/*' -> search for all .py files under rye/bash/
        # 'rye/file-system/read' -> search for rye/file-system/read.py exactly
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
                    meta = _extract_tool_metadata(fp)
                    if meta:
                        results.append({"tool_id": tool_id, "metadata": meta})
        else:
            # Exact tool reference
            if pattern in resolved_ids or pattern in seen:
                continue
            if pattern.startswith(_PRIMARY_PREFIX):
                continue
            for ext in extensions:
                fp = search_dir / f"{pattern}{ext}"
                if fp.is_file():
                    resolved_ids.add(pattern)
                    meta = _extract_tool_metadata(fp)
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
) -> dict:
    """Parse capabilities, resolve matching tools, and return formatted schema blocks.

    Args:
        capabilities: List of capability strings from harness._capabilities.
        project_path: Project root for 3-tier resolution.
        max_tokens: Token budget for the output (estimated at 4 chars/token).

    Returns:
        {"schemas": formatted_string, "preloaded_tools": [item_ids]}
    """
    # Step 1: Parse capabilities into namespace patterns, skip non-tool caps
    patterns = []
    for cap in capabilities:
        pat = _parse_capability(cap)
        if pat:
            patterns.append(pat)

    if not patterns:
        return {"schemas": "", "preloaded_tools": []}

    # Step 2: Sort by specificity — narrow patterns first
    patterns.sort(key=_pattern_specificity, reverse=True)

    # Step 3: Resolve tools across 3-tier space
    resolver = ToolResolver(project_path)
    extensions = get_tool_extensions(project_path)
    seen: set = set()
    all_tools: List[dict] = []

    for pattern in patterns:
        matches = _resolve_tools_for_pattern(pattern, resolver, extensions, seen)
        for m in matches:
            seen.add(m["tool_id"])
        all_tools.extend(matches)

    if not all_tools:
        return {"schemas": "", "preloaded_tools": []}

    # Step 4: Format schema blocks within token budget
    max_chars = max_tokens * _CHARS_PER_TOKEN
    blocks = []
    preloaded = []
    header = "Available tools (call via rye_execute):\n"
    current_chars = len(header)

    for tool in all_tools:
        block = _format_schema_block(tool["tool_id"], tool["metadata"])
        block_chars = len(block) + 2  # +2 for blank line separator
        if current_chars + block_chars > max_chars:
            break
        blocks.append(block)
        preloaded.append(tool["tool_id"])
        current_chars += block_chars

    if not blocks:
        return {"schemas": "", "preloaded_tools": []}

    schemas = header + "\n".join(blocks)
    return {"schemas": schemas, "preloaded_tools": preloaded}
