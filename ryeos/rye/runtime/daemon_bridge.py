"""Daemon-facing execution bridge for ryeosd v3.

This keeps the daemon contract as a single structured execute_item(request)
entrypoint while reusing the existing engine for the first inline tool_run path.
"""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from typing import Any

from rye.actions.execute import ExecuteTool
from rye.actions._search import clear_search_cache
from rye.runtime.daemon_rpc import ThreadLifecycleClient, daemon_runtime_context
from rye.utils.execution_context import ExecutionContext
from rye.utils.extensions import clear_extensions_cache
from rye.utils import path_utils
from rye.utils.path_utils import BundleInfo, get_signing_key_dir, get_user_space


def execute_item(request: dict[str, Any]) -> dict[str, Any]:
    """Execute a single daemon-owned request and return a structured completion."""
    kind = request.get("kind")
    if kind not in ("tool_run", "directive_run", "graph_run"):
        return _failed(
            "unsupported_kind",
            f"ryeosd bridge supports tool_run, directive_run, graph_run — got {kind!r}",
        )

    project_path = Path(request["project_path"]).resolve()
    system_spaces = _repo_system_spaces()
    path_utils._system_spaces_cache = system_spaces
    clear_search_cache()
    clear_extensions_cache()
    ctx = ExecutionContext(
        project_path=project_path,
        user_space=get_user_space(),
        signing_key_dir=get_signing_key_dir(),
        system_spaces=tuple(system_spaces),
    )
    lifecycle = ThreadLifecycleClient.from_request(request)
    tool = ExecuteTool(ctx=ctx)

    try:
        lifecycle.attach_process(
            request["thread_id"],
            os.getpid(),
            os.getpgid(0),
            metadata={"runtime": "python", "bridge": "daemon_bridge", "kind": kind},
        )
        execute_kwargs = {
            "item_id": request["item_ref"],
            "project_path": str(project_path),
            "parameters": request.get("parameters") or {},
            "thread": "inline",
            "async": False,
            "dry_run": False,
        }
        if request.get("model"):
            execute_kwargs["parameters"]["model"] = request["model"]
        with daemon_runtime_context(
            socket_path=request["runtime"]["socket_path"],
            thread_id=request["thread_id"],
            chain_root_id=request.get("chain_root_id"),
        ):
            result = asyncio.run(tool.handle(**execute_kwargs))
    except Exception as exc:
        return _failed("engine_exception", str(exc))

    return _completion_from_result(result)


def _completion_from_result(result: dict[str, Any]) -> dict[str, Any]:
    """Map an ExecuteTool result into the daemon execution completion contract."""
    status = result.get("status")
    data = result.get("data", {})

    # Extract cost from thread execution results (directives report cost in data)
    cost = {"turns": 0, "input_tokens": 0, "output_tokens": 0, "spend": 0.0}
    if isinstance(data, dict):
        thread_cost = data.get("cost") or {}
        if thread_cost:
            cost = {
                "turns": thread_cost.get("turns", 0),
                "input_tokens": thread_cost.get("input_tokens", 0),
                "output_tokens": thread_cost.get("output_tokens", 0),
                "spend": thread_cost.get("spend", 0.0),
            }

    if status == "success":
        return {
            "status": "completed",
            "result": data,
            "error": None,
            "artifacts": [],
            "final_cost": cost,
            "continuation_request": None,
            "metadata": {
                "engine_metadata": result.get("metadata", {}),
                "chain": result.get("chain", []),
            },
        }

    return {
        "status": "failed",
        "result": None,
        "error": {
            "code": "engine_error",
            "message": result.get("error", "execution failed"),
            "details": result,
        },
        "artifacts": [],
        "final_cost": cost,
        "continuation_request": None,
        "metadata": None,
    }


def _failed(code: str, message: str) -> dict[str, Any]:
    return {
        "status": "failed",
        "result": None,
        "error": {"code": code, "message": message},
        "artifacts": [],
        "final_cost": {
            "turns": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "spend": 0.0,
        },
        "continuation_request": None,
        "metadata": None,
    }


def _repo_system_spaces() -> list[BundleInfo]:
    repo_root = Path(__file__).resolve().parents[3]
    bundle_roots = [
        ("ryeos", repo_root / "ryeos/bundles/standard/ryeos_std"),
        ("ryeos-code", repo_root / "ryeos/bundles/code/ryeos_code"),
        ("ryeos-core", repo_root / "ryeos/bundles/core/ryeos_core"),
        ("ryeos-email", repo_root / "ryeos/bundles/email/ryeos_email"),
        ("ryeos-web", repo_root / "ryeos/bundles/web/ryeos_web"),
    ]

    spaces: list[BundleInfo] = []
    for bundle_id, root_path in bundle_roots:
        if not (root_path / ".ai").is_dir():
            continue
        spaces.append(
            BundleInfo(
                bundle_id=bundle_id,
                version="0.0.0",
                root_path=root_path,
                manifest_path=None,
                source="repo",
                categories=None,
            )
        )
    return spaces
