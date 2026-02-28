# rye:signed:2026-02-27T23:55:31Z:32ef2396503910abfd012d02c7ee24febd12c8caefc6c7bc0eecf7dffe4aca4f:0cd-56QLmwif3IKMgFv40asTVW-0Y7BL4LwBroPRsScvJwyuavloAMNwAEz4retwBkRuCDL2nU9HHxxEcoT2DQ==:4b987fd4e40303ac
"""Rye tool test runner — execute .test.yaml specs against real tools.

Discovers test specs from .ai/tests/**/*.test.yaml, executes tools via
ExecuteTool.handle(), and evaluates assertions using condition_evaluator
operators (resolve_path + apply_operator).

Invocation:
    rye execute tool rye/dev/test-runner --params '{"tool": "my/tool"}'

Output:
    stdout: structured JSON (summary + per-test results)
    stderr: streaming progress lines per test case
"""

import argparse
import asyncio
import json
import logging
import os
import re
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import yaml

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/dev"
__tool_description__ = "Run .test.yaml specs against Rye tools"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "tool": {
            "type": "string",
            "description": "Tool ID to test (e.g., 'my-project/scrapers/chart-discovery'). "
            "If omitted, discovers and runs all test specs.",
        },
        "spec": {
            "type": "string",
            "description": "Path to a specific .test.yaml file (relative to project root). "
            "Overrides tool-based discovery.",
        },
        "include_tags": {
            "type": "string",
            "description": "Comma-separated tags to include (only run tests with these tags).",
        },
        "exclude_tags": {
            "type": "string",
            "description": "Comma-separated tags to exclude (skip tests with these tags).",
        },
        "validate_only": {
            "type": "boolean",
            "description": "Validate specs and tool resolution without executing. Default: false.",
            "default": False,
        },
    },
}

logger = logging.getLogger(__name__)

# Quiet noisy loggers during test execution
logging.getLogger("rye").setLevel(logging.WARNING)
logging.getLogger("lillux").setLevel(logging.WARNING)

AI_DIR = ".ai"


# ---------------------------------------------------------------------------
# Assertion parsing & evaluation
# ---------------------------------------------------------------------------

# "path op value" — e.g. "result.count >= 1"
_COMPARISON_RE = re.compile(
    r"^(.+?)\s+(==|!=|>=|<=|>|<|contains|regex|exists|in)\s+(.+)$"
)

_OP_MAP = {
    "==": "eq",
    "!=": "ne",
    ">": "gt",
    ">=": "gte",
    "<": "lt",
    "<=": "lte",
    "contains": "contains",
    "regex": "regex",
    "exists": "exists",
    "in": "in",
}


def _resolve_path(doc: Any, path: str) -> Any:
    """Resolve a dotted path in a nested dict/list structure.

    Same logic as condition_evaluator.resolve_path — reimplemented here
    to avoid anchor/PYTHONPATH dependency on the runtime lib bundle.
    """
    if not path:
        return doc
    parts = path.split(".")
    current = doc
    for part in parts:
        if isinstance(current, dict):
            current = current.get(part)
        elif isinstance(current, list):
            try:
                current = current[int(part)]
            except (ValueError, IndexError):
                return None
        else:
            return None
    return current


def _apply_operator(actual: Any, op: str, expected: Any) -> bool:
    """Apply a comparison operator — same semantics as condition_evaluator."""
    ops = {
        "eq": lambda a, e: a == e,
        "ne": lambda a, e: a != e,
        "gt": lambda a, e: a is not None and a > e,
        "gte": lambda a, e: a is not None and a >= e,
        "lt": lambda a, e: a is not None and a < e,
        "lte": lambda a, e: a is not None and a <= e,
        "in": lambda a, e: a in e if isinstance(e, (list, tuple, set)) else False,
        "contains": lambda a, e: e in str(a) if a is not None else False,
        "regex": lambda a, e: bool(re.search(e, str(a))) if a is not None else False,
        "exists": lambda a, e: a is not None,
    }
    return ops.get(op, lambda a, e: False)(actual, expected)


def _coerce_value(raw: str) -> Any:
    """Coerce a string token from an assertion expression to a typed value."""
    if raw in ("true", "True"):
        return True
    if raw in ("false", "False"):
        return False
    if raw in ("null", "None", "none"):
        return None
    try:
        return int(raw)
    except ValueError:
        pass
    try:
        return float(raw)
    except ValueError:
        pass
    if len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in ('"', "'"):
        return raw[1:-1]
    return raw


def _parse_assertion(key: str, value: Any) -> Tuple[str, str, Any, Any]:
    """Parse an assertion key into (path, op, expected, bool_outcome).

    Two forms:
      1. Simple — key is a dotted path, value is the expected value:
         "success": true  →  path="success", op="eq", expected=True, bool_outcome=None

      2. Expression — key contains an operator, value is expected boolean outcome:
         "result.count >= 1": true  →  path="result.count", op="gte", expected=1, bool_outcome=True
    """
    match = _COMPARISON_RE.match(key)
    if match:
        path = match.group(1).strip()
        op = _OP_MAP[match.group(2)]
        expected = _coerce_value(match.group(3).strip())
        return path, op, expected, value
    return key, "eq", value, None


def evaluate_assertions(
    assertions: Dict[str, Any], doc: Dict,
) -> List[Dict[str, Any]]:
    """Evaluate all assertions against an execution result document.

    Returns list of assertion result dicts.
    """
    results = []
    for key, value in assertions.items():
        path, op, expected, bool_outcome = _parse_assertion(key, value)
        actual = _resolve_path(doc, path)

        if bool_outcome is not None:
            # Expression form: evaluate operator, check against expected bool
            op_result = _apply_operator(actual, op, expected)
            passed = op_result == bool_outcome
            results.append({
                "expr": key,
                "passed": passed,
                "actual": actual,
                "expected": expected,
                "op": op,
            })
        else:
            # Simple form: direct equality
            passed = _apply_operator(actual, op, expected)
            results.append({
                "expr": f"{key} == {expected!r}",
                "passed": passed,
                "actual": actual,
                "expected": expected,
                "op": op,
            })
    return results


# ---------------------------------------------------------------------------
# Test spec discovery
# ---------------------------------------------------------------------------


def _discover_specs(
    project_path: Path,
    tool_filter: Optional[str] = None,
) -> List[Path]:
    """Discover .test.yaml files under .ai/tests/."""
    tests_dir = project_path / AI_DIR / "tests"
    if not tests_dir.exists():
        return []

    specs = sorted(tests_dir.rglob("*.test.yaml"))

    if tool_filter:
        # Filter to specs whose 'tool' field matches
        filtered = []
        for spec_path in specs:
            try:
                content = yaml.safe_load(spec_path.read_text(encoding="utf-8"))
                if content and content.get("tool") == tool_filter:
                    filtered.append(spec_path)
            except Exception:
                continue
        return filtered

    return specs


def _load_spec(spec_path: Path) -> Dict:
    """Load and validate a test spec YAML file."""
    content = yaml.safe_load(spec_path.read_text(encoding="utf-8"))
    if not isinstance(content, dict):
        raise ValueError(f"Invalid test spec: {spec_path} — expected a YAML dict")
    if "tool" not in content:
        raise ValueError(f"Test spec missing 'tool' field: {spec_path}")
    if "tests" not in content or not isinstance(content["tests"], list):
        raise ValueError(f"Test spec missing 'tests' list: {spec_path}")
    return content


def _filter_tests(
    tests: List[Dict],
    include_tags: Optional[set] = None,
    exclude_tags: Optional[set] = None,
) -> List[Dict]:
    """Filter test cases by tag inclusion/exclusion."""
    filtered = []
    for test in tests:
        tags = set(test.get("tags", []))
        if exclude_tags and tags & exclude_tags:
            continue
        if include_tags and not (tags & include_tags):
            continue
        filtered.append(test)
    return filtered


# ---------------------------------------------------------------------------
# Execution result → assertion document
# ---------------------------------------------------------------------------

_DROP_KEYS = frozenset(("chain", "metadata", "path", "source", "resolved_env_keys"))


def _build_assertion_doc(raw_result: Dict) -> Dict:
    """Build the document that assertions run against.

    Unwraps the ExecuteTool envelope (same logic as walker._unwrap_result)
    and merges tool-level data fields to the top level for convenient access.
    """
    doc = {}

    # Lift success/error from envelope
    doc["success"] = raw_result.get("status") != "error"
    if raw_result.get("error"):
        doc["error"] = raw_result["error"]
    doc["duration_ms"] = raw_result.get("duration_ms", 0)

    # Get inner data
    inner = raw_result.get("data")
    if isinstance(inner, dict):
        # Merge inner data fields (the tool's actual output)
        for k, v in inner.items():
            if k not in _DROP_KEYS:
                doc[k] = v
        # Override success if inner reports it
        if "success" in inner:
            doc["success"] = inner["success"]
    elif inner is not None:
        doc["result"] = inner

    return doc


# ---------------------------------------------------------------------------
# Core runner
# ---------------------------------------------------------------------------


def _progress(msg: str) -> None:
    """Print progress to stderr."""
    if not os.environ.get("RYE_TEST_QUIET"):
        print(msg, file=sys.stderr, flush=True)


async def _run_single_test(
    test_case: Dict,
    tool_id: str,
    project_path: str,
    execute_tool: Any,
    validate_only: bool = False,
) -> Dict:
    """Run a single test case and return the result dict."""
    name = test_case.get("name", "unnamed")
    params = test_case.get("params", {})
    assertions_spec = test_case.get("assert", {})
    tags = test_case.get("tags", [])

    result = {
        "name": name,
        "tags": tags,
        "passed": False,
        "skipped": False,
        "duration_ms": 0,
        "assertions": [],
        "error": None,
    }

    if validate_only:
        # Just validate the spec structure, don't execute
        result["skipped"] = True
        result["passed"] = True
        return result

    start = time.time()
    try:
        raw = await execute_tool.handle(
            item_type="tool",
            item_id=tool_id,
            project_path=project_path,
            parameters=params,
        )
        elapsed_ms = (time.time() - start) * 1000
        result["duration_ms"] = round(elapsed_ms, 1)

        doc = _build_assertion_doc(raw)
        result["exec"] = doc

        if assertions_spec:
            assertion_results = evaluate_assertions(assertions_spec, doc)
            result["assertions"] = assertion_results
            result["passed"] = all(a["passed"] for a in assertion_results)
        else:
            # No assertions — pass if execution succeeded
            result["passed"] = doc.get("success", False)

    except Exception as e:
        elapsed_ms = (time.time() - start) * 1000
        result["duration_ms"] = round(elapsed_ms, 1)
        result["error"] = str(e)
        result["passed"] = False

    return result


async def run_spec(
    spec_path: Path,
    project_path: str,
    include_tags: Optional[set] = None,
    exclude_tags: Optional[set] = None,
    validate_only: bool = False,
) -> Dict:
    """Run all tests in a single spec file.

    Returns a summary dict with per-test results.
    """
    from rye.tools.execute import ExecuteTool
    from rye.utils.resolvers import get_user_space

    spec = _load_spec(spec_path)
    tool_id = spec["tool"]
    tests = spec["tests"]

    # Apply tag filters
    tests = _filter_tests(tests, include_tags, exclude_tags)
    skipped_count = len(spec["tests"]) - len(tests)

    execute_tool = ExecuteTool(str(get_user_space()))

    _progress(f"\n[test] {tool_id} ({spec_path.name})")
    _progress(f"[test] {len(tests)} test(s) to run" +
              (f", {skipped_count} filtered" if skipped_count else ""))

    start = time.time()
    results = []
    passed = 0
    failed = 0

    for test_case in tests:
        name = test_case.get("name", "unnamed")
        test_result = await _run_single_test(
            test_case, tool_id, project_path, execute_tool, validate_only,
        )
        results.append(test_result)

        if test_result["skipped"]:
            _progress(f"  ⏭ {name} (skipped)")
        elif test_result["passed"]:
            passed += 1
            _progress(f"  ✓ {name} ({test_result['duration_ms']:.0f}ms)")
        else:
            failed += 1
            detail = test_result.get("error", "")
            if not detail:
                # Show first failing assertion
                for a in test_result.get("assertions", []):
                    if not a["passed"]:
                        detail = f"{a['expr']} — got {a['actual']!r}"
                        break
            _progress(f"  ✗ {name} ({test_result['duration_ms']:.0f}ms) — {detail}")

    total_elapsed = (time.time() - start) * 1000

    summary = {
        "success": failed == 0,
        "tool": tool_id,
        "spec_path": str(spec_path),
        "summary": {
            "total": len(results),
            "passed": passed,
            "failed": failed,
            "skipped": skipped_count + sum(1 for r in results if r["skipped"]),
            "duration_ms": round(total_elapsed, 1),
        },
        "results": results,
    }

    icon = "✓" if failed == 0 else "✗"
    _progress(f"[test] {icon} {passed}/{len(results)} passed ({total_elapsed:.0f}ms)")

    return summary


# ---------------------------------------------------------------------------
# Main entry point
# ---------------------------------------------------------------------------


def execute(params: dict, project_path: str) -> dict:
    """Tool entry point — discovers and runs test specs."""
    tool_filter = params.get("tool")
    spec_path = params.get("spec")
    include_tags = params.get("include_tags")
    exclude_tags = params.get("exclude_tags")
    validate_only = params.get("validate_only", False)

    include_set = set(include_tags.split(",")) if include_tags else None
    exclude_set = set(exclude_tags.split(",")) if exclude_tags else None

    proj = Path(project_path)

    if spec_path:
        # Run a specific spec file
        spec = proj / spec_path
        if not spec.exists():
            return {"success": False, "error": f"Spec not found: {spec_path}"}
        specs = [spec]
    else:
        # Discover specs
        specs = _discover_specs(proj, tool_filter)
        if not specs:
            msg = f"No test specs found"
            if tool_filter:
                msg += f" for tool '{tool_filter}'"
            msg += f" in {proj / AI_DIR / 'tests'}"
            return {"success": False, "error": msg}

    # Run all specs
    all_summaries = []
    total_passed = 0
    total_failed = 0
    total_skipped = 0
    total_tests = 0

    for spec in specs:
        try:
            summary = asyncio.run(run_spec(
                spec, project_path, include_set, exclude_set, validate_only,
            ))
            all_summaries.append(summary)
            total_passed += summary["summary"]["passed"]
            total_failed += summary["summary"]["failed"]
            total_skipped += summary["summary"]["skipped"]
            total_tests += summary["summary"]["total"]
        except Exception as e:
            all_summaries.append({
                "success": False,
                "spec_path": str(spec),
                "error": str(e),
            })
            total_failed += 1

    if len(all_summaries) == 1:
        return all_summaries[0]

    return {
        "success": total_failed == 0,
        "summary": {
            "specs": len(all_summaries),
            "total": total_tests,
            "passed": total_passed,
            "failed": total_failed,
            "skipped": total_skipped,
        },
        "specs": all_summaries,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result, default=str))
