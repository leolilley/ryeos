"""Tests for remote.py and route.py httpx transport (post-HTTP-primitive removal)."""

import importlib.util
import json
import sys

import httpx
import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from conftest import get_bundle_path

# ── Load RemoteHttpClient from remote.py ─────────────────────────────────

_REMOTE_PATH = get_bundle_path("core", "tools/rye/core/remote/remote.py")
_spec_remote = importlib.util.spec_from_file_location("remote_mod", _REMOTE_PATH)
_remote_mod = importlib.util.module_from_spec(_spec_remote)
_spec_remote.loader.exec_module(_remote_mod)

RemoteHttpClient = _remote_mod.RemoteHttpClient

# ── Load _SimpleClient from route.py ─────────────────────────────────────

_ROUTE_PATH = get_bundle_path("core", "tools/rye/core/remote/route/route.py")

# route.py does `from remote_config import ...` — ensure the package dir is on path
_REMOTE_TOOL_DIR = str(get_bundle_path("core", "tools/rye/core/remote"))
if _REMOTE_TOOL_DIR not in sys.path:
    sys.path.insert(0, _REMOTE_TOOL_DIR)

_spec_route = importlib.util.spec_from_file_location("route_mod", _ROUTE_PATH)
_route_mod = importlib.util.module_from_spec(_spec_route)
_spec_route.loader.exec_module(_route_mod)

_SimpleClient = _route_mod._SimpleClient


# ── Helpers ──────────────────────────────────────────────────────────────


def _mock_response(status_code=200, body=None):
    """Build a mock httpx.Response."""
    resp = MagicMock(spec=httpx.Response)
    resp.status_code = status_code
    resp.content = json.dumps(body).encode() if body is not None else b""
    resp.json.return_value = body if body is not None else {}
    return resp


def _mock_non_json_response(status_code=502, text="<html>Bad Gateway</html>"):
    """Build a mock httpx.Response that returns non-JSON content."""
    resp = MagicMock(spec=httpx.Response)
    resp.status_code = status_code
    resp.content = text.encode()
    resp.text = text
    resp.json.side_effect = json.JSONDecodeError("Expecting value", text, 0)
    return resp


# ── RemoteHttpClient tests ───────────────────────────────────────────────


class TestRemoteHttpClient:
    async def test_remote_client_get_success(self):
        client = RemoteHttpClient("https://node.example.com", node_id="fp:abc")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.get.return_value = _mock_response(200, {"status": "ok"})
        client._http = mock_http

        with patch.object(client, "_sign_headers", return_value={}):
            result = await client.get("/api/v1/status")

        assert result["success"] is True
        assert result["status_code"] == 200
        assert result["body"] == {"status": "ok"}
        assert result["error"] is None

    async def test_remote_client_get_failure(self):
        client = RemoteHttpClient("https://node.example.com")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.get.return_value = _mock_response(500, {"error": "internal"})
        client._http = mock_http

        with patch.object(client, "_sign_headers", return_value={}):
            result = await client.get("/api/v1/status")

        assert result["success"] is False
        assert result["status_code"] == 500

    async def test_remote_client_get_network_error(self):
        client = RemoteHttpClient("https://node.example.com")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.get.side_effect = httpx.ConnectError("connection refused")
        client._http = mock_http

        with patch.object(client, "_sign_headers", return_value={}):
            result = await client.get("/api/v1/status")

        assert result["success"] is False
        assert result["status_code"] == 0
        assert result["error"] is not None
        assert "connection refused" in result["error"]

    async def test_remote_client_post_success(self):
        client = RemoteHttpClient("https://node.example.com", node_id="fp:abc")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.post.return_value = _mock_response(200, {"id": "exec-123"})
        client._http = mock_http

        with patch.object(client, "_sign_headers", return_value={}):
            result = await client.post("/api/v1/execute", {"item_id": "test"})

        assert result["success"] is True
        assert result["status_code"] == 200
        assert result["body"] == {"id": "exec-123"}
        assert result["error"] is None

    async def test_get_non_json_response(self):
        client = RemoteHttpClient("https://node.example.com")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.get.return_value = _mock_non_json_response(502, "<html>Bad Gateway</html>")
        client._http = mock_http

        with patch.object(client, "_sign_headers", return_value={}):
            result = await client.get("/api/v1/status")

        assert result["success"] is False
        assert result["status_code"] == 502
        assert "non-JSON" in result["error"]
        assert "502" in result["error"]
        assert result["body"] == "<html>Bad Gateway</html>"

    async def test_post_non_json_response(self):
        client = RemoteHttpClient("https://node.example.com")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.post.return_value = _mock_non_json_response(503, "Service Unavailable")
        client._http = mock_http

        with patch.object(client, "_sign_headers", return_value={}):
            result = await client.post("/objects/put", {"entries": []})

        assert result["success"] is False
        assert result["status_code"] == 503
        assert "non-JSON" in result["error"]
        assert result["body"] == "Service Unavailable"

    async def test_delete_non_json_response(self):
        client = RemoteHttpClient("https://node.example.com")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.delete.return_value = _mock_non_json_response(500, "Internal Server Error")
        client._http = mock_http

        with patch.object(client, "_sign_headers", return_value={}):
            result = await client.delete("/api/v1/resource")

        assert result["success"] is False
        assert result["status_code"] == 500
        assert "non-JSON" in result["error"]


# ── _SimpleClient tests ──────────────────────────────────────────────────


class TestSimpleClient:
    async def test_simple_client_get_success(self):
        client = _SimpleClient("https://node.example.com")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.get.return_value = _mock_response(200, {"healthy": True})
        client._http = mock_http

        result = await client.get("/status")

        assert result["success"] is True
        assert result["status_code"] == 200
        assert result["body"] == {"healthy": True}
        assert result["error"] is None

    async def test_simple_client_get_network_error(self):
        client = _SimpleClient("https://node.example.com")
        mock_http = AsyncMock(spec=httpx.AsyncClient)
        mock_http.get.side_effect = httpx.ConnectError("connection refused")
        client._http = mock_http

        result = await client.get("/status")

        assert result["success"] is False
        assert result["status_code"] == 0
        assert result["error"] is not None
        assert "connection refused" in result["error"]
