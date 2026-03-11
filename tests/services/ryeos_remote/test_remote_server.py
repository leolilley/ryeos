"""Tests for ryeos-remote server endpoints.

Uses FastAPI TestClient with auth dependency overrides.
Covers: happy path, security, limits/quotas, trust/key verification,
sync protocol, materializer, ArtifactStore, health/version endpoints.
"""

import base64
import hashlib
import importlib.util
import json
import logging
import os

import pytest
from fastapi.testclient import TestClient

from lillux.primitives import cas
from lillux.primitives.integrity import canonical_json, compute_integrity
from lillux.primitives.signing import (
    ensure_keypair,
    generate_keypair,
    save_keypair,
    compute_key_fingerprint,
)
from rye.cas.manifest import build_manifest
from rye.cas.materializer import (
    ExecutionPaths,
    cleanup,
    get_system_version,
    materialize,
    _safe_target,
)
from rye.cas.objects import (
    ArtifactIndex,
    ExecutionSnapshot,
    ItemSource,
    RuntimeOutputsBundle,
    SourceManifest,
)
from rye.cas.store import cas_root, read_ref, write_ref
from rye.cas.sync import (
    collect_object_hashes,
    export_objects,
    handle_get_objects,
    handle_has_objects,
    handle_put_objects,
    import_objects,
)
from rye.constants import AI_DIR

from ryeos_remote.auth import User, get_current_user
from ryeos_remote.config import Settings, get_settings
from ryeos_remote.server import app, _ingest_runtime_outputs

# Load ArtifactStore from bundle via importlib (path has .ai/ dir)
from conftest import PROJECT_ROOT, get_bundle_path
_ARTIFACT_STORE_PATH = get_bundle_path(
    "standard", "tools/rye/agent/threads/persistence/artifact_store.py"
)
_as_spec = importlib.util.spec_from_file_location("artifact_store", _ARTIFACT_STORE_PATH)
_as_mod = importlib.util.module_from_spec(_as_spec)
_as_spec.loader.exec_module(_as_mod)
ArtifactStore = _as_mod.ArtifactStore


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _make_settings(cas_base, signing_dir, **overrides):
    """Create a Settings instance with test defaults."""
    kwargs = dict(
        supabase_url="http://localhost",
        supabase_service_key="fake",
        cas_base_path=str(cas_base),
        signing_key_dir=str(signing_dir),
    )
    kwargs.update(overrides)
    return Settings(**kwargs)


@pytest.fixture
def cas_env(tmp_path):
    """Set up temp CAS, override auth + settings, yield (TestClient, user_cas_root, tmp_path)."""
    cas_base = tmp_path / "cas"
    user_cas = cas_base / "test-user" / ".ai" / "objects"
    user_cas.mkdir(parents=True)

    signing_dir = tmp_path / "signing"
    signing_dir.mkdir()

    settings = _make_settings(cas_base, signing_dir)

    app.dependency_overrides[get_current_user] = lambda: User(
        id="test-user", username="tester",
    )
    app.dependency_overrides[get_settings] = lambda: settings

    # Populate lru_cache so middleware (which calls get_settings() directly) uses ours
    get_settings.cache_clear()
    import unittest.mock
    with unittest.mock.patch("ryeos_remote.config.Settings", return_value=settings):
        get_settings()  # populates the cache with our settings

    with TestClient(app) as c:
        yield c, user_cas, tmp_path

    app.dependency_overrides.clear()
    get_settings.cache_clear()


def _build_manifests(tmp_path, user_cas_root):
    """Build project + user manifests, synced to user's CAS."""
    project = tmp_path / "project_src"
    project.mkdir()
    (project / AI_DIR / "tools").mkdir(parents=True)
    (project / AI_DIR / "tools" / "x.py").write_text("print(1)\n")

    ph, pm = build_manifest(project, "project")

    user = tmp_path / "user_src"
    user.mkdir()
    (user / AI_DIR / "config").mkdir(parents=True)
    (user / AI_DIR / "config" / "agent.yaml").write_text("model: gpt-4\n")
    uh, um = build_manifest(user, "user", project_path=project)

    project_cas = cas_root(project)
    all_hashes = (
        collect_object_hashes(pm, project_cas)
        + collect_object_hashes(um, project_cas)
        + [ph, uh]
    )
    entries = export_objects(list(set(all_hashes)), project_cas)
    import_objects(entries, user_cas_root)

    return ph, uh


# ============================================================================
# Health / Version
# ============================================================================


class TestHealth:
    def test_ok(self, cas_env):
        c, _, _ = cas_env
        r = c.get("/health")
        assert r.status_code == 200
        body = r.json()
        assert body["status"] == "ok"
        assert "version" in body

    def test_version_present(self, cas_env):
        c, _, _ = cas_env
        r = c.get("/health")
        assert r.json()["version"] == get_system_version()


class TestPublicKey:
    def test_auto_generate(self, cas_env):
        c, _, tmp_path = cas_env
        # Signing dir exists but no keys yet — ensure_keypair auto-generates
        r = c.get("/public-key")
        assert r.status_code == 200
        body = r.json()
        assert "public_key_pem" in body
        pem = body["public_key_pem"]
        assert pem.startswith("-----BEGIN PUBLIC KEY-----")
        assert pem.strip().endswith("-----END PUBLIC KEY-----")


# ============================================================================
# Sync Protocol
# ============================================================================


class TestObjectsHas:
    def test_partitions(self, cas_env):
        c, root, _ = cas_env
        h = cas.store_blob(b"exists", root)
        r = c.post("/objects/has", json={"hashes": [h, "0" * 64]})
        assert r.status_code == 200
        assert h in r.json()["present"]
        assert "0" * 64 in r.json()["missing"]

    def test_empty_list(self, cas_env):
        c, _, _ = cas_env
        r = c.post("/objects/has", json={"hashes": []})
        assert r.status_code == 200
        assert r.json()["present"] == []
        assert r.json()["missing"] == []


class TestObjectsPut:
    def test_stores_and_verifies(self, cas_env):
        c, root, _ = cas_env
        data = b"blob content"
        h = hashlib.sha256(data).hexdigest()
        r = c.post("/objects/put", json={"entries": [
            {"hash": h, "kind": "blob", "data": base64.b64encode(data).decode()},
        ]})
        assert r.status_code == 200
        assert h in r.json()["stored"]
        assert cas.get_blob(h, root) == data

    def test_stores_object(self, cas_env):
        c, root, _ = cas_env
        obj = {"kind": "test", "value": 42}
        raw = canonical_json(obj).encode("utf-8")
        h = compute_integrity(obj)
        r = c.post("/objects/put", json={"entries": [
            {"hash": h, "kind": "object", "data": base64.b64encode(raw).decode()},
        ]})
        assert r.status_code == 200
        assert h in r.json()["stored"]
        assert cas.get_object(h, root) == obj

    def test_rejects_wrong_blob_hash(self, cas_env):
        c, _, _ = cas_env
        r = c.post("/objects/put", json={"entries": [
            {"hash": "f" * 64, "kind": "blob", "data": base64.b64encode(b"x").decode()},
        ]})
        assert r.status_code == 400

    def test_rejects_wrong_object_hash(self, cas_env):
        c, _, _ = cas_env
        obj = {"kind": "test", "tampered": True}
        raw = canonical_json(obj).encode("utf-8")
        r = c.post("/objects/put", json={"entries": [
            {"hash": "a" * 64, "kind": "object", "data": base64.b64encode(raw).decode()},
        ]})
        assert r.status_code == 400


class TestObjectsGet:
    def test_retrieves_blob(self, cas_env):
        c, root, _ = cas_env
        h = cas.store_blob(b"fetch me", root)
        r = c.post("/objects/get", json={"hashes": [h]})
        assert r.status_code == 200
        entries = r.json()["entries"]
        assert len(entries) == 1
        assert entries[0]["kind"] == "blob"
        assert base64.b64decode(entries[0]["data"]) == b"fetch me"

    def test_retrieves_object(self, cas_env):
        c, root, _ = cas_env
        obj = {"kind": "test_obj", "x": 1}
        h = cas.store_object(obj, root)
        r = c.post("/objects/get", json={"hashes": [h]})
        assert r.status_code == 200
        entries = r.json()["entries"]
        assert len(entries) == 1
        assert entries[0]["kind"] == "object"
        retrieved = json.loads(base64.b64decode(entries[0]["data"]))
        assert retrieved == obj

    def test_missing_hashes_logged(self, cas_env, caplog):
        c, _, _ = cas_env
        with caplog.at_level(logging.WARNING, logger="rye.cas.sync"):
            r = c.post("/objects/get", json={"hashes": ["0" * 64]})
        assert r.status_code == 200
        assert r.json()["entries"] == []
        assert any("not found" in rec.message for rec in caplog.records)


class TestPutGetRoundtrip:
    def test_roundtrip(self, cas_env):
        c, root, _ = cas_env
        # Put
        data = b"roundtrip data"
        h = hashlib.sha256(data).hexdigest()
        c.post("/objects/put", json={"entries": [
            {"hash": h, "kind": "blob", "data": base64.b64encode(data).decode()},
        ]})
        # Get
        r = c.post("/objects/get", json={"hashes": [h]})
        assert r.status_code == 200
        got = base64.b64decode(r.json()["entries"][0]["data"])
        assert got == data


# ============================================================================
# Execute Endpoint
# ============================================================================


class TestExecute:
    def test_executes_tool(self, cas_env):
        c, root, tmp_path = cas_env
        ph, uh = _build_manifests(tmp_path, root)

        r = c.post("/execute", json={
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "system_version": get_system_version(),
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
        })
        assert r.status_code == 200
        body = r.json()
        assert "execution_snapshot_hash" in body
        assert len(body["execution_snapshot_hash"]) == 64
        assert "result" in body

        snapshot = cas.get_object(body["execution_snapshot_hash"], root)
        assert snapshot["kind"] == "execution_snapshot"
        assert snapshot["project_manifest_hash"] == ph
        assert snapshot["user_manifest_hash"] == uh

    def test_snapshot_has_system_version(self, cas_env):
        c, root, tmp_path = cas_env
        ph, uh = _build_manifests(tmp_path, root)

        r = c.post("/execute", json={
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "system_version": get_system_version(),
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
        })
        body = r.json()
        assert body["system_version"] == get_system_version()
        snapshot = cas.get_object(body["execution_snapshot_hash"], root)
        assert snapshot["system_version"] == get_system_version()

    def test_new_object_hashes_returned(self, cas_env):
        c, root, tmp_path = cas_env
        ph, uh = _build_manifests(tmp_path, root)

        r = c.post("/execute", json={
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "system_version": get_system_version(),
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
        })
        body = r.json()
        assert "new_object_hashes" in body
        assert isinstance(body["new_object_hashes"], list)

    def test_version_mismatch(self, cas_env):
        c, _, _ = cas_env
        r = c.post("/execute", json={
            "project_manifest_hash": "a" * 64,
            "user_manifest_hash": "b" * 64,
            "system_version": "99.99.0",
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
        })
        assert r.status_code == 409

    def test_missing_manifest(self, cas_env):
        c, _, _ = cas_env
        r = c.post("/execute", json={
            "project_manifest_hash": "0" * 64,
            "user_manifest_hash": "0" * 64,
            "system_version": get_system_version(),
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
        })
        assert r.status_code == 404


# ============================================================================
# Thread Enforcement
# ============================================================================


class TestExecuteThreadValidation:
    """Server-side thread enforcement on /execute endpoint."""

    def test_directive_inline_rejected(self, cas_env):
        """Directive + thread=inline → 400 (directives must fork on remote)."""
        c, root, tmp_path = cas_env
        ph, uh = _build_manifests(tmp_path, root)

        r = c.post("/execute", json={
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "system_version": get_system_version(),
            "item_type": "directive",
            "item_id": "test_dir",
            "parameters": {},
            "thread": "inline",
        })
        assert r.status_code == 400
        assert "fork" in r.json()["detail"].lower()

    def test_tool_fork_rejected(self, cas_env):
        """Tool + thread=fork → 400 (tools must run inline on remote)."""
        c, root, tmp_path = cas_env
        ph, uh = _build_manifests(tmp_path, root)

        r = c.post("/execute", json={
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "system_version": get_system_version(),
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
            "thread": "fork",
        })
        assert r.status_code == 400
        assert "inline" in r.json()["detail"].lower()

    def test_tool_inline_accepted(self, cas_env):
        """Tool + thread=inline → accepted (not rejected by thread validation)."""
        c, root, tmp_path = cas_env
        ph, uh = _build_manifests(tmp_path, root)

        r = c.post("/execute", json={
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "system_version": get_system_version(),
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
            "thread": "inline",
        })
        # Should be 200 (may fail in execution, but not rejected by thread validation)
        assert r.status_code == 200

    def test_thread_defaults_to_inline(self, cas_env):
        """Omitting thread field → defaults to 'inline' (tool should work)."""
        c, root, tmp_path = cas_env
        ph, uh = _build_manifests(tmp_path, root)

        r = c.post("/execute", json={
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "system_version": get_system_version(),
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
            # thread field omitted — defaults to "inline"
        })
        # Should not be rejected (default inline is valid for tools)
        assert r.status_code == 200

    def test_directive_fork_not_rejected_by_thread_validation(self, cas_env):
        """Directive + thread=fork → passes thread validation (may fail for other reasons)."""
        c, root, tmp_path = cas_env
        ph, uh = _build_manifests(tmp_path, root)

        r = c.post("/execute", json={
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "system_version": get_system_version(),
            "item_type": "directive",
            "item_id": "test_dir",
            "parameters": {},
            "thread": "fork",
        })
        # Not rejected by thread validation — may get 200 (with error in result)
        # or 404 if directive not found, but NOT 400 for thread mismatch
        assert r.status_code != 400 or "fork" not in r.json().get("detail", "").lower()


# ============================================================================
# _inject_user_secrets
# ============================================================================


class TestInjectUserSecrets:
    """Test _inject_user_secrets uses correct RPC response field."""

    def test_uses_decrypted_value_field(self, tmp_path, monkeypatch):
        """Verify _inject_user_secrets reads 'decrypted_value' not 'decrypted_secret'."""
        from unittest.mock import MagicMock, patch
        from ryeos_remote.server import _inject_user_secrets

        cas_base = tmp_path / "cas"
        signing_dir = tmp_path / "signing"
        cas_base.mkdir()
        signing_dir.mkdir()

        settings = _make_settings(cas_base, signing_dir)
        user = User(id="test-user", username="tester")

        # Mock Supabase RPC to return rows with 'decrypted_value' key
        mock_rpc_result = MagicMock()
        mock_rpc_result.data = [
            {"name": "TEST_SECRET_KEY", "decrypted_value": "secret123"},
        ]
        mock_rpc = MagicMock(return_value=MagicMock(execute=MagicMock(return_value=mock_rpc_result)))
        mock_client = MagicMock()
        mock_client.rpc = mock_rpc

        with patch("supabase.create_client", return_value=mock_client):
            injected = _inject_user_secrets(user, settings)

        # Should have injected the secret
        assert len(injected) == 1
        assert injected[0][0] == "TEST_SECRET_KEY"
        assert os.environ.get("TEST_SECRET_KEY") == "secret123"

        # Clean up
        os.environ.pop("TEST_SECRET_KEY", None)


# ============================================================================
# Secrets Endpoints
# ============================================================================


class TestSecretsEndpoints:
    """Tests for POST /secrets, GET /secrets, DELETE /secrets/{name}."""

    def test_upsert_secrets(self, cas_env):
        """POST /secrets upserts secrets via RPC."""
        c, _, _ = cas_env
        from unittest.mock import MagicMock, patch

        mock_result = MagicMock()
        mock_result.data = None
        mock_rpc = MagicMock(return_value=MagicMock(execute=MagicMock(return_value=mock_result)))
        mock_client = MagicMock()
        mock_client.rpc = mock_rpc

        with patch("ryeos_remote.server._get_supabase", return_value=mock_client):
            r = c.post("/secrets", json={
                "secrets": [
                    {"name": "API_KEY", "value": "secret123"},
                    {"name": "OTHER_KEY", "value": "secret456"},
                ],
            })
        assert r.status_code == 200
        body = r.json()
        assert set(body["stored"]) == {"API_KEY", "OTHER_KEY"}
        assert body["count"] == 2

    def test_upsert_skips_empty(self, cas_env):
        """POST /secrets skips entries with empty name or value."""
        c, _, _ = cas_env
        from unittest.mock import MagicMock, patch

        mock_result = MagicMock()
        mock_result.data = None
        mock_rpc = MagicMock(return_value=MagicMock(execute=MagicMock(return_value=mock_result)))
        mock_client = MagicMock()
        mock_client.rpc = mock_rpc

        with patch("ryeos_remote.server._get_supabase", return_value=mock_client):
            r = c.post("/secrets", json={
                "secrets": [
                    {"name": "", "value": "val"},
                    {"name": "KEY", "value": ""},
                    {"name": "GOOD", "value": "ok"},
                ],
            })
        assert r.status_code == 200
        assert r.json()["stored"] == ["GOOD"]

    def test_list_secrets(self, cas_env):
        """GET /secrets returns secret names only."""
        c, _, _ = cas_env
        from unittest.mock import MagicMock, patch

        mock_result = MagicMock()
        mock_result.data = [
            {"name": "API_KEY", "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"},
            {"name": "DB_URL", "created_at": "2026-01-02T00:00:00Z", "updated_at": "2026-01-02T00:00:00Z"},
        ]
        mock_table = MagicMock()
        mock_table.select.return_value.eq.return_value.order.return_value.execute.return_value = mock_result
        mock_client = MagicMock()
        mock_client.table.return_value = mock_table

        with patch("ryeos_remote.server._get_supabase", return_value=mock_client):
            r = c.get("/secrets")
        assert r.status_code == 200
        body = r.json()
        assert len(body["secrets"]) == 2
        assert body["secrets"][0]["name"] == "API_KEY"

    def test_delete_secret(self, cas_env):
        """DELETE /secrets/{name} removes a secret."""
        c, _, _ = cas_env
        from unittest.mock import MagicMock, patch

        mock_result = MagicMock()
        mock_result.data = True
        mock_rpc = MagicMock(return_value=MagicMock(execute=MagicMock(return_value=mock_result)))
        mock_client = MagicMock()
        mock_client.rpc = mock_rpc

        with patch("ryeos_remote.server._get_supabase", return_value=mock_client):
            r = c.delete("/secrets/API_KEY")
        assert r.status_code == 200
        assert r.json()["deleted"] == "API_KEY"

    def test_delete_secret_not_found(self, cas_env):
        """DELETE /secrets/{name} returns 404 for missing secret."""
        c, _, _ = cas_env
        from unittest.mock import MagicMock, patch

        mock_result = MagicMock()
        mock_result.data = False
        mock_rpc = MagicMock(return_value=MagicMock(execute=MagicMock(return_value=mock_result)))
        mock_client = MagicMock()
        mock_client.rpc = mock_rpc

        with patch("ryeos_remote.server._get_supabase", return_value=mock_client):
            r = c.delete("/secrets/NONEXISTENT")
        assert r.status_code == 404


# ============================================================================
# Security — Path Traversal
# ============================================================================


class TestPathTraversal:
    def test_safe_target_rejects_absolute(self, tmp_path):
        with pytest.raises(ValueError, match="Absolute path"):
            _safe_target(tmp_path, "/etc/passwd")

    def test_safe_target_rejects_escape(self, tmp_path):
        with pytest.raises(ValueError, match="escapes target root"):
            _safe_target(tmp_path, "../escape")

    def test_safe_target_rejects_nested_escape(self, tmp_path):
        with pytest.raises(ValueError, match="escapes target root"):
            _safe_target(tmp_path, "a/b/../../../../escape")

    def test_safe_target_allows_normal_path(self, tmp_path):
        target = _safe_target(tmp_path, "a/b/c.txt")
        assert target == (tmp_path / "a" / "b" / "c.txt").resolve()

    def test_manifest_path_traversal_rejected(self, tmp_path):
        """Manifest with ../escape path is rejected during materialize."""
        root = tmp_path / "cas"
        root.mkdir()

        # Create a blob
        content = b"malicious content"
        blob_hash = cas.store_blob(content, root)

        # Create a manifest with path traversal
        manifest = {
            "schema": 1,
            "kind": "source_manifest",
            "space": "project",
            "items": {},
            "files": {"../escape.txt": blob_hash},
        }
        manifest_hash = cas.store_object(manifest, root)

        target_dir = tmp_path / "target"
        target_dir.mkdir()

        with pytest.raises(ValueError, match="escapes target root"):
            from rye.cas.materializer import _materialize_manifest
            _materialize_manifest(manifest_hash, target_dir, root)

    def test_manifest_absolute_path_rejected(self, tmp_path):
        """Manifest with /etc/passwd path is rejected during materialize."""
        root = tmp_path / "cas"
        root.mkdir()

        content = b"malicious"
        blob_hash = cas.store_blob(content, root)

        manifest = {
            "schema": 1,
            "kind": "source_manifest",
            "space": "project",
            "items": {},
            "files": {"/etc/passwd": blob_hash},
        }
        manifest_hash = cas.store_object(manifest, root)

        target_dir = tmp_path / "target"
        target_dir.mkdir()

        with pytest.raises(ValueError, match="Absolute path"):
            from rye.cas.materializer import _materialize_manifest
            _materialize_manifest(manifest_hash, target_dir, root)

    def test_manifest_items_path_traversal_rejected(self, tmp_path):
        """Manifest with ../escape in items path is rejected."""
        root = tmp_path / "cas"
        root.mkdir()

        content = b"tool content"
        blob_hash = cas.store_blob(content, root)
        item_source = {
            "kind": "item_source",
            "item_type": "tool",
            "item_id": "evil",
            "content_blob_hash": blob_hash,
            "integrity": hashlib.sha256(content).hexdigest(),
        }
        item_hash = cas.store_object(item_source, root)

        manifest = {
            "schema": 1,
            "kind": "source_manifest",
            "space": "project",
            "items": {"../../../etc/evil.py": item_hash},
            "files": {},
        }
        manifest_hash = cas.store_object(manifest, root)

        target_dir = tmp_path / "target"
        target_dir.mkdir()

        with pytest.raises(ValueError, match="escapes target root"):
            from rye.cas.materializer import _materialize_manifest
            _materialize_manifest(manifest_hash, target_dir, root)


# ============================================================================
# Limits / Quotas
# ============================================================================


class TestRequestLimits:
    def test_body_exceeds_limit(self, tmp_path):
        """POST body > limit → 413."""
        cas_base = tmp_path / "cas"
        user_cas = cas_base / "test-user" / ".ai" / "objects"
        user_cas.mkdir(parents=True)
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()

        settings = _make_settings(cas_base, signing_dir, max_request_bytes=1024)
        self._apply_overrides(settings)

        try:
            with TestClient(app) as c:
                big_payload = {"hashes": ["a" * 64] * 100}
                r = c.post("/objects/has", json=big_payload)
                assert r.status_code == 413
        finally:
            self._clear_overrides()

    def test_user_quota_exceeded(self, tmp_path):
        """User CAS > quota → 507 on put_objects."""
        cas_base = tmp_path / "cas"
        user_cas = cas_base / "test-user" / ".ai" / "objects"
        user_cas.mkdir(parents=True)
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()

        # Fill user CAS over a tiny quota
        filler = user_cas / "filler.bin"
        filler.write_bytes(b"x" * 2048)

        settings = _make_settings(cas_base, signing_dir, max_user_storage_bytes=1024)
        self._apply_overrides(settings)

        try:
            with TestClient(app) as c:
                data = b"new blob"
                h = hashlib.sha256(data).hexdigest()
                r = c.post("/objects/put", json={"entries": [
                    {"hash": h, "kind": "blob", "data": base64.b64encode(data).decode()},
                ]})
                assert r.status_code == 507
        finally:
            self._clear_overrides()

    def test_request_no_content_length(self, tmp_path):
        """POST without Content-Length header, body > limit → 413."""
        cas_base = tmp_path / "cas"
        user_cas = cas_base / "test-user" / ".ai" / "objects"
        user_cas.mkdir(parents=True)
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()

        settings = _make_settings(cas_base, signing_dir, max_request_bytes=64)
        self._apply_overrides(settings)

        try:
            with TestClient(app) as c:
                # Send raw bytes without explicit Content-Length
                # TestClient always adds Content-Length, but the middleware
                # also checks body size for POST/PUT/PATCH
                big_data = b"x" * 128
                r = c.post(
                    "/objects/has",
                    content=big_data,
                    headers={"Content-Type": "application/json"},
                )
                assert r.status_code == 413
        finally:
            self._clear_overrides()

    def test_post_execute_quota_check(self, cas_env):
        """Execution output that pushes user over quota is flagged.

        We verify the server checks quota before /objects/put.
        If user is already over quota, new puts are rejected.
        """
        c, root, tmp_path = cas_env

        # Fill user CAS to be near-full relative to default 1GB quota
        # Since we're using the default cas_env with 1GB quota, just verify
        # the quota check mechanism works by filling over a smaller limit
        # This is already covered by test_user_quota_exceeded above;
        # here we verify the _check_user_quota function directly
        from ryeos_remote.server import _check_user_quota
        from ryeos_remote.config import Settings

        filler = root / "filler.bin"
        filler.write_bytes(b"x" * 2048)

        user = User(id="test-user", username="tester")
        small_settings = _make_settings(
            root.parent.parent.parent,  # cas_base
            tmp_path / "signing",
            max_user_storage_bytes=1024,
        )

        from fastapi import HTTPException
        with pytest.raises(HTTPException) as exc_info:
            _check_user_quota(user, small_settings)
        assert exc_info.value.status_code == 507

    @staticmethod
    def _apply_overrides(settings):
        import unittest.mock
        app.dependency_overrides[get_current_user] = lambda: User(
            id="test-user", username="tester",
        )
        app.dependency_overrides[get_settings] = lambda: settings
        get_settings.cache_clear()
        with unittest.mock.patch("ryeos_remote.config.Settings", return_value=settings):
            get_settings()

    @staticmethod
    def _clear_overrides():
        app.dependency_overrides.clear()
        get_settings.cache_clear()


# ============================================================================
# Sync Protocol (unit-level)
# ============================================================================


class TestSyncProtocol:
    def test_has_objects_present_missing(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        h = cas.store_blob(b"present", root)
        result = handle_has_objects([h, "0" * 64], root)
        assert h in result["present"]
        assert "0" * 64 in result["missing"]

    def test_put_get_roundtrip(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        data = b"round trip"
        h = hashlib.sha256(data).hexdigest()
        entries = [{"hash": h, "kind": "blob", "data": base64.b64encode(data).decode()}]
        put_result = handle_put_objects(entries, root)
        assert h in put_result["stored"]

        get_result = handle_get_objects([h], root)
        assert len(get_result["entries"]) == 1
        assert base64.b64decode(get_result["entries"][0]["data"]) == data

    def test_import_objects_errors_raise(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        bad_entries = [{"hash": "f" * 64, "kind": "blob", "data": base64.b64encode(b"bad").decode()}]
        with pytest.raises(ValueError, match="CAS import failed"):
            import_objects(bad_entries, root)


# ============================================================================
# Materializer
# ============================================================================


class TestMaterializer:
    def test_materialize_roundtrip(self, tmp_path):
        """Build manifest → materialize → verify files exist."""
        root = tmp_path / "cas"
        root.mkdir()

        # Build a manifest with items and files
        content = b"tool content"
        blob_hash = cas.store_blob(content, root)

        item_source = ItemSource(
            item_type="tool",
            item_id="hello",
            content_blob_hash=blob_hash,
            integrity=hashlib.sha256(content).hexdigest(),
        )
        item_hash = cas.store_object(item_source.to_dict(), root)

        file_content = b"readme content"
        file_blob_hash = cas.store_blob(file_content, root)

        manifest = SourceManifest(
            space="project",
            items={".ai/tools/hello.py": item_hash},
            files={"README.md": file_blob_hash},
        )
        manifest_hash = cas.store_object(manifest.to_dict(), root)

        # Empty user manifest
        empty_manifest = SourceManifest(space="user")
        empty_hash = cas.store_object(empty_manifest.to_dict(), root)

        paths = materialize(manifest_hash, empty_hash, root, tmp_base=tmp_path)
        try:
            # Items are materialized
            tool_file = paths.project_path / ".ai" / "tools" / "hello.py"
            assert tool_file.exists()
            assert tool_file.read_bytes() == content

            # Files are materialized
            readme = paths.project_path / "README.md"
            assert readme.exists()
            assert readme.read_bytes() == file_content
        finally:
            cleanup(paths)

    def test_cleanup_removes_temp(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()

        empty_manifest = SourceManifest(space="project")
        m_hash = cas.store_object(empty_manifest.to_dict(), root)

        paths = materialize(m_hash, m_hash, root, tmp_base=tmp_path)
        base = paths._base
        assert base.exists()
        cleanup(paths)
        assert not base.exists()

    def test_missing_manifest_raises(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()

        with pytest.raises(FileNotFoundError, match="Manifest object"):
            materialize("0" * 64, "0" * 64, root, tmp_base=tmp_path)

    def test_missing_blob_raises(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()

        manifest = {
            "schema": 1,
            "kind": "source_manifest",
            "space": "project",
            "items": {},
            "files": {"data.txt": "0" * 64},  # non-existent blob
        }
        manifest_hash = cas.store_object(manifest, root)

        empty_manifest = SourceManifest(space="user")
        empty_hash = cas.store_object(empty_manifest.to_dict(), root)

        with pytest.raises(FileNotFoundError, match="Blob"):
            materialize(manifest_hash, empty_hash, root, tmp_base=tmp_path)


# ============================================================================
# ArtifactStore
# ============================================================================


class TestArtifactStore:
    def _make_store(self, tmp_path, thread_id="thread-001"):
        project_path = tmp_path / "project"
        project_path.mkdir(exist_ok=True)
        (project_path / AI_DIR / "objects").mkdir(parents=True, exist_ok=True)
        return ArtifactStore(thread_id, project_path), project_path

    def test_store_retrieve_roundtrip(self, tmp_path):
        store, _ = self._make_store(tmp_path)
        data = {"key": "value", "number": 42}
        content_hash = store.store("call-1", "test_tool", data)
        assert len(content_hash) == 64

        result = store.retrieve("call-1")
        assert result is not None
        assert result["data"] == data
        assert result["tool_name"] == "test_tool"
        assert result["content_hash"] == content_hash

    def test_has_content_dedup(self, tmp_path):
        store, _ = self._make_store(tmp_path)
        data = {"dedup": "test"}
        store.store("call-a", "tool_a", data)
        store.store("call-b", "tool_b", data)  # same content

        # Both exist
        content_hash = hashlib.sha256(
            json.dumps(data, sort_keys=True, default=str).encode()
        ).hexdigest()
        found = store.has_content(content_hash)
        assert found in ("call-a", "call-b")

    def test_thread_isolation(self, tmp_path):
        store_a, _ = self._make_store(tmp_path, "thread-A")
        store_b, _ = self._make_store(tmp_path, "thread-B")

        store_a.store("call-1", "tool", {"data": "A"})
        assert store_a.retrieve("call-1") is not None
        assert store_b.retrieve("call-1") is None

    def test_corrupt_index_ref_raises(self, tmp_path):
        store, project_path = self._make_store(tmp_path, "thread-corrupt")

        # Write a ref pointing to a non-existent object
        ref_path = (
            project_path / AI_DIR / "objects" / "refs"
            / "artifacts" / "thread-corrupt.json"
        )
        write_ref(ref_path, "0" * 64)

        with pytest.raises(RuntimeError, match="missing object"):
            store.retrieve("call-1")

    def test_retrieve_nonexistent_returns_none(self, tmp_path):
        store, _ = self._make_store(tmp_path)
        assert store.retrieve("nonexistent") is None


# ============================================================================
# Trust / Key Verification
# ============================================================================


class TestTrustStore:
    def _setup_user(self, tmp_path, monkeypatch):
        """Set up user space with signing key trusted in trust store."""
        user_space = tmp_path / "user"
        user_space.mkdir(exist_ok=True)
        monkeypatch.setenv("USER_SPACE", str(user_space))
        monkeypatch.delenv("RYE_SIGNING_KEY_DIR", raising=False)

        from rye.utils.signature_formats import clear_signature_formats_cache
        clear_signature_formats_cache()

        signing_dir = user_space / AI_DIR / "config" / "keys" / "signing"
        private_pem, public_pem = generate_keypair()
        save_keypair(private_pem, public_pem, signing_dir)

        from rye.utils.trust_store import TrustStore
        ts = TrustStore(project_path=tmp_path / "project")
        # Trust the signing key so signed trust entries pass integrity
        ts.add_key(public_pem, owner="local", space="user")
        return ts

    def test_tofu_first_pin(self, tmp_path, monkeypatch):
        ts = self._setup_user(tmp_path, monkeypatch)

        _, remote_pub = generate_keypair()
        fp = ts.pin_remote_key(remote_pub)
        assert len(fp) == 16

        key = ts.get_remote_key()
        assert key is not None
        assert compute_key_fingerprint(key) == fp

    def test_pinned_key_match(self, tmp_path, monkeypatch):
        ts = self._setup_user(tmp_path, monkeypatch)

        _, remote_pub = generate_keypair()
        fp1 = ts.pin_remote_key(remote_pub)
        fp2 = ts.pin_remote_key(remote_pub)  # same key — no-op
        assert fp1 == fp2

    def test_tampered_key_file_skipped(self, tmp_path, monkeypatch):
        ts = self._setup_user(tmp_path, monkeypatch)

        _, remote_pub = generate_keypair()
        fp = ts.pin_remote_key(remote_pub, remote_name="ryeos-remote")

        # Tamper with the key file
        trust_dir = tmp_path / "user" / AI_DIR / "config" / "keys" / "trusted"
        key_file = trust_dir / f"{fp}.toml"
        content = key_file.read_text()
        tampered = content.replace('owner = "ryeos-remote"', 'owner = "evil"')
        key_file.write_text(tampered)

        # get_remote_key should return None for tampered file
        result = ts.get_remote_key()
        assert result is None

    def test_find_key_skips_invalid_returns_valid(self, tmp_path, monkeypatch):
        """First matching owner is tampered, second is valid → returns valid one."""
        ts = self._setup_user(tmp_path, monkeypatch)

        # Pin two different keys with the same owner
        _, pub1 = generate_keypair()
        fp1 = ts.pin_remote_key(pub1, remote_name="ryeos-remote")

        _, pub2 = generate_keypair()
        fp2 = ts.pin_remote_key(pub2, remote_name="ryeos-remote")

        # Tamper with the first key file
        trust_dir = tmp_path / "user" / AI_DIR / "config" / "keys" / "trusted"
        key_file1 = trust_dir / f"{fp1}.toml"
        content = key_file1.read_text()
        key_file1.write_text(content.replace(f'fingerprint = "{fp1}"', 'fingerprint = "tampered"'))

        # _find_key_fp_by_owner should skip the tampered one and return the valid one
        result = ts.get_remote_key()
        assert result is not None
        assert compute_key_fingerprint(result) == fp2


# ============================================================================
# Remote Key Verification (client-side _verify_remote_key)
# ============================================================================


class TestRemoteKeyVerification:
    """Tests for _verify_remote_key in the remote tool (client-side logic)."""

    def _setup_user(self, tmp_path, monkeypatch):
        """Set up user space with signing key trusted in trust store."""
        user_space = tmp_path / "user"
        user_space.mkdir(exist_ok=True)
        monkeypatch.setenv("USER_SPACE", str(user_space))
        monkeypatch.delenv("RYE_SIGNING_KEY_DIR", raising=False)

        from rye.utils.signature_formats import clear_signature_formats_cache
        clear_signature_formats_cache()

        signing_dir = user_space / AI_DIR / "config" / "keys" / "signing"
        private_pem, public_pem = generate_keypair()
        save_keypair(private_pem, public_pem, signing_dir)

        from rye.utils.trust_store import TrustStore
        ts = TrustStore(project_path=tmp_path / "project")
        ts.add_key(public_pem, owner="local", space="user")
        return ts

    @pytest.mark.asyncio
    async def test_pinned_key_mismatch_fails(self, tmp_path, monkeypatch):
        """Different key from server → hard error dict returned."""
        ts = self._setup_user(tmp_path, monkeypatch)

        # Pin a key using the URL-specific owner format
        _, first_pub = generate_keypair()
        ts.pin_remote_key(first_pub, remote_name="remote:default:mock.example.com")

        # Simulate server returning a DIFFERENT key
        _, second_pub = generate_keypair()
        second_pem = second_pub.decode("utf-8")

        class MockClient:
            base_url = "https://mock.example.com"

            async def get(self, path):
                return {
                    "success": True,
                    "status_code": 200,
                    "body": {"public_key_pem": second_pem},
                    "error": None,
                }

        # Import and call _verify_remote_key
        _remote_path = get_bundle_path("core", "tools/rye/core/remote/remote.py")
        _r_spec = importlib.util.spec_from_file_location("remote_tool", _remote_path)
        _r_mod = importlib.util.module_from_spec(_r_spec)
        _r_spec.loader.exec_module(_r_mod)

        result = await _r_mod._verify_remote_key(MockClient())
        assert result is not None
        assert "error" in result
        assert "mismatch" in result["error"]

    @pytest.mark.asyncio
    async def test_key_fetch_failure_fails(self, tmp_path, monkeypatch):
        """Server /public-key unreachable → hard error dict returned."""
        self._setup_user(tmp_path, monkeypatch)

        class MockClient:
            base_url = "https://mock.example.com"

            async def get(self, path):
                return {
                    "success": False,
                    "status_code": 0,
                    "body": None,
                    "error": "Connection refused",
                }

        _remote_path = get_bundle_path("core", "tools/rye/core/remote/remote.py")
        _r_spec = importlib.util.spec_from_file_location("remote_tool_fetch", _remote_path)
        _r_mod = importlib.util.module_from_spec(_r_spec)
        _r_spec.loader.exec_module(_r_mod)

        result = await _r_mod._verify_remote_key(MockClient())
        assert result is not None
        assert "error" in result
        assert "Could not verify" in result["error"]


# ============================================================================
# Server CAS copy integrity
# ============================================================================


class TestCopyCasObjects:
    def test_valid_blobs_copied(self, tmp_path):
        from ryeos_remote.server import _copy_cas_objects

        src = tmp_path / "src"
        dst = tmp_path / "dst"
        src.mkdir()
        dst.mkdir()

        # Store a blob in src
        h = cas.store_blob(b"test blob", src)

        copied = _copy_cas_objects(src, dst)
        assert h in copied
        assert cas.get_blob(h, dst) == b"test blob"

    def test_valid_objects_copied(self, tmp_path):
        from ryeos_remote.server import _copy_cas_objects

        src = tmp_path / "src"
        dst = tmp_path / "dst"
        src.mkdir()
        dst.mkdir()

        obj = {"kind": "test", "value": 1}
        h = cas.store_object(obj, src)

        copied = _copy_cas_objects(src, dst)
        assert h in copied
        assert cas.get_object(h, dst) == obj

    def test_mismatched_blob_raises(self, tmp_path):
        from ryeos_remote.server import _copy_cas_objects

        src = tmp_path / "src"
        dst = tmp_path / "dst"
        src.mkdir()
        dst.mkdir()

        # Store normally first to get the shard path
        h = cas.store_blob(b"original", src)

        # Overwrite the blob file with different content (simulating corruption)
        from lillux.primitives.cas import _shard_path
        blob_path = _shard_path(src, "blobs", h)
        blob_path.write_bytes(b"tampered content")

        with pytest.raises(RuntimeError, match="Blob hash mismatch"):
            _copy_cas_objects(src, dst)

        # Dst should NOT contain the corrupted blob
        assert cas.get_blob(h, dst) is None

    def test_invalid_object_json_raises(self, tmp_path):
        from ryeos_remote.server import _copy_cas_objects

        src = tmp_path / "src"
        dst = tmp_path / "dst"
        src.mkdir()
        dst.mkdir()

        # Write an invalid JSON file in objects dir
        obj_dir = src / "objects" / "ab" / "cd"
        obj_dir.mkdir(parents=True)
        (obj_dir / "abcd0000.json").write_bytes(b"not valid json {{{")

        with pytest.raises(RuntimeError, match="Invalid CAS object file"):
            _copy_cas_objects(src, dst)


# ============================================================================
# Collect + Export (sync helpers)
# ============================================================================


class TestCollectObjectHashes:
    def test_collects_item_blobs(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()

        blob_hash = cas.store_blob(b"content", root)
        item_source = {"content_blob_hash": blob_hash, "kind": "item_source"}
        item_hash = cas.store_object(item_source, root)

        manifest = {"items": {"test.py": item_hash}, "files": {}}
        hashes = collect_object_hashes(manifest, root)
        assert item_hash in hashes
        assert blob_hash in hashes

    def test_collects_file_blobs(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()

        blob_hash = cas.store_blob(b"readme", root)
        manifest = {"items": {}, "files": {"README.md": blob_hash}}
        hashes = collect_object_hashes(manifest, root)
        assert blob_hash in hashes


# ============================================================================
# Refs
# ============================================================================


class TestRefs:
    def test_write_read_ref(self, tmp_path):
        ref_path = tmp_path / "refs" / "test.json"
        write_ref(ref_path, "abc123")
        assert read_ref(ref_path) == "abc123"

    def test_read_missing_ref(self, tmp_path):
        assert read_ref(tmp_path / "nonexistent.json") is None

    def test_write_ref_atomic_overwrite(self, tmp_path):
        ref_path = tmp_path / "refs" / "test.json"
        write_ref(ref_path, "first")
        write_ref(ref_path, "second")
        assert read_ref(ref_path) == "second"


# ============================================================================
# RuntimeOutputsBundle — server-side ingestion
# ============================================================================


class TestIngestRuntimeOutputs:
    """Tests for _ingest_runtime_outputs() — CAS ingestion of runtime files."""

    def _setup_project(self, tmp_path):
        """Create a fake materialized project with runtime output files."""
        project = tmp_path / "project"
        project.mkdir()

        dst_root = project / AI_DIR / "objects"
        dst_root.mkdir(parents=True)

        # Graph transcript
        graph_dir = project / AI_DIR / "agent" / "graphs" / "run-123"
        graph_dir.mkdir(parents=True)
        (graph_dir / "transcript.jsonl").write_text(
            '{"event": "step_started", "node": "setup"}\n'
        )

        # Thread transcript + metadata
        thread_dir = project / AI_DIR / "agent" / "threads" / "t-abc"
        thread_dir.mkdir(parents=True)
        (thread_dir / "transcript.jsonl").write_text(
            '{"event": "cognition_in", "text": "hello"}\n'
        )
        (thread_dir / "thread.json").write_text(
            '{"thread_id": "t-abc", "status": "completed"}\n'
        )
        (thread_dir / "capabilities.md").write_text("# Capabilities\n")

        # Knowledge markdown
        knowledge_dir = project / AI_DIR / "knowledge" / "agent" / "graphs" / "test"
        knowledge_dir.mkdir(parents=True)
        (knowledge_dir / "run-123.md").write_text("# Graph Report\n")

        # Ref pointer
        refs_dir = project / AI_DIR / "objects" / "refs" / "graphs"
        refs_dir.mkdir(parents=True)
        (refs_dir / "run-123.json").write_text('{"hash": "deadbeef"}')

        return project, dst_root

    def test_ingests_all_runtime_files(self, tmp_path):
        project, dst_root = self._setup_project(tmp_path)

        bundle_hash, new_hashes = _ingest_runtime_outputs(
            project, dst_root, "thread-1", "snap-hash",
        )

        assert bundle_hash
        assert len(new_hashes) > 0

        # Bundle object should be in CAS
        obj = cas.get_object(bundle_hash, dst_root)
        assert obj is not None
        assert obj["kind"] == "runtime_outputs_bundle"
        assert obj["remote_thread_id"] == "thread-1"
        assert obj["execution_snapshot_hash"] == "snap-hash"

        files = obj["files"]
        # Should have: 2 transcripts, thread.json, capabilities.md, knowledge, ref
        assert len(files) == 6

        # All blob hashes should be retrievable
        for rel_path, blob_hash in files.items():
            blob = cas.get_blob(blob_hash, dst_root)
            assert blob is not None, f"Blob missing for {rel_path}"

    def test_returns_all_hashes_for_pull(self, tmp_path):
        """Bundle hash AND all blob hashes must be in new_hashes."""
        project, dst_root = self._setup_project(tmp_path)

        bundle_hash, new_hashes = _ingest_runtime_outputs(
            project, dst_root, "thread-1", "snap-hash",
        )

        # Bundle object hash is included
        assert bundle_hash in new_hashes

        # All blob hashes are included
        obj = cas.get_object(bundle_hash, dst_root)
        for blob_hash in obj["files"].values():
            assert blob_hash in new_hashes

    def test_rejects_symlinks(self, tmp_path):
        project, dst_root = self._setup_project(tmp_path)

        # Create a symlink in agent dir pointing outside
        agent_dir = project / AI_DIR / "agent" / "graphs" / "evil"
        agent_dir.mkdir(parents=True)
        target = tmp_path / "secret.txt"
        target.write_text("sensitive data")
        (agent_dir / "link.jsonl").symlink_to(target)

        bundle_hash, new_hashes = _ingest_runtime_outputs(
            project, dst_root, "thread-1", "snap-hash",
        )

        # Symlink should NOT be in the bundle
        obj = cas.get_object(bundle_hash, dst_root)
        for rel_path in obj["files"]:
            assert "evil/link.jsonl" not in rel_path

    def test_empty_project_returns_no_bundle(self, tmp_path):
        project = tmp_path / "empty"
        project.mkdir()
        (project / AI_DIR / "objects").mkdir(parents=True)

        bundle_hash, new_hashes = _ingest_runtime_outputs(
            project, tmp_path / "cas", "thread-1", "snap-hash",
        )

        assert bundle_hash == ""
        assert new_hashes == []

    def test_bundle_object_schema(self, tmp_path):
        """Verify RuntimeOutputsBundle dataclass produces correct dict."""
        bundle = RuntimeOutputsBundle(
            remote_thread_id="t-1",
            execution_snapshot_hash="snap-1",
            files={".ai/agent/test.jsonl": "abc123"},
        )
        d = bundle.to_dict()
        assert d["kind"] == "runtime_outputs_bundle"
        assert d["schema"] == 1
        assert d["remote_thread_id"] == "t-1"
        assert d["execution_snapshot_hash"] == "snap-1"
        assert d["files"] == {".ai/agent/test.jsonl": "abc123"}


# ============================================================================
# RuntimeOutputsBundle — client-side materialization
# ============================================================================


# Load _materialize_runtime_outputs from core bundle
_REMOTE_TOOL_PATH = get_bundle_path("core", "tools/rye/core/remote/remote.py")
_rt_spec = importlib.util.spec_from_file_location("remote_tool", _REMOTE_TOOL_PATH)
_rt_mod = importlib.util.module_from_spec(_rt_spec)
_rt_spec.loader.exec_module(_rt_mod)
_materialize_runtime_outputs = _rt_mod._materialize_runtime_outputs


class TestMaterializeRuntimeOutputs:
    """Tests for client-side materialization of RuntimeOutputsBundle."""

    def _create_bundle_in_cas(self, project_path, files_content):
        """Store file blobs + bundle object in local CAS. Returns bundle hash."""
        root = cas_root(project_path)
        root.mkdir(parents=True, exist_ok=True)

        file_map = {}
        for rel_path, content in files_content.items():
            blob_hash = cas.store_blob(content.encode(), root)
            file_map[rel_path] = blob_hash

        bundle = RuntimeOutputsBundle(
            remote_thread_id="rye-remote-test123",
            execution_snapshot_hash="snap-abc",
            files=file_map,
        )
        return cas.store_object(bundle.to_dict(), root)

    def test_materializes_files_to_local_tree(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()

        bundle_hash = self._create_bundle_in_cas(project, {
            ".ai/agent/graphs/run-1/transcript.jsonl": '{"event": "started"}\n',
            ".ai/agent/threads/t-1/thread.json": '{"status": "completed"}\n',
            ".ai/knowledge/agent/graphs/test/run-1.md": "# Report\n",
        })

        count = _materialize_runtime_outputs(bundle_hash, project)
        assert count == 3

        assert (project / ".ai/agent/graphs/run-1/transcript.jsonl").exists()
        assert (project / ".ai/agent/threads/t-1/thread.json").exists()
        assert (project / ".ai/knowledge/agent/graphs/test/run-1.md").exists()

        # Content should match
        assert (project / ".ai/agent/graphs/run-1/transcript.jsonl").read_text() == '{"event": "started"}\n'

    def test_refs_materialized(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()

        bundle_hash = self._create_bundle_in_cas(project, {
            ".ai/objects/refs/graphs/run-1.json": '{"hash": "abc"}',
        })

        count = _materialize_runtime_outputs(bundle_hash, project)
        assert count == 1
        assert (project / ".ai/objects/refs/graphs/run-1.json").read_text() == '{"hash": "abc"}'

    def test_rejects_paths_outside_allowlist(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()

        bundle_hash = self._create_bundle_in_cas(project, {
            ".ai/agent/graphs/run-1/transcript.jsonl": "ok",
            ".ai/tools/evil.py": "import os; os.system('rm -rf /')",
            "src/main.py": "print('injected')",
        })

        count = _materialize_runtime_outputs(bundle_hash, project)
        assert count == 1  # only the agent file
        assert not (project / ".ai/tools/evil.py").exists()
        assert not (project / "src/main.py").exists()

    def test_rejects_path_traversal(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()

        bundle_hash = self._create_bundle_in_cas(project, {
            ".ai/agent/../../etc/passwd": "root:x:0:0",
        })

        count = _materialize_runtime_outputs(bundle_hash, project)
        assert count == 0

    def test_missing_bundle_returns_zero(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        (project / AI_DIR / "objects").mkdir(parents=True)

        count = _materialize_runtime_outputs("nonexistent_hash", project)
        assert count == 0

    def test_missing_blob_skips_file(self, tmp_path):
        project = tmp_path / "project"
        project.mkdir()
        root = cas_root(project)
        root.mkdir(parents=True)

        # Store bundle with a fake blob hash that doesn't exist
        bundle = RuntimeOutputsBundle(
            remote_thread_id="t-1",
            execution_snapshot_hash="s-1",
            files={".ai/agent/test/transcript.jsonl": "0" * 64},
        )
        bundle_hash = cas.store_object(bundle.to_dict(), root)

        count = _materialize_runtime_outputs(bundle_hash, project)
        assert count == 0  # blob missing, file skipped
