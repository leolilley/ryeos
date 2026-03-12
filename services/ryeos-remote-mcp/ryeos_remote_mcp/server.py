"""Stateless MCP-over-HTTP proxy for ryeos-remote.

Exposes 4 MCP tools (execute, search, load, sign) plus REST and CAS sync
endpoints.  Everything proxies to Modal's ryeos-remote /execute endpoint.
This service has no rye engine, no CAS store, no volume — just httpx + mcp SDK
+ starlette.
"""

import contextlib
import logging
import os

import httpx
from mcp.server.fastmcp import Context, FastMCP
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Mount, Route

logger = logging.getLogger(__name__)

MODAL_URL = os.environ.get("RYEOS_REMOTE_MODAL_URL", "")

# ---------------------------------------------------------------------------
# MCP server
# ---------------------------------------------------------------------------

mcp = FastMCP("ryeos-remote", stateless_http=True, json_response=True)
mcp.settings.streamable_http_path = "/"


# ---------------------------------------------------------------------------
# Shared proxy core
# ---------------------------------------------------------------------------


async def _proxy_to_modal(path: str, body: dict, token: str | None = None) -> dict:
    headers = {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    async with httpx.AsyncClient() as client:
        resp = await client.post(
            f"{MODAL_URL}{path}",
            json=body,
            headers=headers,
            timeout=300,
        )
        resp.raise_for_status()
        return resp.json()


def _extract_token(request: Request) -> str:
    """Extract bearer token from a Starlette request, or return empty string."""
    auth = request.headers.get("authorization", "")
    if auth.lower().startswith("bearer "):
        return auth[7:]
    return ""


def _extract_token_from_ctx(ctx: Context) -> str:
    """Extract bearer token from MCP tool context.

    The MCP SDK's streamable HTTP transport stores the Starlette Request
    object on ``RequestContext.request``.
    """
    try:
        request = ctx.request_context.request
        if request is not None:
            auth = request.headers.get("authorization", "")
            if auth.lower().startswith("bearer "):
                return auth[7:]
    except Exception:
        pass
    return ""


# ---------------------------------------------------------------------------
# MCP tools
# ---------------------------------------------------------------------------


@mcp.tool()
async def execute(
    item_type: str,
    item_id: str,
    ctx: Context,
    thread: str,
    parameters: dict | None = None,
    dry_run: bool = False,
    project_name: str | None = None,
) -> dict:
    """Execute a rye item on ryeos-remote."""
    token = _extract_token_from_ctx(ctx)
    body: dict = {
        "item_type": item_type,
        "item_id": item_id,
        "thread": thread,
        "dry_run": dry_run,
    }
    if parameters is not None:
        body["parameters"] = parameters
    if project_name is not None:
        body["project_name"] = project_name
    return await _proxy_to_modal("/execute", body, token or None)


@mcp.tool()
async def search(
    scope: str,
    query: str,
    ctx: Context,
    source: str = "all",
    limit: int = 10,
) -> dict:
    """Search for rye items on ryeos-remote."""
    token = _extract_token_from_ctx(ctx)
    body: dict = {
        "item_type": "tool",
        "item_id": "rye/search",
        "parameters": {
            "scope": scope,
            "query": query,
            "source": source,
            "limit": limit,
        },
    }
    return await _proxy_to_modal("/execute", body, token or None)


@mcp.tool()
async def load(
    item_type: str,
    item_id: str,
    ctx: Context,
    source: str | None = None,
    destination: str | None = None,
) -> dict:
    """Load/inspect a rye item on ryeos-remote."""
    token = _extract_token_from_ctx(ctx)
    params: dict = {
        "item_type": item_type,
        "item_id": item_id,
    }
    if source is not None:
        params["source"] = source
    if destination is not None:
        params["destination"] = destination
    body: dict = {
        "item_type": "tool",
        "item_id": "rye/load",
        "parameters": params,
    }
    return await _proxy_to_modal("/execute", body, token or None)


@mcp.tool()
async def sign(
    item_type: str,
    item_id: str,
    ctx: Context,
    source: str = "project",
) -> dict:
    """Sign a rye item on ryeos-remote."""
    token = _extract_token_from_ctx(ctx)
    body: dict = {
        "item_type": "tool",
        "item_id": "rye/sign",
        "parameters": {
            "item_type": item_type,
            "item_id": item_id,
            "source": source,
        },
    }
    return await _proxy_to_modal("/execute", body, token or None)


# ---------------------------------------------------------------------------
# REST endpoints
# ---------------------------------------------------------------------------


async def rest_execute(request: Request) -> JSONResponse:
    body = await request.json()
    token = _extract_token(request)
    result = await _proxy_to_modal("/execute", body, token or None)
    return JSONResponse(result)


async def rest_search(request: Request) -> JSONResponse:
    body = await request.json()
    token = _extract_token(request)
    payload: dict = {
        "item_type": "tool",
        "item_id": "rye/search",
        "parameters": body,
    }
    result = await _proxy_to_modal("/execute", payload, token or None)
    return JSONResponse(result)


async def rest_load(request: Request) -> JSONResponse:
    body = await request.json()
    token = _extract_token(request)
    payload: dict = {
        "item_type": "tool",
        "item_id": "rye/load",
        "parameters": body,
    }
    result = await _proxy_to_modal("/execute", payload, token or None)
    return JSONResponse(result)


async def rest_sign(request: Request) -> JSONResponse:
    body = await request.json()
    token = _extract_token(request)
    payload: dict = {
        "item_type": "tool",
        "item_id": "rye/sign",
        "parameters": body,
    }
    result = await _proxy_to_modal("/execute", payload, token or None)
    return JSONResponse(result)



# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------


async def health(request: Request) -> JSONResponse:
    return JSONResponse({"status": "ok"})


# ---------------------------------------------------------------------------
# Application assembly
# ---------------------------------------------------------------------------


@contextlib.asynccontextmanager
async def lifespan(application: Starlette):
    async with contextlib.AsyncExitStack() as stack:
        await stack.enter_async_context(mcp.session_manager.run())
        yield


app = Starlette(
    routes=[
        Mount("/mcp", app=mcp.streamable_http_app()),
        Route("/execute", rest_execute, methods=["POST"]),
        Route("/search", rest_search, methods=["POST"]),
        Route("/load", rest_load, methods=["POST"]),
        Route("/sign", rest_sign, methods=["POST"]),
        Route("/health", health, methods=["GET"]),
    ],
    lifespan=lifespan,
)
