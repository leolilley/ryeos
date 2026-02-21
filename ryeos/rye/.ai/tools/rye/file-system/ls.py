# rye:signed:2026-02-21T05:56:40Z:999d5c38e7ddcca40d2af868726bdc49ea2f4bed925b0579e2595e72cd4aab4e:ucjh9XlRw0SadCKkDf5bVaC9SC1QhU6bx1KqBq5DGG9Eyi3WbQwU_gqweXkHCQJYVk-ugr4wuTqlZT9guV5UBQ==:9fbfabe975fa5a7f
"""List directory contents."""

import argparse
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
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
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
