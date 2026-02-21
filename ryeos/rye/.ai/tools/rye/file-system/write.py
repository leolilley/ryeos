# rye:signed:2026-02-21T05:56:40Z:38c6c176ee9cc03c5719febe402bcfd2de742fc9f7d53a438d74357096a58647:zqc24vUsVKgvaOwFQ74ATPlLDfy2oiZxKslsiFFiD3d--yFBqfL2Qtglz3kv57PtQlNBgn9a4cnwFQ3y6EdnAw==:9fbfabe975fa5a7f
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
__tool_description__ = "Create or overwrite one or more files"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "file_path": {
            "type": "string",
            "description": "Path to file (single-file mode). Mutually exclusive with 'files'.",
        },
        "content": {
            "type": "string",
            "description": "Content to write (single-file mode).",
        },
        "files": {
            "type": "array",
            "description": "Batch mode — list of {file_path, content} objects to write in one call.",
            "items": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["file_path", "content"],
            },
        },
    },
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


def _write_one(file_path_str: str, content: str, project: Path) -> dict:
    """Write a single file and return its result."""
    file_path = Path(file_path_str)

    if not file_path.is_absolute():
        file_path = project / file_path
    file_path = file_path.resolve()

    if not file_path.is_relative_to(project):
        return {"success": False, "file_path": file_path_str, "error": "Path is outside the project workspace"}

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
        return {"success": False, "file_path": file_path_str, "error": str(e)}


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()

    # Batch mode
    files = params.get("files")
    if files:
        results = [_write_one(f["file_path"], f["content"], project) for f in files]
        failed = [r for r in results if not r["success"]]
        return {
            "success": len(failed) == 0,
            "files_written": len(results) - len(failed),
            "files_total": len(results),
            "results": results,
            **({"errors": failed} if failed else {}),
        }

    # Single-file mode
    if "file_path" not in params or "content" not in params:
        return {"success": False, "error": "Provide either 'files' (batch) or 'file_path'+'content' (single)."}

    return _write_one(params["file_path"], params["content"], project)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
