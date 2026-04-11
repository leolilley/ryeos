"""Deletion guards: fail if legacy runtime authority paths are reintroduced.

These tests check that the old Python-owned lifecycle state paths
cannot be exercised. They should FAIL if someone accidentally
re-enables a deleted path.
"""

import importlib
from pathlib import Path

import pytest

from conftest import PROJECT_ROOT


class TestNoLegacyImports:
    """No production code should import deleted authority modules."""

    def test_thread_registry_is_dead(self):
        """ThreadRegistry must not be importable as a working class."""
        try:
            from rye.agent.threads.persistence.thread_registry import ThreadRegistry
            # If it imports, calling it must raise RuntimeError
            with pytest.raises(RuntimeError):
                ThreadRegistry("/tmp/dummy")
        except ImportError:
            pass  # Also acceptable — module may be fully deleted

    def test_budget_ledger_is_dead(self):
        """BudgetLedger must not be importable as a working class."""
        try:
            from rye.agent.threads.persistence.budgets import BudgetLedger
            with pytest.raises(RuntimeError):
                BudgetLedger("/tmp/dummy")
        except ImportError:
            pass

    def test_no_registry_db_in_production_code(self):
        """No file under ryeos/ or ryeos-cli/ should reference registry.db."""
        for search_dir in ["ryeos/rye", "ryeos-cli/rye_cli"]:
            root = PROJECT_ROOT / search_dir
            if not root.exists():
                continue
            for py_file in root.rglob("*.py"):
                if "__pycache__" in str(py_file):
                    continue
                source = py_file.read_text(errors="replace")
                assert "registry.db" not in source, (
                    f"{py_file.relative_to(PROJECT_ROOT)} references registry.db"
                )

    def test_cli_no_direct_execute_tool(self):
        """CLI verbs must not instantiate ExecuteTool directly."""
        cli_dir = PROJECT_ROOT / "ryeos-cli" / "rye_cli" / "verbs"
        if not cli_dir.exists():
            pytest.skip("CLI verbs directory does not exist yet")
        for py_file in cli_dir.glob("*.py"):
            if py_file.name.startswith("__"):
                continue
            # fetch.py and sign.py are allowed (policy operations)
            if py_file.name in ("fetch.py", "sign.py", "uninstall.py"):
                continue
            source = py_file.read_text()
            assert "ExecuteTool" not in source, (
                f"{py_file.name} still uses ExecuteTool — must use daemon_execute"
            )
