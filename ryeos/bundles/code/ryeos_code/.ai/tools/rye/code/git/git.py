# rye:signed:2026-02-26T06:42:43Z:a23a1a0375614da586cad7d7dcaa6e27ebe4ddb16efad18e250643bd760bbf66:T3nOWnXiQGrJ01dYvkT5WAn2Nua2lrvgU-HsZissNpVkj4vasWwNC-wYc_3HGGxCgNSSU4_tin1UA-eQ2ooRCg==:4b987fd4e40303ac

"""Git operations - status, add, commit, diff, log, branch, checkout, stash, reset, tag."""

import argparse
import json
import subprocess
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/code/git"
__tool_description__ = "Git operations - status, add, commit, diff, log, branch, checkout, stash, reset, tag"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["status", "add", "commit", "diff", "log", "branch", "checkout", "stash", "reset", "tag"],
            "description": "Git action to perform",
        },
        "args": {
            "type": "array",
            "items": {"type": "string"},
            "default": [],
            "description": "Arguments for the action (file paths, branch names, etc.)",
        },
        "message": {
            "type": "string",
            "description": "Commit/tag message (required for commit action)",
        },
        "flags": {
            "type": "object",
            "default": {},
            "description": "Flags to pass (e.g. { staged: true, amend: true, create: true })",
        },
        "working_dir": {
            "type": "string",
            "description": "Working directory (relative to project root or absolute)",
        },
        "timeout": {
            "type": "integer",
            "default": 30,
            "description": "Timeout in seconds",
        },
    },
    "required": ["action"],
}

MAX_OUTPUT_BYTES = 51200
DEFAULT_TIMEOUT = 30


def truncate_output(output: str, max_bytes: int) -> tuple[str, bool]:
    encoded = output.encode("utf-8", errors="replace")
    if len(encoded) <= max_bytes:
        return output, False

    truncated_bytes = encoded[:max_bytes]
    truncated_str = truncated_bytes.decode("utf-8", errors="replace")

    truncation_msg = f"\n... [output truncated, {len(encoded)} bytes total]"
    return truncated_str + truncation_msg, True


def build_command(params: dict) -> list[str]:
    action = params["action"]
    args = params.get("args", [])
    flags = params.get("flags", {})
    message = params.get("message")

    match action:
        case "status":
            cmd = ["git", "status", "--porcelain"]
            if flags.get("long"):
                cmd = ["git", "status"]
            return cmd + args

        case "add":
            if not args:
                raise ValueError("add requires explicit file paths")
            blocked = {"-A", "--all", "."}
            for a in args:
                if a in blocked:
                    raise ValueError("Use explicit file paths instead of '-A', '--all', or '.'")
            return ["git", "add"] + args

        case "commit":
            if not message:
                raise ValueError("commit requires a message")
            cmd = ["git", "commit", "-m", message]
            if flags.get("no_verify"):
                cmd.append("--no-verify")
            if flags.get("amend"):
                cmd.append("--amend")
            return cmd

        case "diff":
            cmd = ["git", "diff"]
            if flags.get("staged") or flags.get("cached"):
                cmd.append("--staged")
            return cmd + args

        case "log":
            max_count = flags.get("max_count", 20)
            cmd = ["git", "log", "--oneline", f"-n{max_count}"]
            if flags.get("format"):
                cmd = ["git", "log", f"--format={flags['format']}", f"-n{max_count}"]
            return cmd + args

        case "branch":
            if flags.get("delete") and args:
                return ["git", "branch", "-d", args[0]]
            if flags.get("list") or not args:
                cmd = ["git", "branch"]
                if flags.get("all"):
                    cmd.append("-a")
                return cmd
            return ["git", "branch", args[0]]

        case "checkout":
            if not args:
                raise ValueError("checkout requires a branch or file path")
            cmd = ["git", "checkout"]
            if flags.get("create"):
                cmd.append("-b")
            return cmd + args

        case "stash":
            sub = args[0] if args else "push"
            valid = {"push", "pop", "list", "drop", "apply"}
            if sub not in valid:
                raise ValueError(f"Invalid stash subcommand: {sub}. Valid: {', '.join(sorted(valid))}")
            cmd = ["git", "stash", sub]
            return cmd + args[1:]

        case "reset":
            cmd = ["git", "reset"]
            if flags.get("hard"):
                cmd.append("--hard")
            elif flags.get("soft"):
                cmd.append("--soft")
            elif flags.get("mixed"):
                cmd.append("--mixed")
            return cmd + args

        case "tag":
            if flags.get("list") or (flags.get("delete") is None and not args):
                return ["git", "tag", "--list"]
            if flags.get("delete") and args:
                return ["git", "tag", "-d", args[0]]
            if not args:
                raise ValueError("tag requires a tag name")
            cmd = ["git", "tag"]
            if flags.get("message"):
                cmd.extend(["-a", args[0], "-m", flags["message"]])
            else:
                cmd.append(args[0])
            return cmd

        case _:
            raise ValueError(f"Unknown action: {action}")


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
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
        cmd = build_command(params)
    except ValueError as e:
        return {"success": False, "error": str(e)}

    try:
        result = subprocess.run(
            cmd,
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
