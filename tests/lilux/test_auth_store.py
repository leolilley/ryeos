"""Tests for authentication store (Phase 4.2)."""

import pytest
from unittest.mock import AsyncMock, patch, MagicMock
import time
from lilux.runtime.auth import AuthStore, AuthenticationRequired, RefreshError


@pytest.fixture(autouse=True)
def mock_keyring_available():
    """Mock KEYRING_AVAILABLE to True for all tests."""
    with patch("lilux.runtime.auth.KEYRING_AVAILABLE", True):
        with patch("lilux.runtime.auth.keyring") as mock_keyring:
            # Set up the mock to behave like a real keyring
            mock_keyring.set_password = MagicMock()
            mock_keyring.get_password = MagicMock(return_value=None)
            mock_keyring.delete_password = MagicMock()
            yield


class TestAuthStoreInit:
    """Test AuthStore initialization."""

    def test_init_default_service_name(self):
        """AuthStore initializes with default service name."""
        auth = AuthStore()
        assert auth.service_name == "lilux"

    def test_init_custom_service_name(self):
        """AuthStore initializes with custom service name."""
        auth = AuthStore(service_name="myapp")
        assert auth.service_name == "myapp"


class TestAuthStoreSetToken:
    """Test token storage."""

    def test_set_token_stores_token(self):
        """set_token stores access token."""
        with patch("lilux.runtime.auth.keyring.set_password") as mock_set:
            auth = AuthStore()
            auth.set_token(service="github", access_token="token123")
            # set_password should be called
            mock_set.assert_called()

    def test_set_token_with_refresh_token(self):
        """set_token can store refresh token."""
        with patch("lilux.runtime.auth.keyring.set_password") as mock_set:
            auth = AuthStore()
            auth.set_token(
                service="github", access_token="access123", refresh_token="refresh456"
            )
            # set_password should be called (stores all in JSON blob)
            mock_set.assert_called()
            # Verify refresh_token is in the stored data
            call_args = mock_set.call_args
            import json

            stored_data = json.loads(call_args[0][2])
            assert stored_data["refresh_token"] == "refresh456"

    def test_set_token_with_expires_in(self):
        """set_token handles expires_in."""
        with patch("lilux.runtime.auth.keyring.set_password"):
            auth = AuthStore()
            auth.set_token(service="github", access_token="token123", expires_in=3600)
            # Metadata should be cached
            assert "github" in auth._metadata_cache

    def test_set_token_with_scopes(self):
        """set_token can store scopes."""
        with patch("lilux.runtime.auth.keyring.set_password"):
            auth = AuthStore()
            auth.set_token(
                service="github", access_token="token123", scopes=["read", "write"]
            )
            # Scopes should be cached
            assert auth._metadata_cache["github"]["scopes"] == ["read", "write"]


class TestAuthStoreIsAuthenticated:
    """Test authentication status."""

    def test_is_authenticated_true(self):
        """is_authenticated returns True when token exists."""
        import json
        token_json = json.dumps({"access_token": "token123", "expires_at": time.time() + 3600})
        with patch("lilux.runtime.auth.keyring.get_password", return_value=token_json):
            auth = AuthStore()
            assert auth.is_authenticated("github")

    def test_is_authenticated_false(self):
        """is_authenticated returns False when no token."""
        with patch("lilux.runtime.auth.keyring.get_password", return_value=None):
            auth = AuthStore()
            assert not auth.is_authenticated("github")

    def test_is_authenticated_keychain_error(self):
        """is_authenticated handles keychain errors."""
        with patch(
            "lilux.runtime.auth.keyring.get_password",
            side_effect=Exception("Keychain error"),
        ):
            auth = AuthStore()
            # Should return False on error
            assert not auth.is_authenticated("github")


class TestAuthStoreClearToken:
    """Test token removal."""

    def test_clear_token_removes_token(self):
        """clear_token removes token from keychain."""
        with patch("lilux.runtime.auth.keyring.delete_password") as mock_delete:
            auth = AuthStore()
            auth._metadata_cache["github"] = {"expires_at": 123}

            auth.clear_token("github")

            # delete_password should be called
            assert mock_delete.called
            # Cache should be cleared
            assert "github" not in auth._metadata_cache


@pytest.mark.asyncio
class TestAuthStoreGetToken:
    """Test token retrieval."""

    async def test_get_token_returns_valid_token(self):
        """get_token returns valid token."""
        # Token with future expiry
        future_expiry = time.time() + 3600
        token_json = f'{{"access_token": "token123", "expires_at": {future_expiry}}}'
        with patch("lilux.runtime.auth.keyring.get_password", return_value=token_json):
            auth = AuthStore()
            token = await auth.get_token("github")

            assert token == "token123"

    async def test_get_token_missing_raises_error(self):
        """get_token raises AuthenticationRequired if missing."""
        with patch("lilux.runtime.auth.keyring.get_password", return_value=None):
            auth = AuthStore()

            with pytest.raises(AuthenticationRequired):
                await auth.get_token("github")

    async def test_get_token_keychain_error_raises(self):
        """get_token raises AuthenticationRequired on keychain error."""
        with patch(
            "lilux.runtime.auth.keyring.get_password",
            side_effect=Exception("Keychain error"),
        ):
            auth = AuthStore()

            with pytest.raises(AuthenticationRequired):
                await auth.get_token("github")

    async def test_get_token_with_scope_validation(self):
        """get_token validates scopes."""
        future_expiry = time.time() + 3600
        token_json = f'{{"access_token": "token123", "expires_at": {future_expiry}, "scopes": ["read", "write"]}}'
        with patch("lilux.runtime.auth.keyring.get_password", return_value=token_json):
            auth = AuthStore()
            token = await auth.get_token("github", scope="read")

            assert token == "token123"

    async def test_get_token_missing_scope_raises(self):
        """get_token raises error for missing scope."""
        future_expiry = time.time() + 3600
        token_json = f'{{"access_token": "token123", "expires_at": {future_expiry}, "scopes": ["read"]}}'
        with patch("lilux.runtime.auth.keyring.get_password", return_value=token_json):
            auth = AuthStore()

            with pytest.raises(AuthenticationRequired):
                await auth.get_token("github", scope="write")

    async def test_get_token_expired_no_refresh_raises(self):
        """get_token raises error if expired with no refresh token."""
        # Token is expired
        past_expiry = time.time() - 1000
        token_json = f'{{"access_token": "old_token", "expires_at": {past_expiry}}}'
        with patch("lilux.runtime.auth.keyring.get_password", return_value=token_json):
            auth = AuthStore()

            with pytest.raises(AuthenticationRequired):
                await auth.get_token("github")


@pytest.mark.asyncio
class TestAuthStoreOAuth2:
    """Test OAuth2 refresh."""

    @patch("lilux.runtime.auth.httpx.AsyncClient")
    async def test_refresh_token_http_request(self, mock_client_class):
        """_refresh_token makes HTTP request."""
        mock_response = AsyncMock()
        mock_response.status_code = 200
        mock_response.json = MagicMock(
            return_value={
                "access_token": "new_token",
                "refresh_token": "new_refresh",
                "expires_in": 3600,
            }
        )

        mock_client_instance = AsyncMock()
        mock_client_instance.post = AsyncMock(return_value=mock_response)
        mock_client_instance.__aenter__ = AsyncMock(return_value=mock_client_instance)
        mock_client_instance.__aexit__ = AsyncMock(return_value=None)
        mock_client_class.return_value = mock_client_instance

        auth = AuthStore()
        new_tokens = await auth._refresh_token(
            refresh_token="old_refresh",
            refresh_url="https://oauth.example.com/token",
            client_id="client123",
            client_secret="secret456",
        )

        assert new_tokens["access_token"] == "new_token"

    @patch("lilux.runtime.auth.httpx.AsyncClient")
    async def test_refresh_token_failure_raises(self, mock_client_class):
        """_refresh_token raises RefreshError on failure."""
        mock_response = AsyncMock()
        mock_response.status_code = 401
        mock_response.text = "Invalid credentials"

        mock_client_instance = AsyncMock()
        mock_client_instance.post = AsyncMock(return_value=mock_response)
        mock_client_instance.__aenter__ = AsyncMock(return_value=mock_client_instance)
        mock_client_instance.__aexit__ = AsyncMock(return_value=None)
        mock_client_class.return_value = mock_client_instance

        auth = AuthStore()

        with pytest.raises(RefreshError):
            await auth._refresh_token(
                refresh_token="bad_token",
                refresh_url="https://oauth.example.com/token",
                client_id="client123",
                client_secret="secret456",
            )

    @patch("lilux.runtime.auth.httpx.AsyncClient")
    async def test_refresh_token_request_format(self, mock_client_class):
        """_refresh_token sends correct OAuth2 request."""
        mock_response = AsyncMock()
        mock_response.status_code = 200
        mock_response.json = MagicMock(
            return_value={"access_token": "new_token", "expires_in": 3600}
        )

        mock_client_instance = AsyncMock()
        mock_client_instance.post = AsyncMock(return_value=mock_response)
        mock_client_instance.__aenter__ = AsyncMock(return_value=mock_client_instance)
        mock_client_instance.__aexit__ = AsyncMock(return_value=None)
        mock_client_class.return_value = mock_client_instance

        auth = AuthStore()
        await auth._refresh_token(
            refresh_token="refresh123",
            refresh_url="https://oauth.example.com/token",
            client_id="client_id",
            client_secret="client_secret",
        )

        # Verify request was sent
        call_kwargs = mock_client_instance.post.call_args[1]
        assert "data" in call_kwargs
        data = call_kwargs["data"]
        assert data["grant_type"] == "refresh_token"
        assert data["refresh_token"] == "refresh123"


class TestAuthStoreMultiService:
    """Test multi-service support."""

    def test_multiple_services(self):
        """Can manage tokens for multiple services."""
        import json
        token_json = json.dumps({"access_token": "token", "expires_at": time.time() + 3600})
        with patch("lilux.runtime.auth.keyring.set_password") as mock_set:
            with patch("lilux.runtime.auth.keyring.get_password", return_value=token_json):
                auth = AuthStore()

                auth.set_token("github", access_token="github_token")
                auth.set_token("google", access_token="google_token")

                assert auth.is_authenticated("github")
                assert auth.is_authenticated("google")

    def test_clear_one_service(self):
        """Clearing one service doesn't affect others."""
        with patch("lilux.runtime.auth.keyring.delete_password"):
            auth = AuthStore()

            auth._metadata_cache["github"] = {"expires_at": 123}
            auth._metadata_cache["google"] = {"expires_at": 456}

            auth.clear_token("github")

            assert "github" not in auth._metadata_cache
            assert "google" in auth._metadata_cache
