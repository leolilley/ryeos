"""Tests that the (Rust) `ryeos-cli` dispatches every execution over HTTP
to the daemon, with no in-process engine fallback.

Original intent: the V5.2 cutover replaced an in-process `ExecuteTool` path
in the Python `rye_cli` with a daemon-HTTP path. After the V5.5 cli-impl
swap, the entire CLI is now Rust (`ryeos-cli/src/`), so the guards below
read Rust source instead of Python AST. The contract being defended is
the same: the CLI must never execute items in-process — every dispatch
goes through `transport::http::post_json` to the running daemon, and a
missing daemon must fail fast (no silent in-process fallback).
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from conftest import PROJECT_ROOT

CLI_SRC = PROJECT_ROOT / "ryeos-cli" / "src"
DISPATCHER = CLI_SRC / "dispatcher.rs"


class TestRustCliExistsAndIsRust:
    """The Python `ryeos-cli/rye_cli/` package was deleted by the cli-impl
    swap. Make sure no file resurrects it under the same path (which would
    silently shadow the Rust binary on `pip install`)."""

    def test_python_rye_cli_package_is_gone(self):
        legacy = PROJECT_ROOT / "ryeos-cli" / "rye_cli"
        assert not legacy.exists(), (
            f"legacy Python package present at {legacy} — the V5.5 cli-impl "
            "swap replaced it with a Rust crate; remove the Python tree."
        )

    def test_rust_dispatcher_exists(self):
        assert DISPATCHER.exists(), f"missing {DISPATCHER}"


class TestNoInProcessExecution:
    """Regression guard: the dispatcher must not execute items via the
    engine in-process. Every dispatch path must go through HTTP transport."""

    FORBIDDEN_PATTERNS = [
        # Direct engine-level execution surfaces. If any of these names
        # appear in CLI sources, the CLI is bypassing the daemon.
        re.compile(r"\bExecuteTool\b"),
        re.compile(r"\bEngine::execute\b"),
        re.compile(r"\bryeos_engine::engine::Engine::execute\b"),
        re.compile(r"\bryeos_directive_runtime::\w*execute\w*\b"),
        re.compile(r"\bryeos_graph_runtime::\w*execute\w*\b"),
    ]

    def test_no_in_process_execute_in_dispatcher(self):
        source = DISPATCHER.read_text()
        for pat in self.FORBIDDEN_PATTERNS:
            assert not pat.search(source), (
                f"{DISPATCHER} matches forbidden pattern {pat.pattern!r} — "
                "the CLI must dispatch via HTTP, not in-process engine calls."
            )

    @pytest.mark.parametrize("subpath", [
        "verbs.rs",
        "arg_bind.rs",
        "transport/http.rs",
        "transport/signing.rs",
    ])
    def test_no_in_process_execute_in_module(self, subpath):
        path = CLI_SRC / subpath
        if not path.exists():
            pytest.skip(f"{subpath} does not exist (file layout changed?)")
        source = path.read_text()
        for pat in self.FORBIDDEN_PATTERNS:
            assert not pat.search(source), (
                f"{path} matches forbidden pattern {pat.pattern!r} — "
                "the CLI must dispatch via HTTP, not in-process engine calls."
            )


class TestHttpDispatchPathIsPresent:
    """Counter-positive: confirm the dispatcher really does route through
    HTTP. If this regresses (e.g. someone replaces `post_json` with a
    no-op), the negative guards above still pass — so we also assert the
    positive."""

    def test_dispatcher_calls_post_json(self):
        source = DISPATCHER.read_text()
        assert "transport::http::post_json" in source, (
            "dispatcher.rs no longer references `transport::http::post_json` — "
            "did the CLI lose its HTTP dispatch path?"
        )

    def test_dispatcher_reads_daemon_bind(self):
        source = DISPATCHER.read_text()
        assert "read_daemon_bind" in source, (
            "dispatcher.rs no longer reads daemon.json — did the CLI gain "
            "a silent in-process fallback?"
        )


class TestExecuteBodyIncludesProjectPath:
    """V5.5 contract: every `/execute` request body includes `project_path`.
    The daemon's typed body deserialization with `deny_unknown_fields` would
    reject anything else; this guard makes the contract visible at the
    source level so a regression is caught before it ships."""

    def test_dispatcher_includes_project_path(self):
        source = DISPATCHER.read_text()
        # `project_path` should appear in every JSON body the dispatcher
        # constructs. Conservatively require at least two occurrences (the
        # explicit-`execute`-escape-hatch path and the verb-table path).
        count = source.count('"project_path"')
        assert count >= 2, (
            f"dispatcher.rs only constructs {count} body(s) with "
            "`project_path` — both the `rye execute <ref>` path and the "
            "verb-table path must include it."
        )
