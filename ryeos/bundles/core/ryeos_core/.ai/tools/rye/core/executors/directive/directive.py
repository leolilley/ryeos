# rye:signed:2026-04-09T03:34:58Z:c5bc6adbd7f71bffcd3537b27b314dfa807fcf60a2f4a6409293780df2832da8:qiUW_Z8CYiselrGOhMU04kpIZaJMLq3od1LJVYNpITU2WKUdxov_5xEGNUm7QGMpfsEYvrhDI9pqvpoYuluiDQ:4b987fd4e40303ac
"""Directive executor — parse, validate, and return directive content.

Receives a generic envelope from the engine:
    {item_id, parameters, thread, async, dry_run}

Inline mode: parses the directive, validates/interpolates inputs,
returns ``your_directions`` with the processed body.

Fork mode: delegates to ``rye/agent/threads/thread_directive``
(requires the agent bundle).
"""

__version__ = "1.0.0"
__tool_type__ = "executor"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/executors/directive"
__tool_description__ = "Parse and dispatch directive execution"
__allowed_threads__ = ["inline", "fork"]
__allowed_targets__ = ["local", "remote"]


def execute(params: dict, project_path: str) -> dict:
    """Execute a directive."""
    import json
    import os
    from pathlib import Path

    from rye.constants import ItemType
    from rye.utils.parser_router import ParserRouter
    from rye.utils.processor_router import ProcessorRouter
    from rye.utils.path_utils import (
        get_project_kind_path,
        get_system_spaces,
        get_user_kind_path,
    )
    from rye.utils.extensions import get_item_extensions
    from rye.utils.integrity import verify_item, IntegrityError
    from rye.utils.execution_context import ExecutionContext
    from rye.constants import AI_DIR

    item_id = params.get("item_id", "")
    parameters = params.get("parameters", {})
    thread = params.get("thread", "inline")
    async_exec = params.get("async", False)
    dry_run = params.get("dry_run", False)

    if not item_id:
        return {"status": "error", "error": "item_id is required"}

    # Strip canonical prefix
    _, bare_id = ItemType.parse_canonical_ref(item_id)

    # Find directive file
    proj = Path(project_path)
    file_path = _find_directive(proj, bare_id)
    if not file_path:
        return {"status": "error", "error": f"Directive not found: {bare_id}"}

    # Verify integrity
    ctx = ExecutionContext.from_env(project_path=proj)
    try:
        verify_item(file_path, ItemType.DIRECTIVE, ctx=ctx)
    except IntegrityError as exc:
        return {"status": "error", "error": str(exc), "item_id": bare_id}

    # Parse
    parser_router = ParserRouter()
    content = file_path.read_text(encoding="utf-8")
    parsed = parser_router.parse("markdown/xml", content)
    if "error" in parsed:
        return {"status": "error", "error": parsed.get("error"), "item_id": bare_id}

    # Extract special parameters before validation
    model = parameters.pop("model", None)
    limit_overrides = parameters.pop("limit_overrides", None)
    previous_thread_id = parameters.pop("previous_thread_id", None)

    # Validate inputs
    processor_router = ProcessorRouter(ctx.project_path)
    validation = processor_router.run("inputs/validate", parsed, parameters)
    if validation.get("status") == "error":
        validation["item_id"] = bare_id
        return validation

    # Interpolate
    inputs = validation["inputs"]
    processor_router.run("inputs/interpolate", parsed, inputs)

    # Dry run
    if dry_run:
        return {
            "status": "validation_passed",
            "type": "directive",
            "item_id": bare_id,
            "data": parsed,
            "inputs": inputs,
            "message": "Directive validation passed (dry run)",
        }

    # Inline: return directions directly
    if thread == "inline":
        return {"your_directions": parsed.get("body", ""), "metadata": {}}

    # Fork: delegate to thread_directive
    if thread == "fork":
        return _dispatch_fork(
            bare_id=bare_id,
            inputs=inputs,
            model=model,
            limit_overrides=limit_overrides,
            previous_thread_id=previous_thread_id,
            async_exec=async_exec,
            project_path=project_path,
        )

    return {"status": "error", "error": f"Unknown thread mode: {thread!r}"}


def _dispatch_fork(
    *,
    bare_id: str,
    inputs: dict,
    model,
    limit_overrides,
    previous_thread_id,
    async_exec: bool,
    project_path: str,
) -> dict:
    """Delegate fork execution to thread_directive tool."""
    import asyncio
    from pathlib import Path

    from rye.constants import ItemType
    from rye.executor import PrimitiveExecutor
    from rye.utils.execution_context import ExecutionContext

    td_tool = "rye/agent/threads/thread_directive"
    ctx = ExecutionContext.from_env(project_path=Path(project_path))
    executor = PrimitiveExecutor(ctx=ctx)

    # Check if thread_directive is available
    chain = asyncio.get_event_loop().run_until_complete(
        executor._build_chain(td_tool)
    )
    if not chain:
        return {
            "status": "error",
            "error": (
                'thread="fork" requires the rye/agent thread infrastructure '
                f"(tool '{td_tool}' not found). "
                'Either install the rye-agent package or use thread="inline".'
            ),
            "item_id": bare_id,
        }

    td_params = {
        "directive_id": bare_id,
        "inputs": inputs,
        "async": async_exec,
    }
    if model:
        td_params["model"] = model
    if limit_overrides:
        td_params["limit_overrides"] = limit_overrides
    if previous_thread_id:
        td_params["previous_thread_id"] = previous_thread_id

    import os
    parent_tid = os.environ.get("RYE_PARENT_THREAD_ID")
    if parent_tid:
        td_params["parent_thread_id"] = parent_tid

    result = asyncio.get_event_loop().run_until_complete(
        executor.execute(
            item_id=td_tool,
            parameters=td_params,
            validate_chain=True,
        )
    )

    if result.success:
        return {
            "status": "success",
            "type": "directive",
            "item_id": bare_id,
            "data": result.data,
            "metadata": {
                "duration_ms": result.duration_ms,
                **result.metadata,
            },
        }
    return {
        "status": "error",
        "error": result.error,
        "item_id": bare_id,
    }


def _find_directive(project_path, bare_id):
    """Find a directive file across project > user > system spaces."""
    from pathlib import Path
    from rye.constants import AI_DIR, ItemType
    from rye.utils.path_utils import (
        get_project_kind_path,
        get_system_spaces,
        get_user_kind_path,
    )
    from rye.utils.extensions import get_item_extensions

    search_bases = [
        get_project_kind_path(project_path, ItemType.DIRECTIVE),
        get_user_kind_path(ItemType.DIRECTIVE),
    ]
    type_folder = ItemType.KIND_DIRS[ItemType.DIRECTIVE]
    for bundle in get_system_spaces():
        search_bases.append(bundle.root_path / AI_DIR / type_folder)

    extensions = get_item_extensions(ItemType.DIRECTIVE, project_path)

    for base in search_bases:
        if not base.exists():
            continue
        for ext in extensions:
            file_path = base / f"{bare_id}{ext}"
            if file_path.is_file():
                return file_path
    return None
