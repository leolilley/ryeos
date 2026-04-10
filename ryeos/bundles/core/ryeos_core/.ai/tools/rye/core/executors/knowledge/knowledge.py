# rye:signed:2026-04-10T08:31:58Z:2594597f222c1a3a6f4ee2da9b92fe79f02c1ba817f2b345ab6003a7431febe1:3kn_V06R69AaX9Z239TBeL_7i7ia3TNe-uhIPwsXBVNAoymGhgXCc29gU7hNGXrGCy0fJsppzMiUYh6mIZ5VDw:4b987fd4e40303ac
"""Knowledge executor — returns knowledge content without metadata.

Receives a generic envelope from the engine:
    {item_id, parameters, thread, async, dry_run}

Parses the knowledge item and returns just the body content,
stripping frontmatter metadata.  Fetch returns the whole doc;
execute returns just the content.
"""

__version__ = "1.0.0"
__tool_type__ = "executor"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/executors/knowledge"
__tool_description__ = "Return knowledge content without metadata"
__allowed_threads__ = ["inline"]
__allowed_targets__ = ["local"]


def execute(params: dict, project_path: str) -> dict:
    """Execute a knowledge item — return its content."""
    from pathlib import Path

    from rye.constants import AI_DIR, ItemType
    from rye.utils.parser_router import ParserRouter
    from rye.utils.path_utils import (
        get_project_kind_path,
        get_system_spaces,
        get_user_kind_path,
    )
    from rye.utils.extensions import get_item_extensions
    from rye.utils.integrity import verify_item, IntegrityError
    from rye.utils.execution_context import ExecutionContext

    item_id = params.get("item_id", "")
    if not item_id:
        return {"status": "error", "error": "item_id is required"}

    _, bare_id = ItemType.parse_canonical_ref(item_id)

    proj = Path(project_path)
    file_path = _find_knowledge(proj, bare_id)
    if not file_path:
        return {"status": "error", "error": f"Knowledge not found: {bare_id}"}

    ctx = ExecutionContext.from_env(project_path=proj)
    try:
        verify_item(file_path, ItemType.KNOWLEDGE, ctx=ctx)
    except IntegrityError as exc:
        return {"status": "error", "error": str(exc), "item_id": bare_id}

    parser_router = ParserRouter()
    content = file_path.read_text(encoding="utf-8")
    parsed = parser_router.parse("markdown/frontmatter", content)

    if "error" in parsed:
        return {"status": "error", "error": parsed["error"], "item_id": bare_id}

    body = parsed.get("body", "") or content

    return {"content": body, "item_id": bare_id, "metadata": {}}


def _find_knowledge(project_path, bare_id):
    """Find a knowledge file across project > user > system spaces."""
    from rye.constants import AI_DIR, ItemType
    from rye.utils.path_utils import (
        get_project_kind_path,
        get_system_spaces,
        get_user_kind_path,
    )
    from rye.utils.extensions import get_item_extensions

    search_bases = [
        get_project_kind_path(project_path, ItemType.KNOWLEDGE),
        get_user_kind_path(ItemType.KNOWLEDGE),
    ]
    type_folder = ItemType.KIND_DIRS[ItemType.KNOWLEDGE]
    for bundle in get_system_spaces():
        search_bases.append(bundle.root_path / AI_DIR / type_folder)

    extensions = get_item_extensions(ItemType.KNOWLEDGE, project_path)

    for base in search_bases:
        if not base.exists():
            continue
        for ext in extensions:
            file_path = base / f"{bare_id}{ext}"
            if file_path.is_file():
                return file_path
    return None
