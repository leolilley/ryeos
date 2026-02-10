"""Tests for Registry API endpoints."""

import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from fastapi.testclient import TestClient


# Mock settings before importing app
@pytest.fixture(autouse=True)
def mock_settings():
    """Mock settings for all tests."""
    with patch("registry_api.config.get_settings") as mock:
        mock.return_value = MagicMock(
            supabase_url="https://test.supabase.co",
            supabase_service_key="test-service-key",
            supabase_jwt_secret="test-jwt-secret",
            host="0.0.0.0",
            port=8000,
            log_level="INFO",
            allowed_origins=["*"],
        )
        yield mock


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
    from registry_api.main import app
    return TestClient(app)


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
        
        # Should return 403 (no auth header)
        assert response.status_code == 403

    def test_push_validates_item_type(self, client):
        """Push validates item_type field."""
        response = client.post(
            "/v1/push",
            json={
                "item_type": "invalid",
                "item_id": "test",
                "content": "test",
                "version": "1.0.0",
            },
            headers={"Authorization": "Bearer fake-token"},
        )
        
        # Should return 422 (validation error) for invalid enum
        assert response.status_code == 422

    def test_push_validates_version_format(self, client):
        """Push validates semver version format."""
        response = client.post(
            "/v1/push",
            json={
                "item_type": "directive",
                "item_id": "test",
                "content": "test",
                "version": "v1.0",  # Invalid semver
            },
            headers={"Authorization": "Bearer fake-token"},
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
        # Mock empty result
        mock_supabase.table.return_value.select.return_value.eq.return_value.execute.return_value = MagicMock(
            data=[]
        )
        
        response = client.get("/v1/pull/directive/nonexistent")
        
        assert response.status_code == 404


class TestSearchEndpoint:
    """Tests for search endpoint."""

    def test_search_basic(self, client, mock_supabase):
        """Search returns results."""
        # Mock search result
        mock_supabase.table.return_value.select.return_value.or_.return_value.range.return_value.execute.return_value = MagicMock(
            data=[{
                "name": "test-directive",
                "description": "A test",
                "category": "core",
                "download_count": 10,
                "created_at": "2026-02-04T10:00:00Z",
                "users": {"username": "testuser"},
            }],
            count=1,
        )
        
        response = client.get("/v1/search?query=test")
        
        assert response.status_code == 200
        data = response.json()
        assert "results" in data
        assert "total" in data
