# rye:signed:2026-02-26T05:02:40Z:3f1501b5d312ff78d820eb5e5dbe655e47dc4f9ece10df8408692199f91ab41e:p1usWES1AD0q2WLRjg8LR1QRPdbfk_ww290Dfrv1oEZKqWeDS8C8rupXeFLuwSk-I3CH8Xk6NyW9E8DTO2u7CA==:4b987fd4e40303ac
"""Read a file with persistent line IDs for stable editing."""

import argparse
import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/file-system"
__tool_description__ = (
    "Read file content. Each line is prefixed with LINE_NUM:LID where LID is a "
    "stable 6-char hex reference. LIDs are NOT part of the file content — they are "
    "metadata for use with edit_lines (pass as line_id, start_line_id, end_line_id)."
)

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "path": {
            "type": "string",
            "description": "Path to file (relative to project root or absolute)",
        },
        "offset": {
            "type": "integer",
            "description": "Starting line number (1-indexed)",
            "default": 1,
        },
        "limit": {
            "type": "integer",
            "description": "Maximum number of lines to read",
            "default": 2000,
        },
    },
    "required": ["path"],
}

MAX_LINE_LENGTH = 2000
MAX_TOTAL_BYTES = 51200


def generate_line_id(line_num: int, content: str) -> str:
    """Generate a short stable ID for a line."""
    data = f"{line_num}:{content}"
    return hashlib.sha256(data.encode()).hexdigest()[:6]


def get_line_index_path(file_path: Path, project_path: Path) -> Path:
    """Get cache path for line index following RYE conventions."""
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


def save_cached_index(cache_path: Path, index: dict) -> None:
    """Save line index to cache."""
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    cache_path.write_text(json.dumps(index, indent=2))


def compute_content_hash(content: str) -> str:
    """Compute hash of file content."""
    return hashlib.sha256(content.encode()).hexdigest()


def reconcile_line_index(
    lines: list[str], cached_index: dict | None
) -> tuple[list[dict], int, int]:
    """Match current lines to cached IDs by content hash.

    Returns:
        (new_index, reused_count, new_count)
    """
    if cached_index is None:
        new_index = []
        for i, line in enumerate(lines, 1):
            content_hash = hashlib.sha256(line.encode()).hexdigest()
            new_index.append(
                {
                    "id": generate_line_id(i, line),
                    "line_num": i,
                    "content_hash": content_hash,
                }
            )
        return new_index, 0, len(lines)

    content_to_line = {
        line["content_hash"]: line for line in cached_index.get("lines", [])
    }

    new_index = []
    reused = 0

    for i, line_content in enumerate(lines, 1):
        content_hash = hashlib.sha256(line_content.encode()).hexdigest()

        if content_hash in content_to_line:
            line_id = content_to_line[content_hash]["id"]
            reused += 1
        else:
            line_id = generate_line_id(i, line_content)

        new_index.append(
            {
                "id": line_id,
                "line_num": i,
                "content_hash": content_hash,
            }
        )

    return new_index, reused, len(lines) - reused


def format_output_with_line_ids(lines: list[str], index: list[dict]) -> str:
    """Format lines with line_num:lid│ prefixes."""
    output_lines = [
        "# LINE:LID│ content — LIDs are stable references for edit_lines, not file content"
    ]
    for line_info, line_content in zip(index, lines):
        output_lines.append(
            f"{line_info['line_num']}:{line_info['id']}│ {line_content}"
        )
    return "\n".join(output_lines)


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    file_path = Path(params["path"])
    offset = params.get("offset", 1)
    limit = params.get("limit", 2000)

    if not file_path.is_absolute():
        file_path = project / file_path
    file_path = file_path.resolve()

    if not file_path.is_relative_to(project):
        return {"success": False, "error": "Path is outside the project workspace"}

    if not file_path.exists():
        return {"success": False, "error": f"File not found: {file_path}"}

    if file_path.is_dir():
        return {"success": False, "error": "Path is a directory, not a file"}

    try:
        content = file_path.read_text()
        all_lines = content.splitlines()
        total_lines = len(all_lines)

        start_idx = max(0, offset - 1)
        end_idx = min(total_lines, start_idx + limit)
        lines = all_lines[start_idx:end_idx]

        cache_path = get_line_index_path(file_path, project)
        cached_index = load_cached_index(cache_path)

        current_content_hash = compute_content_hash(content)
        if cached_index and cached_index.get("content_hash") == current_content_hash:
            cached_lines = {l["line_num"]: l for l in cached_index.get("lines", [])}
            index = [
                cached_lines.get(
                    i + 1,
                    {
                        "id": generate_line_id(i + 1, line),
                        "line_num": i + 1,
                        "content_hash": hashlib.sha256(line.encode()).hexdigest(),
                    },
                )
                for i, line in enumerate(lines)
            ]
            for i, line in enumerate(lines):
                if (start_idx + i + 1) not in cached_lines:
                    index[i] = {
                        "id": generate_line_id(start_idx + i + 1, line),
                        "line_num": start_idx + i + 1,
                        "content_hash": hashlib.sha256(line.encode()).hexdigest(),
                    }
        else:
            full_index, reused, new_count = reconcile_line_index(
                all_lines, cached_index
            )
            index = full_index[start_idx:end_idx]

            new_cache = {
                "file_path": str(file_path.relative_to(project)),
                "content_hash": current_content_hash,
                "last_modified": datetime.now(timezone.utc).isoformat(),
                "lines": full_index,
            }
            save_cached_index(cache_path, new_cache)

        output = format_output_with_line_ids(lines, index)

        truncated = False
        if len(output) > MAX_TOTAL_BYTES:
            output = output[:MAX_TOTAL_BYTES]
            truncated = True

        return {
            "success": True,
            "output": output,
            "line_count": len(lines),
            "total_lines": total_lines,
            "truncated": truncated,
            "offset": offset,
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
