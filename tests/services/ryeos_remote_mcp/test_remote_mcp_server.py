"""Tests for ryeos-remote-mcp proxy server.

Uses httpx.AsyncClient with ASGITransport to test the Starlette app directly.
Mocks _proxy_to_modal to avoid real HTTP calls to Modal.
"""

import httpx
import pytest
from unittest.mock import AsyncMock, patch
from httpx import ASGITransport, AsyncClient
from starlette.requests import Request

from ryeos_remote_mcp.server import app, _extract_token, _proxy_to_modal


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

MOCK_MODAL_RESPONSE = {"status": "ok", "result": {"data": "test"}}


@pytest.fixture
def client():
    transport = ASGITransport(app=app)
    return AsyncClient(transport=transport, base_url="http://testserver")


# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------


class TestHealth:
    async def test_returns_200(self, client):
        resp = await client.get("/health")
        assert resp.status_code == 200
        assert resp.json() == {"status": "ok"}


# ---------------------------------------------------------------------------
# Token extraction
# ---------------------------------------------------------------------------


class TestTokenExtraction:
    def test_extracts_bearer_token(self):
        scope = {
            "type": "http",
            "method": "POST",
            "path": "/",
            "headers": [(b"authorization", b"Bearer my-secret-token")],
        }
        request = Request(scope)
        assert _extract_token(request) == "my-secret-token"

    def test_missing_authorization_returns_empty(self):
        scope = {
            "type": "http",
            "method": "POST",
            "path": "/",
            "headers": [],
        }
        request = Request(scope)
        assert _extract_token(request) == ""

    def test_non_bearer_returns_empty(self):
        scope = {
            "type": "http",
            "method": "POST",
            "path": "/",
            "headers": [(b"authorization", b"Basic dXNlcjpwYXNz")],
        }
        request = Request(scope)
        assert _extract_token(request) == ""

    def test_case_insensitive_bearer(self):
        scope = {
            "type": "http",
            "method": "POST",
            "path": "/",
            "headers": [(b"authorization", b"BEARER my-token")],
        }
        request = Request(scope)
        assert _extract_token(request) == "my-token"


# ---------------------------------------------------------------------------
# REST execute
# ---------------------------------------------------------------------------


class TestRestExecute:
    async def test_proxies_to_modal(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value=MOCK_MODAL_RESPONSE,
        ) as mock:
            resp = await client.post(
                "/execute",
                json={"item_type": "tool", "item_id": "test/tool", "thread": "inline"},
                headers={"Authorization": "Bearer test-token"},
            )
            assert resp.status_code == 200
            assert resp.json() == MOCK_MODAL_RESPONSE
            mock.assert_called_once_with(
                "/execute",
                {"item_type": "tool", "item_id": "test/tool", "thread": "inline"},
                "test-token",
            )

    async def test_missing_token_passes_none(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value=MOCK_MODAL_RESPONSE,
        ) as mock:
            resp = await client.post(
                "/execute",
                json={"item_type": "tool", "item_id": "test/tool", "thread": "inline"},
            )
            assert resp.status_code == 200
            mock.assert_called_once_with(
                "/execute",
                {"item_type": "tool", "item_id": "test/tool", "thread": "inline"},
                None,
            )

    async def test_passthrough_preserves_thread(self, client):
        """REST is pure passthrough — thread value forwarded as-is to Modal."""
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value=MOCK_MODAL_RESPONSE,
        ) as mock:
            resp = await client.post(
                "/execute",
                json={"item_type": "directive", "item_id": "email/send", "thread": "fork"},
                headers={"Authorization": "Bearer tok"},
            )
            assert resp.status_code == 200
            body_sent = mock.call_args[0][1]
            assert body_sent["thread"] == "fork"


# ---------------------------------------------------------------------------
# REST search
# ---------------------------------------------------------------------------


class TestRestSearch:
    async def test_wraps_params_correctly(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value=MOCK_MODAL_RESPONSE,
        ) as mock:
            resp = await client.post(
                "/search",
                json={"scope": "tool", "query": "test"},
                headers={"Authorization": "Bearer tok"},
            )
            assert resp.status_code == 200
            mock.assert_called_once_with(
                "/execute",
                {
                    "item_type": "tool",
                    "item_id": "rye/search",
                    "parameters": {"scope": "tool", "query": "test"},
                },
                "tok",
            )


# ---------------------------------------------------------------------------
# REST load
# ---------------------------------------------------------------------------


class TestRestLoad:
    async def test_wraps_params_correctly(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value=MOCK_MODAL_RESPONSE,
        ) as mock:
            resp = await client.post(
                "/load",
                json={"item_type": "directive", "item_id": "init"},
                headers={"Authorization": "Bearer tok"},
            )
            assert resp.status_code == 200
            mock.assert_called_once_with(
                "/execute",
                {
                    "item_type": "tool",
                    "item_id": "rye/load",
                    "parameters": {"item_type": "directive", "item_id": "init"},
                },
                "tok",
            )


# ---------------------------------------------------------------------------
# REST sign
# ---------------------------------------------------------------------------


class TestRestSign:
    async def test_wraps_params_correctly(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value=MOCK_MODAL_RESPONSE,
        ) as mock:
            resp = await client.post(
                "/sign",
                json={"item_type": "tool", "item_id": "my/tool"},
                headers={"Authorization": "Bearer tok"},
            )
            assert resp.status_code == 200
            mock.assert_called_once_with(
                "/execute",
                {
                    "item_type": "tool",
                    "item_id": "rye/sign",
                    "parameters": {"item_type": "tool", "item_id": "my/tool"},
                },
                "tok",
            )


# ---------------------------------------------------------------------------
# CAS proxy
# ---------------------------------------------------------------------------


class TestCasProxy:
    async def test_has_forwards_correctly(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value={"present": ["abc"], "missing": []},
        ) as mock:
            resp = await client.post(
                "/objects/has",
                json={"hashes": ["abc"]},
                headers={"Authorization": "Bearer tok"},
            )
            assert resp.status_code == 200
            mock.assert_called_once_with(
                "/objects/has",
                {"hashes": ["abc"]},
                "tok",
            )

    async def test_put_forwards_correctly(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value={"stored": ["abc"]},
        ) as mock:
            resp = await client.post(
                "/objects/put",
                json={"entries": [{"hash": "abc", "kind": "blob", "data": "eA=="}]},
                headers={"Authorization": "Bearer tok"},
            )
            assert resp.status_code == 200
            mock.assert_called_once_with(
                "/objects/put",
                {"entries": [{"hash": "abc", "kind": "blob", "data": "eA=="}]},
                "tok",
            )

    async def test_get_forwards_correctly(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value={"entries": []},
        ) as mock:
            resp = await client.post(
                "/objects/get",
                json={"hashes": ["abc"]},
                headers={"Authorization": "Bearer tok"},
            )
            assert resp.status_code == 200
            mock.assert_called_once_with(
                "/objects/get",
                {"hashes": ["abc"]},
                "tok",
            )


# ---------------------------------------------------------------------------
# Push proxy
# ---------------------------------------------------------------------------


class TestPushProxy:
    async def test_forwards_correctly(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            return_value={"snapshot_hash": "abc123"},
        ) as mock:
            resp = await client.post(
                "/push",
                json={"project_name": "my-project", "entries": []},
                headers={"Authorization": "Bearer tok"},
            )
            assert resp.status_code == 200
            mock.assert_called_once_with(
                "/push",
                {"project_name": "my-project", "entries": []},
                "tok",
            )


# ---------------------------------------------------------------------------
# Error handling
# ---------------------------------------------------------------------------


class TestErrorHandling:
    async def test_modal_http_error_propagates(self, client):
        with patch(
            "ryeos_remote_mcp.server._proxy_to_modal",
            new_callable=AsyncMock,
            side_effect=httpx.HTTPStatusError(
                "Internal Server Error",
                request=httpx.Request("POST", "http://modal/execute"),
                response=httpx.Response(500),
            ),
        ):
            with pytest.raises(httpx.HTTPStatusError):
                await client.post(
                    "/execute",
                    json={"item_type": "tool", "item_id": "test/tool"},
                )


# ---------------------------------------------------------------------------
# _proxy_to_modal unit tests
# ---------------------------------------------------------------------------


class TestProxyToModal:
    async def test_sends_auth_header_when_token_present(self):
        mock_response = httpx.Response(
            200,
            json={"ok": True},
            request=httpx.Request("POST", "http://modal/execute"),
        )

        with patch("ryeos_remote_mcp.server.httpx.AsyncClient") as MockClient:
            mock_instance = AsyncMock()
            mock_instance.post.return_value = mock_response
            mock_instance.__aenter__ = AsyncMock(return_value=mock_instance)
            mock_instance.__aexit__ = AsyncMock(return_value=False)
            MockClient.return_value = mock_instance

            with patch("ryeos_remote_mcp.server.MODAL_URL", "http://modal"):
                result = await _proxy_to_modal("/execute", {"test": True}, "my-token")

            assert result == {"ok": True}
            mock_instance.post.assert_called_once_with(
                "http://modal/execute",
                json={"test": True},
                headers={"Authorization": "Bearer my-token"},
                timeout=300,
            )

    async def test_omits_auth_header_when_no_token(self):
        mock_response = httpx.Response(
            200,
            json={"ok": True},
            request=httpx.Request("POST", "http://modal/execute"),
        )

        with patch("ryeos_remote_mcp.server.httpx.AsyncClient") as MockClient:
            mock_instance = AsyncMock()
            mock_instance.post.return_value = mock_response
            mock_instance.__aenter__ = AsyncMock(return_value=mock_instance)
            mock_instance.__aexit__ = AsyncMock(return_value=False)
            MockClient.return_value = mock_instance

            with patch("ryeos_remote_mcp.server.MODAL_URL", "http://modal"):
                result = await _proxy_to_modal("/execute", {"test": True})

            mock_instance.post.assert_called_once_with(
                "http://modal/execute",
                json={"test": True},
                headers={},
                timeout=300,
            )
