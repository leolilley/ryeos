"""MCP transport for ryeos-remote.

Exposes 3 MCP tools (fetch, execute, sign) that call the engine
directly — no HTTP proxy, no separate service.

Mounted at /mcp in the FastAPI app via modal_app.py.
"""

import logging
from typing import Optional

from mcp.server.fastmcp import Context, FastMCP

from ryeos_remote.auth import User, _resolve_api_key, require_scope
from ryeos_remote.config import Settings, get_settings
from ryeos_remote.server import _execute_from_head

logger = logging.getLogger(__name__)

mcp = FastMCP("ryeos-remote", stateless_http=True, json_response=True)
mcp.settings.streamable_http_path = "/"


def _extract_token_from_ctx(ctx: Context) -> str:
    """Extract bearer token from MCP tool context."""
    try:
        request = ctx.request_context.request
        if request is not None:
            auth = request.headers.get("authorization", "")
            if auth.lower().startswith("bearer "):
                return auth[7:]
    except Exception:
        pass
    return ""


async def _authenticate(ctx: Context) -> tuple[User, Settings]:
    """Authenticate MCP request via bearer token. Returns (user, settings)."""
    token = _extract_token_from_ctx(ctx)
    if not token:
        raise ValueError("No authorization token provided")
    settings = get_settings()
    user = await _resolve_api_key(token, settings)
    return user, settings


@mcp.tool()
async def execute(
    item_type: str,
    item_id: str,
    ctx: Context,
    thread: str = "",
    parameters: dict | None = None,
    dry_run: bool = False,
    project_path: str | None = None,
) -> dict:
    """Execute a rye item on ryeos-remote."""
    user, settings = await _authenticate(ctx)
    require_scope(user, "remote:execute")

    if not project_path:
        raise ValueError("project_path is required")
    if item_type not in ("tool", "directive"):
        raise ValueError(f"item_type must be 'tool' or 'directive', got {item_type!r}")

    if not thread:
        thread = "fork" if item_type == "directive" else "inline"

    if item_type == "directive" and thread != "fork":
        raise ValueError(f"Directives must use thread=fork on remote, got thread={thread!r}")
    if item_type == "tool" and thread != "inline":
        raise ValueError(f"Tools must use thread=inline on remote, got thread={thread!r}")

    return await _execute_from_head(
        user=user,
        settings=settings,
        project_path=project_path,
        item_type=item_type,
        item_id=item_id,
        parameters=parameters or {},
        thread=thread,
    )


@mcp.tool()
async def fetch(
    ctx: Context,
    project_path: str | None = None,
    item_id: str | None = None,
    item_type: str | None = None,
    query: str | None = None,
    scope: str | None = None,
    source: str | None = None,
    destination: str | None = None,
    limit: int | None = None,
) -> dict:
    """Fetch rye items on ryeos-remote. ID mode (item_id) or query mode (query+scope)."""
    user, settings = await _authenticate(ctx)
    require_scope(user, "remote:execute")

    if not project_path:
        raise ValueError("project_path is required")

    params: dict = {}
    if item_id is not None:
        params["item_id"] = item_id
    if item_type is not None:
        params["item_type"] = item_type
    if query is not None:
        params["query"] = query
    if scope is not None:
        params["scope"] = scope
    if source is not None:
        params["source"] = source
    if destination is not None:
        params["destination"] = destination
    if limit is not None:
        params["limit"] = limit

    return await _execute_from_head(
        user=user,
        settings=settings,
        project_path=project_path,
        item_type="tool",
        item_id="rye/fetch",
        parameters=params,
        thread="inline",
    )


@mcp.tool()
async def sign(
    item_type: str,
    item_id: str,
    ctx: Context,
    project_path: str | None = None,
    source: str = "project",
) -> dict:
    """Sign a rye item on ryeos-remote."""
    user, settings = await _authenticate(ctx)
    require_scope(user, "remote:execute")

    if not project_path:
        raise ValueError("project_path is required")

    return await _execute_from_head(
        user=user,
        settings=settings,
        project_path=project_path,
        item_type="tool",
        item_id="rye/sign",
        parameters={"item_type": item_type, "item_id": item_id, "source": source},
        thread="inline",
    )


def get_mcp_app():
    """Return the ASGI app for mounting at /mcp."""
    return mcp.streamable_http_app()
