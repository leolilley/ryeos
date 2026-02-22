# rye:signed:2026-02-22T09:00:56Z:83e5ac41d405a1bf18b3c99b5c7c418a80138b1f1b2c2d42ccdc0d96a20e0fce:Vhp7oZWx2bPv_MCUKcIx1X3Jsy89j9aIbE5OkZ07Cf2KpcfCPwUBqNP3n7biP2pi7QLXCK06UKDcLfF2qC6CDA==:9fbfabe975fa5a7f
"""Get LSP diagnostics for a file."""

import argparse
import json
import shutil
import subprocess
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
__category__ = "rye/lsp"
__tool_description__ = "Get LSP diagnostics for a file"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "file_path": {
            "type": "string",
            "description": "Path to file to get diagnostics for",
        },
        "linters": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Linters to use (ruff, mypy, pylint for Python). Auto-detected if not specified.",
        },
    },
    "required": ["file_path"],
}

LINTER_PRIORITY = ["ruff", "mypy", "pylint", "eslint", "tsc"]
MAX_OUTPUT_BYTES = 32768


def detect_file_type(file_path: Path) -> str | None:
    """Detect file type from extension."""
    ext = file_path.suffix.lower()
    type_map = {
        ".py": "python",
        ".js": "javascript",
        ".jsx": "javascript",
        ".ts": "typescript",
        ".tsx": "typescript",
        ".go": "go",
        ".rs": "rust",
        ".rb": "ruby",
        ".java": "java",
        ".kt": "kotlin",
        ".swift": "swift",
        ".c": "c",
        ".cpp": "cpp",
        ".h": "c",
        ".hpp": "cpp",
    }
    return type_map.get(ext)


def get_linters_for_type(file_type: str | None) -> list[str]:
    """Get available linters for file type."""
    if file_type == "python":
        return ["ruff", "mypy", "pylint", "flake8"]
    elif file_type in ("javascript", "typescript"):
        return ["eslint", "tsc"]
    elif file_type == "go":
        return ["golint", "go vet"]
    elif file_type == "rust":
        return ["cargo clippy", "rustc"]
    return []


def find_available_linters(linters: list[str], project_path: Path) -> list[str]:
    """Find which linters are available on the system or in venv."""
    available = []
    venv_bin = project_path / ".venv" / "bin"

    for linter in linters:
        linter_cmd = linter.split()[0]
        found = shutil.which(linter_cmd)
        if not found and venv_bin.exists():
            venv_linter = venv_bin / linter_cmd
            if venv_linter.exists():
                found = str(venv_linter)
        if found:
            available.append(linter)
    return available


def run_linter(linter: str, file_path: Path, project_path: Path) -> list[dict]:
    """Run a linter and parse output."""
    diagnostics = []

    try:
        cmd = []
        if linter == "ruff":
            cmd = ["ruff", "check", "--output-format=json", str(file_path)]
        elif linter == "mypy":
            cmd = ["mypy", "--no-error-summary", "--no-color-output", str(file_path)]
        elif linter == "pylint":
            cmd = ["pylint", "--output-format=json", str(file_path)]
        elif linter == "flake8":
            cmd = ["flake8", "--format=default", str(file_path)]
        elif linter == "eslint":
            cmd = ["eslint", "--format=json", str(file_path)]
        elif linter == "tsc":
            cmd = ["tsc", "--noEmit", "--pretty", "false", str(file_path)]
        else:
            return []

        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            cwd=str(project_path),
            timeout=30,
        )

        if linter == "ruff" and result.stdout:
            try:
                issues = json.loads(result.stdout)
                for issue in issues:
                    diagnostics.append(
                        {
                            "line": issue.get("location", {}).get("row", 0),
                            "column": issue.get("location", {}).get("column", 0),
                            "severity": (
                                "error"
                                if issue.get("severity", "") == "error"
                                else "warning"
                            ),
                            "message": issue.get("message", ""),
                            "code": issue.get("code", ""),
                        }
                    )
            except json.JSONDecodeError:
                pass

        elif linter == "mypy":
            import re

            pattern = r"(.+?):(\d+): (error|warning|note): (.+)"
            for line in result.stdout.split("\n"):
                match = re.match(pattern, line)
                if match:
                    diagnostics.append(
                        {
                            "line": int(match.group(2)),
                            "column": 0,
                            "severity": (
                                match.group(3) if match.group(3) != "note" else "info"
                            ),
                            "message": match.group(4),
                            "code": "",
                        }
                    )

        elif linter == "pylint" and result.stdout:
            try:
                issues = json.loads(result.stdout)
                for issue in issues:
                    diagnostics.append(
                        {
                            "line": issue.get("line", 0),
                            "column": issue.get("column", 0),
                            "severity": (
                                "error" if issue.get("type") == "error" else "warning"
                            ),
                            "message": issue.get("message", ""),
                            "code": issue.get("symbol", ""),
                        }
                    )
            except json.JSONDecodeError:
                pass

        elif linter in ("flake8",):
            import re

            pattern = r"(.+?):(\d+):(\d+): ([A-Z]\d+) (.+)"
            for line in result.stdout.split("\n"):
                match = re.match(pattern, line)
                if match:
                    diagnostics.append(
                        {
                            "line": int(match.group(2)),
                            "column": int(match.group(3)),
                            "severity": (
                                "error" if match.group(4).startswith("E") else "warning"
                            ),
                            "message": match.group(5),
                            "code": match.group(4),
                        }
                    )

        elif linter == "eslint" and result.stdout:
            try:
                data = json.loads(result.stdout)
                for file_result in data:
                    for msg in file_result.get("messages", []):
                        diagnostics.append(
                            {
                                "line": msg.get("line", 0),
                                "column": msg.get("column", 0),
                                "severity": msg.get("severity", "warning"),
                                "message": msg.get("message", ""),
                                "code": msg.get("ruleId", ""),
                            }
                        )
            except json.JSONDecodeError:
                pass

    except subprocess.TimeoutExpired:
        diagnostics.append(
            {
                "line": 0,
                "column": 0,
                "severity": "error",
                "message": f"{linter} timed out",
                "code": "timeout",
            }
        )
    except FileNotFoundError:
        pass
    except Exception as e:
        diagnostics.append(
            {
                "line": 0,
                "column": 0,
                "severity": "error",
                "message": f"{linter} error: {e}",
                "code": "error",
            }
        )

    return diagnostics


def format_diagnostics(diagnostics: list[dict], file_path: str) -> str:
    """Format diagnostics as text."""
    if not diagnostics:
        return f"No issues found in {file_path}"

    lines = []
    for d in sorted(diagnostics, key=lambda x: (x.get("line", 0), x.get("column", 0))):
        severity = d.get("severity", "warning")
        line_num = d.get("line", 0)
        col = d.get("column", 0)
        message = d.get("message", "")
        code = d.get("code", "")

        code_str = f" [{code}]" if code else ""
        col_str = f":{col}" if col else ""

        lines.append(
            f"{file_path}:{line_num}{col_str}: {severity}: {message}{code_str}"
        )

    return "\n".join(lines)


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    file_path = Path(params["file_path"])
    requested_linters = params.get("linters")

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
        file_type = detect_file_type(file_path)

        if requested_linters:
            linters = requested_linters
        else:
            linters = get_linters_for_type(file_type)

        available_linters = find_available_linters(linters, project)

        if not available_linters:
            return {
                "success": True,
                "output": f"No linters available for {file_type or 'unknown'} files",
                "diagnostics": [],
                "linters_checked": [],
            }

        all_diagnostics = []
        for linter in available_linters:
            diagnostics = run_linter(linter, file_path, project)
            all_diagnostics.extend(diagnostics)

        seen = set()
        unique_diagnostics = []
        for d in all_diagnostics:
            key = (d.get("line"), d.get("column"), d.get("message"))
            if key not in seen:
                seen.add(key)
                unique_diagnostics.append(d)

        try:
            relative_path = str(file_path.relative_to(project))
        except ValueError:
            relative_path = str(file_path)

        output = format_diagnostics(unique_diagnostics, relative_path)

        if len(output) > MAX_OUTPUT_BYTES:
            output = output[:MAX_OUTPUT_BYTES] + "\n... [output truncated]"

        return {
            "success": True,
            "output": output,
            "diagnostics": unique_diagnostics,
            "linters_checked": available_linters,
            "file_type": file_type,
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
