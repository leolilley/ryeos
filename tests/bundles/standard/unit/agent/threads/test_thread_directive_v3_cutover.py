"""Tests for v3 thread_directive cutover behavior."""

import asyncio
import importlib.util

from conftest import get_bundle_path

THREAD_DIRECTIVE_PATH = get_bundle_path(
    "standard", "tools/rye/agent/threads/thread_directive.py"
)
_spec = importlib.util.spec_from_file_location("thread_directive_module", THREAD_DIRECTIVE_PATH)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)


def test_parent_thread_requires_explicit_parent_limits(tmp_path):
    result = asyncio.run(
        _mod.execute(
            {
                "directive_id": "test/example",
                "parent_thread_id": "T-parent",
            },
            str(tmp_path),
        )
    )

    assert result["success"] is False
    assert "parent_limits" in result["error"]


def test_continuation_inputs_are_disabled(tmp_path):
    result = asyncio.run(
        _mod.execute(
            {
                "directive_id": "test/example",
                "previous_thread_id": "T-old",
            },
            str(tmp_path),
        )
    )

    assert result["success"] is False
    assert "continued edges" in result["error"]
