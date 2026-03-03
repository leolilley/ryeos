"""Tests for Registry API endpoints."""

import pytest
from unittest.mock import AsyncMock, MagicMock, patch

pytest.importorskip("fastapi")
pytest.importorskip("supabase")

from fastapi.testclient import TestClient

from registry_api.config import Settings, get_settings
from registry_api.main import app


def _mock_settings():
    return MagicMock(
        spec=Settings,
        supabase_url="https://test.supabase.co",
        supabase_service_key="test-service-key",
        supabase_jwt_secret="test-jwt-secret",
        host="0.0.0.0",
        port=8000,
        log_level="INFO",
        allowed_origins=["*"],
    )


@pytest.fixture(autouse=True)
def _override_settings():
    """Override settings via FastAPI dependency overrides."""
    app.dependency_overrides[get_settings] = _mock_settings
    yield
    app.dependency_overrides.pop(get_settings, None)


@pytest.fixture
def mock_supabase():
    """Mock Supabase client."""
    with patch("registry_api.main.get_supabase") as mock:
        client = MagicMock()
        mock.return_value = client
        yield client


@pytest.fixture
def mock_user():
    """Mock authenticated user."""
    from registry_api.auth import User
    return User(id="test-user-id", email="test@example.com", username="testuser")


@pytest.fixture
def client(mock_supabase):
    """Test client with mocked dependencies."""
    return TestClient(app)


@pytest.fixture
def authed_client(mock_supabase, mock_user):
    """Test client with authentication bypassed."""
    from registry_api.auth import get_current_user
    app.dependency_overrides[get_current_user] = lambda: mock_user
    yield TestClient(app)
    app.dependency_overrides.pop(get_current_user, None)


class TestHealthCheck:
    """Tests for health check endpoint."""

    def test_health_check_success(self, client, mock_supabase):
        """Health check returns healthy status."""
        # Mock successful DB query
        mock_supabase.table.return_value.select.return_value.limit.return_value.execute.return_value = MagicMock()
        
        response = client.get("/health")
        
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "healthy"
        assert "version" in data
        assert data["database"] == "connected"

    def test_health_check_db_error(self, client, mock_supabase):
        """Health check handles DB errors gracefully."""
        # Mock DB error
        mock_supabase.table.side_effect = Exception("DB connection failed")
        
        response = client.get("/health")
        
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "healthy"
        assert data["database"] == "error"


class TestPushEndpoint:
    """Tests for push endpoint."""

    def test_push_requires_auth(self, client):
        """Push endpoint requires authentication."""
        response = client.post("/v1/push", json={
            "item_type": "directive",
            "item_id": "test",
            "content": "<directive/>",
            "version": "1.0.0",
        })
        
        # HTTPBearer returns 401 when no Authorization header is present
        assert response.status_code == 401

    def test_push_validates_item_type(self, authed_client):
        """Push validates item_type field."""
        response = authed_client.post(
            "/v1/push",
            json={
                "item_type": "invalid",
                "item_id": "test",
                "content": "test",
                "version": "1.0.0",
            },
        )
        
        # Should return 422 (validation error) for invalid enum
        assert response.status_code == 422

    def test_push_validates_version_format(self, authed_client):
        """Push validates semver version format."""
        response = authed_client.post(
            "/v1/push",
            json={
                "item_type": "directive",
                "item_id": "test",
                "content": "test",
                "version": "v1.0",  # Invalid semver
            },
        )
        
        assert response.status_code == 422


class TestPullEndpoint:
    """Tests for pull endpoint."""

    def test_pull_invalid_item_type(self, client):
        """Pull rejects invalid item type."""
        response = client.get("/v1/pull/invalid/test-item")
        
        assert response.status_code == 400
        assert "Invalid item_type" in response.json()["detail"]["error"]

    def test_pull_not_found(self, client, mock_supabase):
        """Pull returns 404 for non-existent item."""
        # Mock empty result — chain is .select().eq().eq().eq().execute()
        eq_chain = mock_supabase.table.return_value.select.return_value.eq.return_value
        eq_chain.eq.return_value.eq.return_value.execute.return_value = MagicMock(data=[])
        
        # item_id must be namespace/category/name format
        response = client.get("/v1/pull/directive/testns/core/nonexistent")
        
        assert response.status_code == 404


class TestSearchEndpoint:
    """Tests for search endpoint."""

    def test_search_basic(self, client, mock_supabase):
        """Search returns results."""
        mock_result = MagicMock(
            data=[{
                "id": "uuid-1",
                "namespace": "testuser",
                "category": "core",
                "name": "test-directive",
                "description": "A test",
                "visibility": "public",
                "download_count": 10,
                "latest_version": "1.0.0",
                "created_at": "2026-02-04T10:00:00Z",
            }],
            count=1,
        )
        # The search iterates 3 item types, each calling the same chain.
        # MagicMock auto-chains so all paths resolve to this result.
        tbl = mock_supabase.table.return_value
        tbl.select.return_value.or_.return_value.eq.return_value.range.return_value.execute.return_value = mock_result
        
        response = client.get("/v1/search?query=test&item_type=directive")
        
        assert response.status_code == 200
        data = response.json()
        assert "results" in data
        assert "total" in data
