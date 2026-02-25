# rye:signed:2026-02-25T00:02:14Z:844180b3aab9a63904111c72cfd1be9f23938ac6bd0af64a336687a00372fb8b:ybRchZjbd_9rfFGI4X5IPK_4dl3UvbGeQ9n8_RjvBo_56T0TR34b5TucG8CFAqZf7rk0_15nmoDPnB1OzyOEAQ==:9fbfabe975fa5a7f
"""Edit files by line ID (not string matching)."""

import argparse
import difflib
import hashlib
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/file-system"
__tool_description__ = (
    "Edit files using LIDs (stable line references from the read tool). "
    "Pass LIDs as line_id for single-line edits, or start_line_id/end_line_id for ranges."
)

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "path": {
            "type": "string",
            "description": "Path to file (relative to project root or absolute)",
        },
        "changes": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "line_id": {
                        "type": "string",
                        "description": "Line ID to replace",
                    },
                    "start_line_id": {
                        "type": "string",
                        "description": "Start line ID for range replacement",
                    },
                    "end_line_id": {
                        "type": "string",
                        "description": "End line ID for range replacement (inclusive)",
                    },
                    "new_content": {
                        "type": "string",
                        "description": "New content for the line(s)",
                    },
                },
            },
            "description": "List of change operations",
        },
    },
    "required": ["path", "changes"],
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


def invalidate_cache(file_path: Path, project_path: Path) -> None:
    """Remove line index cache for the file."""
    cache_path = get_line_index_path(file_path, project_path)
    if cache_path.exists():
        cache_path.unlink()


def build_id_to_line_map(cached_index: dict) -> dict[str, int]:
    """Build a map from line ID to line number."""
    return {line["id"]: line["line_num"] for line in cached_index.get("lines", [])}


def validate_changes(
    changes: list[dict], id_to_line: dict[str, int]
) -> tuple[bool, list[str]]:
    """Validate all changes have valid line IDs.

    Returns:
        (is_valid, list_of_invalid_ids)
    """
    invalid = []
    for change in changes:
        if "line_id" in change:
            if change["line_id"] not in id_to_line:
                invalid.append(change["line_id"])
        elif "start_line_id" in change and "end_line_id" in change:
            if change["start_line_id"] not in id_to_line:
                invalid.append(change["start_line_id"])
            if change["end_line_id"] not in id_to_line:
                invalid.append(change["end_line_id"])
    return len(invalid) == 0, invalid


def apply_changes(
    lines: list[str], changes: list[dict], id_to_line: dict[str, int]
) -> tuple[list[str], int]:
    """Apply changes to lines.

    Returns:
        (new_lines, lines_changed_count)
    """
    line_changes = []

    for change in changes:
        new_content = change.get("new_content", "")

        if "line_id" in change:
            line_num = id_to_line[change["line_id"]]
            line_changes.append((line_num, line_num, new_content.splitlines()))
        elif "start_line_id" in change and "end_line_id" in change:
            start_num = id_to_line[change["start_line_id"]]
            end_num = id_to_line[change["end_line_id"]]
            line_changes.append((start_num, end_num, new_content.splitlines()))

    line_changes.sort(key=lambda x: x[0], reverse=True)

    new_lines = lines.copy()
    lines_changed = 0

    for start_num, end_num, new_content_lines in line_changes:
        lines_affected = end_num - start_num + 1
        lines_changed += max(lines_affected, len(new_content_lines))

        del new_lines[start_num - 1 : end_num]
        for i, content_line in enumerate(reversed(new_content_lines)):
            new_lines.insert(start_num - 1, content_line)

    return new_lines, lines_changed


def generate_diff(old_lines: list[str], new_lines: list[str], file_path: str) -> str:
    """Generate unified diff output."""
    old_with_newlines = [line + "\n" for line in old_lines]
    new_with_newlines = [line + "\n" for line in new_lines]

    diff = difflib.unified_diff(
        old_with_newlines,
        new_with_newlines,
        fromfile=f"a/{file_path}",
        tofile=f"b/{file_path}",
    )

    return "".join(diff)


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    file_path = Path(params["path"])
    changes = params["changes"]

    if not file_path.is_absolute():
        file_path = project / file_path
    file_path = file_path.resolve()

    if not file_path.is_relative_to(project):
        return {"success": False, "error": "Path is outside the project workspace"}

    if not file_path.exists():
        return {"success": False, "error": f"File not found: {file_path}"}

    if file_path.is_dir():
        return {"success": False, "error": "Path is a directory, not a file"}

    cache_path = get_line_index_path(file_path, project)
    cached_index = load_cached_index(cache_path)

    if cached_index is None:
        return {
            "success": False,
            "error": "No line ID cache found. Read the file first to generate line IDs.",
        }

    id_to_line = build_id_to_line_map(cached_index)

    is_valid, invalid_ids = validate_changes(changes, id_to_line)
    if not is_valid:
        available_ids = list(id_to_line.keys())[:10]
        return {
            "success": False,
            "error": f"Invalid line IDs: {invalid_ids}. Available IDs include: {available_ids}...",
        }

    try:
        content = file_path.read_text()
        lines = content.splitlines()

        current_hash = hashlib.sha256(content.encode()).hexdigest()
        if current_hash != cached_index.get("content_hash"):
            return {
                "success": False,
                "error": "File has changed since last read. Re-read the file to get updated line IDs.",
            }

        new_lines, lines_changed = apply_changes(lines, changes, id_to_line)

        new_content = "\n".join(new_lines)
        if content.endswith("\n"):
            new_content += "\n"

        file_path.write_text(new_content)

        invalidate_cache(file_path, project)

        try:
            relative_path = str(file_path.relative_to(project))
        except ValueError:
            relative_path = str(file_path)

        diff_output = generate_diff(lines, new_lines, relative_path)

        return {
            "success": True,
            "output": diff_output,
            "changes_applied": len(changes),
            "lines_changed": lines_changed,
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
