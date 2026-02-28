"""Tests for rye/dev/test_runner — assertion evaluation and spec handling."""

import sys
from pathlib import Path

import pytest

# Add the tool's directory to sys.path so we can import it directly
_TOOL_DIR = (
    Path(__file__).parent.parent.parent
    / "ryeos" / "bundles" / "standard" / "ryeos_std"
    / ".ai" / "tools" / "rye" / "dev"
)
if str(_TOOL_DIR) not in sys.path:
    sys.path.insert(0, str(_TOOL_DIR))

from test_runner import (
    _resolve_path,
    _apply_operator,
    _coerce_value,
    _parse_assertion,
    evaluate_assertions,
    _build_assertion_doc,
    _discover_specs,
    _load_spec,
    _filter_tests,
)


# ---------------------------------------------------------------------------
# resolve_path
# ---------------------------------------------------------------------------


class TestResolvePath:
    def test_simple_key(self):
        assert _resolve_path({"a": 1}, "a") == 1

    def test_nested_key(self):
        assert _resolve_path({"a": {"b": {"c": 42}}}, "a.b.c") == 42

    def test_list_index(self):
        assert _resolve_path({"items": [10, 20, 30]}, "items.1") == 20

    def test_nested_list_dict(self):
        doc = {"tasks": [{"name": "a"}, {"name": "b"}]}
        assert _resolve_path(doc, "tasks.1.name") == "b"

    def test_missing_key(self):
        assert _resolve_path({"a": 1}, "b") is None

    def test_missing_nested(self):
        assert _resolve_path({"a": {"b": 1}}, "a.c") is None

    def test_empty_path(self):
        doc = {"a": 1}
        assert _resolve_path(doc, "") == doc

    def test_list_out_of_bounds(self):
        assert _resolve_path({"items": [1]}, "items.5") is None


# ---------------------------------------------------------------------------
# apply_operator
# ---------------------------------------------------------------------------


class TestApplyOperator:
    def test_eq(self):
        assert _apply_operator(42, "eq", 42)
        assert not _apply_operator(42, "eq", 43)

    def test_ne(self):
        assert _apply_operator(42, "ne", 43)
        assert not _apply_operator(42, "ne", 42)

    def test_gt(self):
        assert _apply_operator(5, "gt", 3)
        assert not _apply_operator(3, "gt", 5)

    def test_gte(self):
        assert _apply_operator(5, "gte", 5)
        assert _apply_operator(5, "gte", 3)
        assert not _apply_operator(3, "gte", 5)

    def test_lt(self):
        assert _apply_operator(3, "lt", 5)
        assert not _apply_operator(5, "lt", 3)

    def test_lte(self):
        assert _apply_operator(5, "lte", 5)

    def test_contains(self):
        assert _apply_operator("hello world", "contains", "world")
        assert not _apply_operator("hello", "contains", "world")

    def test_regex(self):
        assert _apply_operator("error: timeout", "regex", r"error:\s+\w+")
        assert not _apply_operator("success", "regex", r"error")

    def test_exists(self):
        assert _apply_operator("anything", "exists", None)
        assert not _apply_operator(None, "exists", None)

    def test_in(self):
        assert _apply_operator("a", "in", ["a", "b", "c"])
        assert not _apply_operator("d", "in", ["a", "b"])

    def test_none_safety(self):
        assert not _apply_operator(None, "gt", 5)
        assert not _apply_operator(None, "contains", "x")


# ---------------------------------------------------------------------------
# coerce_value
# ---------------------------------------------------------------------------


class TestCoerceValue:
    def test_bool(self):
        assert _coerce_value("true") is True
        assert _coerce_value("false") is False

    def test_none(self):
        assert _coerce_value("null") is None
        assert _coerce_value("None") is None

    def test_int(self):
        assert _coerce_value("42") == 42
        assert _coerce_value("0") == 0

    def test_float(self):
        assert _coerce_value("3.14") == 3.14

    def test_quoted_string(self):
        assert _coerce_value('"hello"') == "hello"
        assert _coerce_value("'world'") == "world"

    def test_bare_string(self):
        assert _coerce_value("something") == "something"


# ---------------------------------------------------------------------------
# parse_assertion
# ---------------------------------------------------------------------------


class TestParseAssertion:
    def test_simple_form(self):
        path, op, expected, bool_outcome = _parse_assertion("success", True)
        assert path == "success"
        assert op == "eq"
        assert expected is True
        assert bool_outcome is None

    def test_expression_gte(self):
        path, op, expected, bool_outcome = _parse_assertion(
            "result.count >= 1", True
        )
        assert path == "result.count"
        assert op == "gte"
        assert expected == 1
        assert bool_outcome is True

    def test_expression_eq(self):
        path, op, expected, bool_outcome = _parse_assertion(
            "result.total == 0", True
        )
        assert path == "result.total"
        assert op == "eq"
        assert expected == 0

    def test_expression_contains(self):
        path, op, expected, bool_outcome = _parse_assertion(
            "error contains timeout", True
        )
        assert path == "error"
        assert op == "contains"
        assert expected == "timeout"

    def test_expression_regex(self):
        path, op, expected, bool_outcome = _parse_assertion(
            "output regex ^OK", True
        )
        assert path == "output"
        assert op == "regex"
        assert expected == "^OK"


# ---------------------------------------------------------------------------
# evaluate_assertions
# ---------------------------------------------------------------------------


class TestEvaluateAssertions:
    def test_simple_pass(self):
        doc = {"success": True, "count": 5}
        results = evaluate_assertions({"success": True, "count": 5}, doc)
        assert len(results) == 2
        assert all(r["passed"] for r in results)

    def test_simple_fail(self):
        doc = {"success": False}
        results = evaluate_assertions({"success": True}, doc)
        assert not results[0]["passed"]
        assert results[0]["actual"] is False
        assert results[0]["expected"] is True

    def test_expression_pass(self):
        doc = {"result": {"total_found": 3}}
        results = evaluate_assertions(
            {"result.total_found >= 1": True}, doc
        )
        assert results[0]["passed"]

    def test_expression_fail(self):
        doc = {"result": {"total_found": 0}}
        results = evaluate_assertions(
            {"result.total_found >= 1": True}, doc
        )
        assert not results[0]["passed"]

    def test_negated_expression(self):
        doc = {"error": None}
        results = evaluate_assertions({"error exists None": False}, doc)
        assert results[0]["passed"]

    def test_nested_path(self):
        doc = {"data": {"items": [{"name": "a"}, {"name": "b"}]}}
        results = evaluate_assertions({"data.items.0.name": "a"}, doc)
        assert results[0]["passed"]

    def test_mixed_assertions(self):
        doc = {"success": True, "count": 10, "status": "ok"}
        results = evaluate_assertions({
            "success": True,
            "count >= 5": True,
            "status contains ok": True,
        }, doc)
        assert all(r["passed"] for r in results)


# ---------------------------------------------------------------------------
# build_assertion_doc
# ---------------------------------------------------------------------------


class TestBuildAssertionDoc:
    def test_success_envelope(self):
        raw = {
            "status": "success",
            "type": "tool",
            "item_id": "my/tool",
            "data": {"stdout": "hello", "stderr": "", "exit_code": 0},
            "chain": [],
            "metadata": {},
        }
        doc = _build_assertion_doc(raw)
        assert doc["success"] is True
        assert doc["stdout"] == "hello"
        assert doc["exit_code"] == 0
        assert "chain" not in doc
        assert "metadata" not in doc

    def test_error_envelope(self):
        raw = {
            "status": "error",
            "error": "Tool not found",
            "data": None,
        }
        doc = _build_assertion_doc(raw)
        assert doc["success"] is False
        assert doc["error"] == "Tool not found"

    def test_inner_success_false(self):
        raw = {
            "status": "success",
            "data": {"success": False, "error": "bad input"},
        }
        doc = _build_assertion_doc(raw)
        assert doc["success"] is False
        assert doc["error"] == "bad input"

    def test_scalar_data(self):
        raw = {"status": "success", "data": 42}
        doc = _build_assertion_doc(raw)
        assert doc["result"] == 42


# ---------------------------------------------------------------------------
# spec discovery and loading
# ---------------------------------------------------------------------------


class TestDiscoverSpecs:
    def test_discover_all(self, tmp_path):
        tests_dir = tmp_path / ".ai" / "tests"
        tests_dir.mkdir(parents=True)
        (tests_dir / "a.test.yaml").write_text("tool: my/tool-a\ntests: []\n")
        (tests_dir / "b.test.yaml").write_text("tool: my/tool-b\ntests: []\n")

        specs = _discover_specs(tmp_path)
        assert len(specs) == 2

    def test_discover_by_tool(self, tmp_path):
        tests_dir = tmp_path / ".ai" / "tests"
        tests_dir.mkdir(parents=True)
        (tests_dir / "a.test.yaml").write_text("tool: my/tool-a\ntests: []\n")
        (tests_dir / "b.test.yaml").write_text("tool: my/tool-b\ntests: []\n")

        specs = _discover_specs(tmp_path, tool_filter="my/tool-a")
        assert len(specs) == 1
        assert specs[0].name == "a.test.yaml"

    def test_discover_nested(self, tmp_path):
        nested = tmp_path / ".ai" / "tests" / "scrapers"
        nested.mkdir(parents=True)
        (nested / "chart.test.yaml").write_text("tool: scrapers/chart\ntests: []\n")

        specs = _discover_specs(tmp_path)
        assert len(specs) == 1

    def test_empty_dir(self, tmp_path):
        assert _discover_specs(tmp_path) == []


class TestLoadSpec:
    def test_valid_spec(self, tmp_path):
        spec_file = tmp_path / "test.yaml"
        spec_file.write_text(
            "tool: my/tool\n"
            "tests:\n"
            "  - name: basic\n"
            "    params: {}\n"
            "    assert:\n"
            "      success: true\n"
        )
        spec = _load_spec(spec_file)
        assert spec["tool"] == "my/tool"
        assert len(spec["tests"]) == 1

    def test_missing_tool(self, tmp_path):
        spec_file = tmp_path / "test.yaml"
        spec_file.write_text("tests: []\n")
        with pytest.raises(ValueError, match="missing 'tool'"):
            _load_spec(spec_file)

    def test_missing_tests(self, tmp_path):
        spec_file = tmp_path / "test.yaml"
        spec_file.write_text("tool: my/tool\n")
        with pytest.raises(ValueError, match="missing 'tests'"):
            _load_spec(spec_file)


class TestFilterTests:
    def test_no_filter(self):
        tests = [{"name": "a"}, {"name": "b", "tags": ["slow"]}]
        assert _filter_tests(tests) == tests

    def test_exclude_tags(self):
        tests = [
            {"name": "a"},
            {"name": "b", "tags": ["integration"]},
            {"name": "c", "tags": ["slow"]},
        ]
        result = _filter_tests(tests, exclude_tags={"integration"})
        assert len(result) == 2
        assert result[0]["name"] == "a"
        assert result[1]["name"] == "c"

    def test_include_tags(self):
        tests = [
            {"name": "a"},
            {"name": "b", "tags": ["unit"]},
            {"name": "c", "tags": ["integration"]},
        ]
        result = _filter_tests(tests, include_tags={"unit"})
        assert len(result) == 1
        assert result[0]["name"] == "b"

    def test_both_filters(self):
        tests = [
            {"name": "a", "tags": ["unit"]},
            {"name": "b", "tags": ["unit", "slow"]},
            {"name": "c", "tags": ["integration"]},
        ]
        result = _filter_tests(
            tests, include_tags={"unit"}, exclude_tags={"slow"}
        )
        assert len(result) == 1
        assert result[0]["name"] == "a"
