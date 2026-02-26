# rye:signed:2026-02-26T05:52:24Z:0d26dc604cf0ceddb70ba92891972bdc65402f1abff4f3ab43d1a04d1d25b83f:Y5XxOxRr8JqQu_h57W96NG71zrAw9Sf2ZP3_vJuP-UzDqQhjpzXbQuoV4cfDywYITU4J8DEeVHSDRzxkMULTDg==:4b987fd4e40303ac
"""Execute shell commands."""

import argparse
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/bash"
__tool_description__ = "Execute shell commands"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "command": {
            "type": "string",
            "description": "Shell command to execute",
        },
        "timeout": {
            "type": "integer",
            "description": "Timeout in seconds (default: 120)",
            "default": 120,
        },
        "working_dir": {
            "type": "string",
            "description": "Working directory (default: project root)",
        },
    },
    "required": ["command"],
}

MAX_OUTPUT_BYTES = 51200
DEFAULT_TIMEOUT = 120


def truncate_output(output: str, max_bytes: int) -> tuple[str, bool]:
    """Truncate output if it exceeds max bytes.

    Returns:
        (truncated_output, was_truncated)
    """
    encoded = output.encode("utf-8", errors="replace")
    if len(encoded) <= max_bytes:
        return output, False

    truncated_bytes = encoded[:max_bytes]
    truncated_str = truncated_bytes.decode("utf-8", errors="replace")

    truncation_msg = f"\n... [output truncated, {len(encoded)} bytes total]"
    return truncated_str + truncation_msg, True


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    command = params["command"]
    timeout = params.get("timeout", DEFAULT_TIMEOUT)
    working_dir = params.get("working_dir")

    if working_dir:
        work_path = Path(working_dir)
        if not work_path.is_absolute():
            work_path = project / work_path
        work_path = work_path.resolve()

        if not work_path.is_relative_to(project):
            return {
                "success": False,
                "error": "Working directory is outside the project workspace",
            }

        if not work_path.exists():
            return {
                "success": False,
                "error": f"Working directory not found: {work_path}",
            }
    else:
        work_path = project

    try:
        result = subprocess.run(
            command,
            shell=True,
            capture_output=True,
            text=True,
            cwd=str(work_path),
            timeout=timeout,
        )

        stdout = result.stdout or ""
        stderr = result.stderr or ""

        stdout, stdout_truncated = truncate_output(stdout, MAX_OUTPUT_BYTES)
        stderr, stderr_truncated = truncate_output(stderr, MAX_OUTPUT_BYTES)

        success = result.returncode == 0

        output_parts = []
        if stdout:
            output_parts.append(stdout)
        if stderr:
            output_parts.append(f"[stderr]\n{stderr}")

        combined_output = "\n".join(output_parts)

        return {
            "success": success,
            "output": combined_output,
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": result.returncode,
            "truncated": stdout_truncated or stderr_truncated,
        }
    except subprocess.TimeoutExpired:
        return {
            "success": False,
            "error": f"Command timed out after {timeout} seconds",
            "timeout": timeout,
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
