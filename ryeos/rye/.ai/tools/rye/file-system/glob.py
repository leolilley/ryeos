# rye:signed:2026-02-25T00:02:14Z:e68fb35441cc447ee2123931c9ab3a6bd05c7a8229f9fc015724c5f8c1b2bea3:xtcYV2BG-3Tol8L31LDCOT28jGiE4YWIBTsGNoFw10iJ-AiyvjsuDzWgar7lLThJQvWp8U2fZLFOm7Qmie_MDw==:9fbfabe975fa5a7f
"""Find files by glob pattern."""

import argparse
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/file-system"
__tool_description__ = "Find files by glob pattern"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "pattern": {
            "type": "string",
            "description": "Glob pattern (e.g., '**/*.py')",
        },
        "path": {
            "type": "string",
            "description": "Search path (default: project root)",
        },
    },
    "required": ["pattern"],
}

MAX_RESULTS = 100
IGNORE_DIRS = {
    "node_modules",
    "__pycache__",
    ".git",
    ".venv",
    "venv",
    ".tox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "dist",
    "build",
    "*.egg-info",
    ".eggs",
    ".nox",
    ".hg",
    ".svn",
}


def should_ignore(path: Path, base: Path) -> bool:
    """Check if path should be ignored."""
    try:
        relative = path.relative_to(base)
        for part in relative.parts:
            for ignore_pattern in IGNORE_DIRS:
                if ignore_pattern.startswith("*"):
                    if ignore_pattern[1:] in part:
                        return True
                elif part == ignore_pattern:
                    return True
        return False
    except ValueError:
        return True


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    pattern = params["pattern"]
    search_path = params.get("path")

    if search_path:
        search_path = Path(search_path)
        if not search_path.is_absolute():
            search_path = project / search_path
        search_path = search_path.resolve()

        if not search_path.is_relative_to(project):
            return {
                "success": False,
                "error": "Search path is outside the project workspace",
            }
    else:
        search_path = project

    if not search_path.exists():
        return {"success": False, "error": f"Search path not found: {search_path}"}

    try:
        matches = []
        for match in search_path.rglob(pattern):
            if match.is_file() and not should_ignore(match, project):
                try:
                    relative = match.relative_to(project)
                    matches.append(str(relative))
                except ValueError:
                    continue

                if len(matches) >= MAX_RESULTS:
                    break

        matches.sort()
        truncated = len(matches) >= MAX_RESULTS

        output = "\n".join(matches)

        return {
            "success": True,
            "output": output,
            "files": matches,
            "count": len(matches),
            "truncated": truncated,
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
