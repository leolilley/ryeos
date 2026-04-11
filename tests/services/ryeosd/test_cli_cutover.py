"""Tests that CLI verbs use daemon HTTP, not ExecuteTool."""

import ast
import importlib
from pathlib import Path

import pytest

from conftest import PROJECT_ROOT

CLI_VERBS = PROJECT_ROOT / "ryeos-cli" / "rye_cli" / "verbs"


class TestNoExecuteToolImports:
    """Guard: no CLI verb may import ExecuteTool."""

    @pytest.mark.parametrize("verb_file", [
        "execute.py", "thread.py", "graph.py", "remote.py",
        "test.py", "install.py",
    ])
    def test_no_execute_tool_import(self, verb_file):
        verb_path = CLI_VERBS / verb_file
        if not verb_path.exists():
            pytest.skip(f"{verb_file} does not exist yet")
        source = verb_path.read_text()
        tree = ast.parse(source)
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom):
                if node.module and "execute" in node.module:
                    names = [alias.name for alias in node.names]
                    assert "ExecuteTool" not in names, (
                        f"{verb_file} imports ExecuteTool — must use daemon_execute"
                    )


class TestDaemonExecuteFunction:
    """Verify daemon_execute exists and has the right signature."""

    def test_output_has_daemon_execute(self):
        from rye_cli.output import daemon_execute
        assert callable(daemon_execute)

    def test_daemon_url_default(self):
        import os
        os.environ.pop("RYEOSD_URL", None)
        from rye_cli.output import daemon_url
        assert daemon_url() == "http://127.0.0.1:7400"

    def test_daemon_url_override(self, monkeypatch):
        monkeypatch.setenv("RYEOSD_URL", "http://custom:9999")
        from rye_cli.output import daemon_url
        assert daemon_url() == "http://custom:9999"
