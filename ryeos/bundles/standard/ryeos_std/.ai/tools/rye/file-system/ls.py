# rye:signed:2026-04-10T00:57:19Z:2f5d545c7a1c51e6d6b76dcd6e1524f283c709d4f63eefd2838c4f6c74680b1f:uWNrYnWa3LA4YaSKxMDfGRVNSDTGOy_oexJMKuamy3dg22fO-FL5raEIW_vLCYvdHxpzhxqiSWyfprry-O4ZDQ:4b987fd4e40303ac
"""List directory contents."""

import argparse
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/file-system"
__tool_description__ = "List directory contents"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "path": {
            "type": "string",
            "description": "Directory path (default: project root)",
        },
    },
    "required": [],
}

IGNORE_ENTRIES = {
    "__pycache__",
    ".venv",
    "venv",
    "node_modules",
    ".git",
    ".tox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "*.egg-info",
    ".eggs",
    ".nox",
    ".hg",
    ".svn",
}


def should_ignore(name: str) -> bool:
    """Check if entry should be ignored."""
    for pattern in IGNORE_ENTRIES:
        if pattern.startswith("*"):
            if pattern[1:] in name:
                return True
        elif name == pattern:
            return True
    return False


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    dir_path = params.get("path")

    if dir_path:
        dir_path = Path(dir_path)
        if not dir_path.is_absolute():
            dir_path = project / dir_path
        dir_path = dir_path.resolve()

        if not dir_path.is_relative_to(project):
            return {"success": False, "error": "Path is outside the project workspace"}
    else:
        dir_path = project

    if not dir_path.exists():
        return {"success": False, "error": f"Directory not found: {dir_path}"}

    if not dir_path.is_dir():
        return {"success": False, "error": "Path is not a directory"}

    try:
        entries = []
        output_parts = []

        for entry in sorted(
            dir_path.iterdir(), key=lambda e: (not e.is_dir(), e.name.lower())
        ):
            if should_ignore(entry.name):
                continue

            try:
                relative = entry.relative_to(project)
                relative_str = str(relative)
            except ValueError:
                relative_str = entry.name

            if entry.is_dir():
                entries.append({"name": relative_str, "type": "directory"})
                output_parts.append(f"{relative_str}/")
            else:
                entries.append({"name": relative_str, "type": "file"})
                output_parts.append(relative_str)

        return {
            "success": True,
            "output": "\n".join(output_parts),
            "entries": entries,
        }
    except Exception as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
