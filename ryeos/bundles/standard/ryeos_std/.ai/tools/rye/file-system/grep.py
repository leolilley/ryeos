# rye:signed:2026-02-26T05:02:40Z:d0619e08dac8504f9a7ccabd98e37cc84fc08412cf0ca20e1bd9526c7a1e68b9:3TMWQS2xDgPmS1hpiH1U7J1S3pDcj_taiiZi6MTGuP_TkxcEwoP6JxjFiQgnBHuGAsNNC1lOaeweWKudKBmWAg==:4b987fd4e40303ac
"""Search file contents with regex, returning line IDs."""

import argparse
import fnmatch
import hashlib
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/file-system"
__tool_description__ = (
    "Search file contents with regex. Results include LIDs (stable line references) "
    "when available — pass them to edit_lines to edit matched lines."
)

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "pattern": {
            "type": "string",
            "description": "Regex pattern to search for",
        },
        "path": {
            "type": "string",
            "description": "Search path (default: project root)",
        },
        "include": {
            "type": "string",
            "description": "File glob filter (e.g., '*.py')",
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
}


def get_line_index_path(file_path: Path, project_path: Path) -> Path:
    """Get cache path for line index."""
    try:
        relative = file_path.relative_to(project_path)
    except ValueError:
        relative = Path(file_path.name)
    path_hash = hashlib.sha256(str(relative).encode()).hexdigest()[:16]
    return (
        project_path
        / ".ai"
        / "cache"
        / "tools"
        / "read"
        / "line_index"
        / f"{path_hash}.json"
    )


def load_cached_index(cache_path: Path) -> dict | None:
    """Load cached line index if it exists."""
    if not cache_path.exists():
        return None
    try:
        return json.loads(cache_path.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def get_line_id_for_line(
    file_path: Path, project_path: Path, line_num: int
) -> str | None:
    """Get line ID for a specific line number."""
    cache_path = get_line_index_path(file_path, project_path)
    cached_index = load_cached_index(cache_path)

    if cached_index is None:
        return None

    for line_info in cached_index.get("lines", []):
        if line_info["line_num"] == line_num:
            return line_info["id"]

    return None


def has_ripgrep() -> bool:
    """Check if ripgrep is available."""
    return shutil.which("rg") is not None


def search_with_ripgrep(
    pattern: str, search_path: Path, include: str | None
) -> list[tuple[str, int, str]]:
    """Search using ripgrep."""
    cmd = [
        "rg",
        "-n",
        "--no-heading",
        "--line-number",
        "-H",
    ]

    for ignore_dir in IGNORE_DIRS:
        cmd.extend(["--glob", f"!{ignore_dir}/**"])

    if include:
        cmd.extend(["--glob", include])

    cmd.extend([pattern, str(search_path)])

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=30,
        )

        matches = []
        for line in result.stdout.strip().split("\n"):
            if not line:
                continue

            if ":" in line:
                parts = line.split(":", 2)
                if len(parts) >= 3:
                    file_path = parts[0]
                    try:
                        line_num = int(parts[1])
                    except ValueError:
                        continue
                    content = parts[2]
                    matches.append((file_path, line_num, content))

        return matches
    except subprocess.TimeoutExpired:
        return []
    except Exception:
        return []


def search_with_fallback(
    pattern: str, search_path: Path, include: str | None
) -> list[tuple[str, int, str]]:
    """Fallback search using Python."""
    matches = []
    try:
        regex = re.compile(pattern)
    except re.error:
        return []

    for file_path in search_path.rglob("*"):
        if not file_path.is_file():
            continue

        if any(ignore_dir in file_path.parts for ignore_dir in IGNORE_DIRS):
            continue

        if include and not fnmatch.fnmatch(file_path.name, include):
            continue

        try:
            content = file_path.read_text()
            for i, line in enumerate(content.splitlines(), 1):
                if regex.search(line):
                    matches.append((str(file_path), i, line))
                    if len(matches) >= MAX_RESULTS:
                        return matches
        except (OSError, UnicodeDecodeError):
            continue

    return matches


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    pattern = params["pattern"]
    search_path = params.get("path")
    include = params.get("include")

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
        if has_ripgrep():
            raw_matches = search_with_ripgrep(pattern, search_path, include)
        else:
            raw_matches = search_with_fallback(pattern, search_path, include)

        truncated = len(raw_matches) >= MAX_RESULTS
        raw_matches = raw_matches[:MAX_RESULTS]

        matches = []
        output_lines = []

        for file_path_str, line_num, content in raw_matches:
            file_path = Path(file_path_str)
            if not file_path.is_absolute():
                file_path = search_path / file_path

            line_id = get_line_id_for_line(file_path, project, line_num)

            try:
                relative_path = str(file_path.relative_to(project))
            except ValueError:
                relative_path = str(file_path)

            match_info = {
                "file": relative_path,
                "line": line_num,
                "content": content,
            }

            if line_id:
                match_info["line_id"] = line_id
                output_lines.append(
                    f"{relative_path}:{line_num}:{line_id}│ {content}"
                )
            else:
                output_lines.append(f"{relative_path}:{line_num}│ {content}")

            matches.append(match_info)

        return {
            "success": True,
            "output": "\n".join(output_lines),
            "matches": matches,
            "count": len(matches),
            "truncated": truncated,
        }
    except re.error as e:
        return {"success": False, "error": f"Invalid regex pattern: {e}"}
    except Exception as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
