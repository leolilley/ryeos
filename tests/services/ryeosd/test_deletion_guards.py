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
        """No production code should reference registry.db.

        The V5.5 cli-impl swap replaced the Python `ryeos-cli/rye_cli/`
        package with a Rust crate at `ryeos-cli/src/`, so this guard now
        scans both Python (legacy `ryeos/rye/`) and Rust (`ryeos-cli/src/`)
        production trees.
        """
        targets = [
            (PROJECT_ROOT / "ryeos" / "rye", "*.py"),
            (PROJECT_ROOT / "ryeos-cli" / "src", "*.rs"),
        ]
        for root, pattern in targets:
            if not root.exists():
                continue
            for src_file in root.rglob(pattern):
                if "__pycache__" in str(src_file) or "/target/" in str(src_file):
                    continue
                source = src_file.read_text(errors="replace")
                assert "registry.db" not in source, (
                    f"{src_file.relative_to(PROJECT_ROOT)} references registry.db"
                )

    def test_cli_no_direct_execute_tool(self):
        """CLI sources must not instantiate ExecuteTool directly.

        Post cli-impl swap, the CLI is Rust under `ryeos-cli/src/`;
        scan it for the forbidden symbol. (Detailed structural checks
        live in `test_cli_cutover.py`; this is the deletion-guard
        sentinel that fires if anything resurrects the legacy name.)
        """
        cli_src = PROJECT_ROOT / "ryeos-cli" / "src"
        if not cli_src.exists():
            pytest.skip("ryeos-cli/src does not exist yet")
        for rs_file in cli_src.rglob("*.rs"):
            source = rs_file.read_text(errors="replace")
            assert "ExecuteTool" not in source, (
                f"{rs_file.relative_to(PROJECT_ROOT)} references ExecuteTool — "
                "the CLI must dispatch via HTTP, not in-process engine calls."
            )
