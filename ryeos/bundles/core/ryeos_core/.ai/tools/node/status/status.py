# rye:signed:2026-04-20T05:37:45Z:2c75956c7bb299ac3b3af7dd1c4b8e30d8bb8175dcaa155c1a7ab281b0830d8e:w1FBpnG4Q2cnwUmc4hE3J9kzsLkCgEJqtlNLHU554jBGb64F4QyVCk15KgKJzvyf_3f7HXfmCb7LWe0xFBaYCA:4b987fd4e40303ac
"""
Node capability scanner — introspects system bundle tools.

Scans .ai/tools/ across installed system bundles, reads __execution__
markers to classify tools as provides (directly executable) vs routes
(meta-routing tools), and builds capability strings.

Does NOT include node_id, active count, health, or hardware — those
are server runtime state.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "node/status"
__execution__ = "routed"
__tool_description__ = "Local workspace introspection — scans available tools, reports capabilities"

import logging
from typing import Any, Dict, List

logger = logging.getLogger(__name__)

TOOL_METADATA = {
    "name": "status",
    "description": "Scan system bundles for tool capabilities",
    "version": __version__,
    "protected": True,
}

ACTIONS = ["scan"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {"type": "string", "enum": ACTIONS},
    },
    "required": ["action"],
}


def scan_capabilities() -> tuple[list, list]:
    """Scan system bundle tools for capability classification.

    Returns:
        (provides, routes) — two lists of capability strings.
    """
    provides: List[str] = []
    routes: List[str] = []
    try:
        from rye.utils.path_utils import get_system_spaces
        from rye.constants import AI_DIR

        for bundle in get_system_spaces():
            tools_dir = bundle.root_path / AI_DIR / "tools"
            if not tools_dir.is_dir():
                continue
            for file_path in tools_dir.rglob("*"):
                if not file_path.is_file() or file_path.name.startswith("_"):
                    continue
                if file_path.suffix not in (".py", ".md", ".yaml", ".yml"):
                    continue
                rel = file_path.relative_to(tools_dir)
                tool_id = str(rel.with_suffix(""))
                cap = f"rye.execute.tool.{tool_id.replace('/', '.')}"
                try:
                    head = file_path.read_text(errors="replace")[:2048]
                    if "__execution__" in head:
                        for line in head.splitlines():
                            if line.strip().startswith("__execution__"):
                                val = line.split("=", 1)[1].strip().strip("\"'")
                                if val == "routed":
                                    routes.append(cap)
                                    break
                        else:
                            provides.append(cap)
                    else:
                        provides.append(cap)
                except Exception:
                    provides.append(cap)
    except Exception:
        logger.warning("Failed to scan tools", exc_info=True)
    return provides, routes


async def _scan(params: Dict, project_path: str) -> Dict:
    """Run the scan action."""
    provides, routes = scan_capabilities()
    return {"success": True, "provides": provides, "routes": routes}


async def execute(params: dict, project_path: str) -> dict:
    """Entry point for function runtime."""
    action = params.pop("action", None)
    if not action:
        return {"success": False, "error": "action required"}
    if action != "scan":
        return {"success": False, "error": f"Unknown action: {action}"}

    try:
        return await _scan(params, project_path)
    except Exception as e:
        logger.exception("Scan failed")
        return {"success": False, "error": f"Scan failed: {e}"}
