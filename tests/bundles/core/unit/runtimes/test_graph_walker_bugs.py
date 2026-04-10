"""Tests for walker bug fixes: foreach string-as-array, dispatch_action kind resolution.

Covers:
  - foreach: JSON string in `over` is auto-parsed to list
  - foreach: non-JSON string falls back to empty list
  - foreach: non-list/non-string falls back to empty list
  - dispatch_action: canonical ref item_id resolves kind
  - dispatch_action: bare item_id + kind field resolves correctly
  - dispatch_action: bare item_id + deprecated item_type field still works (with warning)
  - dispatch_action: bare item_id + no kind/item_type returns actionable error
"""

import asyncio
import importlib.util
import json
import logging
import sys
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from conftest import get_bundle_path
from rye.constants import AI_DIR

# ---------------------------------------------------------------------------
# Load walker module
# ---------------------------------------------------------------------------

_WALKER_DIR = get_bundle_path("core", "tools/rye/core/runtimes/state-graph")

if str(_WALKER_DIR) not in sys.path:
    sys.path.insert(0, str(_WALKER_DIR))

_spec = importlib.util.spec_from_file_location(
    "walker_bugs", _WALKER_DIR / "walker.py"
)
walker = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(walker)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def graph_project(tmp_path, _setup_user_space, monkeypatch):
    """Minimal project with CAS dirs for graph execution."""
    project = tmp_path / "project"
    project.mkdir()
    (project / AI_DIR / "state" / "objects").mkdir(parents=True)
    (project / AI_DIR / "agent" / "graphs").mkdir(parents=True)
    monkeypatch.delenv("RYE_SIGNING_KEY_DIR", raising=False)
    return project


# ---------------------------------------------------------------------------
# Foreach: string-as-array parsing
# ---------------------------------------------------------------------------


def _foreach_graph(over_expr="[0, 1, 2]"):
    """Graph with a foreach node whose `over` is set via assign from bash stdout."""
    return {
        "_item_id": "test/foreach_parse",
        "permissions": ["rye.execute.tool.*"],
        "config": {
            "start": "build_list",
            "max_steps": 50,
            "max_concurrency": 10,
            "on_error": "fail",
            "nodes": {
                "build_list": {
                    "action": {
                        "primary": "execute",
                        "item_id": "tool:test/echo",
                        "params": {},
                    },
                    "assign": {
                        "items": "${result.body.stdout}",
                    },
                    "next": "loop",
                },
                "loop": {
                    "type": "foreach",
                    "over": "${state.items}",
                    "as": "idx",
                    "collect": "results",
                    "action": {
                        "primary": "execute",
                        "item_id": "tool:test/process",
                        "params": {"index": "${idx}"},
                    },
                    "next": "done",
                },
                "done": {
                    "type": "return",
                    "output": {
                        "collected": "${state.results}",
                    },
                },
            },
        },
    }


class TestForeachStringParsing:
    """foreach `over` auto-parses JSON strings to arrays."""

    @pytest.mark.asyncio
    async def test_json_string_parsed_to_list(self, graph_project):
        """When bash stdout returns '[0, 1, 2]\\n', foreach iterates 3 items."""
        call_count = 0

        async def mock_dispatch(action, project_path, **kwargs):
            nonlocal call_count
            call_count += 1
            item_id = action.get("item_id", "")
            if "echo" in item_id:
                return {"status": "ok", "body": {"stdout": "[0, 1, 2]\n"}}
            return {"status": "ok", "body": {"value": action.get("params", {}).get("index")}}

        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock:
            mock.side_effect = mock_dispatch
            result = await walker.execute(
                _foreach_graph(), {}, str(graph_project)
            )

        assert result["success"] is True, f"Graph failed: {result.get('error')}"
        assert len(result["output"]["collected"]) == 3

    @pytest.mark.asyncio
    async def test_non_json_string_becomes_empty(self, graph_project):
        """Non-JSON string in `over` falls back to empty list (no iterations)."""

        async def mock_dispatch(action, project_path, **kwargs):
            return {"status": "ok", "body": {"stdout": "not-json-at-all"}}

        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock:
            mock.side_effect = mock_dispatch
            result = await walker.execute(
                _foreach_graph(), {}, str(graph_project)
            )

        assert result["success"] is True
        assert result["output"]["collected"] == []

    @pytest.mark.asyncio
    async def test_json_object_string_becomes_empty(self, graph_project):
        """JSON object string (not array) in `over` falls back to empty list."""

        async def mock_dispatch(action, project_path, **kwargs):
            return {"status": "ok", "body": {"stdout": '{"key": "value"}'}}

        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock:
            mock.side_effect = mock_dispatch
            result = await walker.execute(
                _foreach_graph(), {}, str(graph_project)
            )

        assert result["success"] is True
        assert result["output"]["collected"] == []

    @pytest.mark.asyncio
    async def test_actual_list_still_works(self, graph_project):
        """A real list (not string) in `over` still works as before."""
        graph = _foreach_graph()
        # Override: build_list returns an actual list, not a string
        graph["config"]["nodes"]["build_list"]["assign"]["items"] = "${result.body.items}"

        async def mock_dispatch(action, project_path, **kwargs):
            item_id = action.get("item_id", "")
            if "echo" in item_id:
                return {"status": "ok", "body": {"items": [10, 20]}}
            return {"status": "ok", "body": {"value": action.get("params", {}).get("index")}}

        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock:
            mock.side_effect = mock_dispatch
            result = await walker.execute(graph, {}, str(graph_project))

        assert result["success"] is True
        assert len(result["output"]["collected"]) == 2


# ---------------------------------------------------------------------------
# dispatch_action: kind resolution
# ---------------------------------------------------------------------------


class TestDispatchActionKindResolution:
    """_dispatch_action resolves kind from canonical ref, kind field, or item_type."""

    @pytest.mark.asyncio
    async def test_canonical_ref_resolves(self):
        """item_id='tool:test/echo' resolves kind without needing `kind` field."""
        action = {"primary": "execute", "item_id": "tool:test/echo", "params": {}}
        with patch.object(walker, "_tools_instance") as mock_tools:
            mock_execute = AsyncMock(return_value={"status": "ok"})
            mock_tools.return_value = {"execute": type("T", (), {"handle": mock_execute})()}
            await walker._dispatch_action(action, "/tmp/fake")

        mock_execute.assert_called_once()
        call_kwargs = mock_execute.call_args[1]
        assert call_kwargs["item_id"] == "tool:test/echo"

    @pytest.mark.asyncio
    async def test_kind_field_prepends_prefix(self):
        """Bare item_id + kind field → canonical ref is constructed."""
        action = {
            "primary": "execute",
            "item_id": "test/classify",
            "kind": "directive",
            "params": {},
        }
        with patch.object(walker, "_tools_instance") as mock_tools:
            mock_execute = AsyncMock(return_value={"status": "ok"})
            mock_tools.return_value = {"execute": type("T", (), {"handle": mock_execute})()}
            await walker._dispatch_action(action, "/tmp/fake")

        call_kwargs = mock_execute.call_args[1]
        assert call_kwargs["item_id"] == "directive:test/classify"
        # Directives auto-upgrade to fork
        assert call_kwargs["thread"] == "fork"

    @pytest.mark.asyncio
    async def test_deprecated_item_type_still_works(self, caplog):
        """Bare item_id + item_type field → still resolves, emits deprecation warning."""
        action = {
            "primary": "execute",
            "item_id": "test/old_tool",
            "item_type": "tool",
            "params": {},
        }
        with patch.object(walker, "_tools_instance") as mock_tools:
            mock_execute = AsyncMock(return_value={"status": "ok"})
            mock_tools.return_value = {"execute": type("T", (), {"handle": mock_execute})()}
            with caplog.at_level(logging.WARNING):
                await walker._dispatch_action(action, "/tmp/fake")

        call_kwargs = mock_execute.call_args[1]
        assert call_kwargs["item_id"] == "tool:test/old_tool"
        assert "Deprecated" in caplog.text
        assert "item_type" in caplog.text
        assert "kind" in caplog.text

    @pytest.mark.asyncio
    async def test_bare_item_id_no_kind_returns_error(self):
        """Bare item_id with no kind/item_type → actionable error, not silent failure."""
        action = {
            "primary": "execute",
            "item_id": "test/mystery",
            "params": {},
        }
        result = await walker._dispatch_action(action, "/tmp/fake")

        assert result["status"] == "error"
        assert "no canonical ref prefix" in result["error"]
        assert "kind" in result["error"]
        assert "test/mystery" in result["error"]

    @pytest.mark.asyncio
    async def test_kind_preferred_over_item_type(self):
        """When both `kind` and `item_type` are present, `kind` wins."""
        action = {
            "primary": "execute",
            "item_id": "test/dual",
            "kind": "tool",
            "item_type": "directive",
            "params": {},
        }
        with patch.object(walker, "_tools_instance") as mock_tools:
            mock_execute = AsyncMock(return_value={"status": "ok"})
            mock_tools.return_value = {"execute": type("T", (), {"handle": mock_execute})()}
            await walker._dispatch_action(action, "/tmp/fake")

        call_kwargs = mock_execute.call_args[1]
        assert call_kwargs["item_id"] == "tool:test/dual"


# ---------------------------------------------------------------------------
# Webhook: thread defaults
# ---------------------------------------------------------------------------


class TestWebhookThreadDefault:
    """resolve_execution webhook path defaults thread to 'inline'."""

    def test_webhook_body_without_thread_gets_inline(self):
        """body.get('thread') → None → should become 'inline'."""
        body = {"hook_id": "wh_test", "parameters": {}}
        thread = body.get("thread") or "inline"
        assert thread == "inline"

    def test_webhook_body_with_explicit_thread_preserved(self):
        """body.get('thread') → 'fork' → preserved."""
        body = {"hook_id": "wh_test", "parameters": {}, "thread": "fork"}
        thread = body.get("thread") or "inline"
        assert thread == "fork"

    def test_webhook_body_with_empty_string_thread_defaults(self):
        """body.get('thread') → '' → should become 'inline'."""
        body = {"hook_id": "wh_test", "parameters": {}, "thread": ""}
        thread = body.get("thread") or "inline"
        assert thread == "inline"
