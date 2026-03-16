# rye:signed:2026-03-16T09:27:24Z:5e6339f087bab4cbc03374dbee278fadc4d81973673730ffe08b5ec75dd9f36b:B6VAJaRS-Lu9HsFI--aseX9wjB15E8sPc8nxy8ztuaYSE4oBK6NQpN0xUeW6Gihh6e9odAlfHCEQL3kjxbQQCw==:4b987fd4e40303ac
# rye:unsigned

"""Quality gate runner — executes project-configured quality checks and returns structured pass/fail."""

import json
import subprocess
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/code/quality"
__tool_description__ = "Run project-configured quality gates (lint, typecheck, test, coverage) and return structured pass/fail per gate"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "gates_config": {
            "type": "string",
            "description": "Path to quality_gates.yaml (default: .ai/config/quality_gates.yaml)",
        },
        "working_dir": {
            "type": "string",
            "description": "Working directory (relative to project root or absolute)",
        },
        "timeout": {
            "type": "integer",
            "default": 120,
            "description": "Timeout per gate command in seconds",
        },
    },
    "required": [],
}

MAX_OUTPUT_BYTES = 51200
DEFAULT_TIMEOUT = 120
DEFAULT_GATES_CONFIG = ".ai/config/quality_gates.yaml"


def truncate_output(output: str, max_bytes: int) -> tuple[str, bool]:
    encoded = output.encode("utf-8", errors="replace")
    if len(encoded) <= max_bytes:
        return output, False

    truncated_bytes = encoded[:max_bytes]
    truncated_str = truncated_bytes.decode("utf-8", errors="replace")

    truncation_msg = f"\n... [output truncated, {len(encoded)} bytes total]"
    return truncated_str + truncation_msg, True


def _load_gates_config(config_path: Path) -> list[dict]:
    if not config_path.exists():
        return []
    try:
        import yaml
    except ImportError:
        return _parse_simple_gates_yaml(config_path)
    with open(config_path, "r", encoding="utf-8") as f:
        data = yaml.safe_load(f) or {}
    return data.get("gates", [])


def _parse_simple_gates_yaml(config_path: Path) -> list[dict]:
    """Minimal YAML parser for gates config when PyYAML is unavailable."""
    gates = []
    current_gate = None
    with open(config_path, "r", encoding="utf-8") as f:
        for line in f:
            stripped = line.strip()
            if stripped.startswith("- name:"):
                if current_gate:
                    gates.append(current_gate)
                current_gate = {"name": stripped.split(":", 1)[1].strip().strip('"')}
            elif current_gate and stripped.startswith("command:"):
                current_gate["command"] = stripped.split(":", 1)[1].strip().strip('"')
            elif current_gate and stripped.startswith("required:"):
                val = stripped.split(":", 1)[1].strip().lower()
                current_gate["required"] = val in ("true", "yes")
    if current_gate:
        gates.append(current_gate)
    return gates


def _run_gate(gate: dict, work_path: Path, timeout: int) -> dict:
    name = gate.get("name", "unnamed")
    command = gate.get("command", "")
    required = gate.get("required", True)

    if not command:
        return {
            "name": name,
            "passed": False,
            "required": required,
            "error": "No command specified for gate",
        }

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
        stdout, _ = truncate_output(stdout, MAX_OUTPUT_BYTES // 4)
        stderr, _ = truncate_output(stderr, MAX_OUTPUT_BYTES // 4)

        output_parts = []
        if stdout:
            output_parts.append(stdout)
        if stderr:
            output_parts.append(f"[stderr]\n{stderr}")

        return {
            "name": name,
            "passed": result.returncode == 0,
            "required": required,
            "exit_code": result.returncode,
            "output": "\n".join(output_parts),
        }
    except subprocess.TimeoutExpired:
        return {
            "name": name,
            "passed": False,
            "required": required,
            "error": f"Timed out after {timeout} seconds",
        }
    except Exception as e:
        return {
            "name": name,
            "passed": False,
            "required": required,
            "error": str(e),
        }


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

    config_rel = params.get("gates_config", DEFAULT_GATES_CONFIG)
    config_path = Path(config_rel)
    if not config_path.is_absolute():
        config_path = project / config_path

    gates = _load_gates_config(config_path)
    if not gates:
        return {
            "success": False,
            "error": f"No gates configured. Create {DEFAULT_GATES_CONFIG} with gate definitions.",
        }

    results = []
    for gate in gates:
        result = _run_gate(gate, work_path, timeout)
        results.append(result)

    all_passed = all(r["passed"] for r in results)
    required_passed = all(r["passed"] for r in results if r.get("required", True))

    summary_parts = []
    for r in results:
        status = "PASS" if r["passed"] else "FAIL"
        req = " (required)" if r.get("required", True) else " (optional)"
        summary_parts.append(f"  {status} {r['name']}{req}")

    output = f"Quality Gates: {'ALL PASSED' if all_passed else 'FAILED'}\n"
    output += "\n".join(summary_parts)

    return {
        "success": required_passed,
        "output": output,
        "gates": results,
        "all_passed": all_passed,
        "required_passed": required_passed,
        "gates_checked": len(results),
    }
