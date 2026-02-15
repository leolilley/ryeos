# rye:signed:2026-02-15T07:11:41Z:e9727fbb38ce81d4810d88760539d38ea32125ebf6a480660520b3c7b6052c63:S2FTT-fptlHmCsb6XH-5wZQWBbLfzpJE94mCLFcJLtHAQ59Ul3N4_VbV6Wuxjc0R5tI1JFc26a47nUmFbsvdAw==:440443d0858f0199
"""Create or overwrite a file, invalidating line ID cache."""

import argparse
import hashlib
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
__category__ = "rye/file-system"
__tool_description__ = "Create or overwrite a file"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "file_path": {
            "type": "string",
            "description": "Path to file (relative to project root or absolute)",
        },
        "content": {
            "type": "string",
            "description": "Content to write to the file",
        },
    },
    "required": ["file_path", "content"],
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


def invalidate_cache(file_path: Path, project_path: Path) -> None:
    """Remove line index cache for the file."""
    cache_path = get_line_index_path(file_path, project_path)
    if cache_path.exists():
        cache_path.unlink()


def generate_diff(old_content: str | None, new_content: str, file_path: str) -> str:
    """Generate a simple diff output."""
    import difflib

    if old_content is None:
        return f"Created new file {file_path} ({len(new_content)} bytes)"

    old_lines = old_content.splitlines(keepends=True)
    new_lines = new_content.splitlines(keepends=True)

    diff = difflib.unified_diff(
        old_lines,
        new_lines,
        fromfile=f"a/{file_path}",
        tofile=f"b/{file_path}",
    )

    return "".join(diff)


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    file_path = Path(params["file_path"])
    content = params["content"]

    if not file_path.is_absolute():
        file_path = project / file_path
    file_path = file_path.resolve()

    if not file_path.is_relative_to(project):
        return {"success": False, "error": "Path is outside the project workspace"}

    created = not file_path.exists()

    old_content = None
    if file_path.exists():
        try:
            old_content = file_path.read_text()
        except Exception:
            pass

    try:
        file_path.parent.mkdir(parents=True, exist_ok=True)
        file_path.write_text(content)

        invalidate_cache(file_path, project)

        try:
            relative_path = str(file_path.relative_to(project))
        except ValueError:
            relative_path = str(file_path)

        diff_output = generate_diff(old_content, content, relative_path)

        return {
            "success": True,
            "output": diff_output,
            "file_path": relative_path,
            "bytes_written": len(content),
            "created": created,
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
