"""Tests for ryeos-remote server endpoints.

Uses FastAPI TestClient with auth dependency overrides.
Covers: happy path, security, limits/quotas, trust/key verification,
sync protocol, materializer, ArtifactStore, health/version endpoints.
"""

import asyncio
import base64
import hashlib
import importlib.util
import json
import logging
import os
import time
from pathlib import Path

import pytest
from fastapi import HTTPException, Request
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
    ProjectSnapshot,
    RuntimeOutputsBundle,
    SourceManifest,
    get_history,
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

from ryeos_remote.auth import Principal, ResolvedExecution, get_current_principal, require_capability
from ryeos_remote.config import Settings, get_settings
from ryeos_remote.server import (
    app,
    resolve_execution,
    _execute_from_head,
    _ingest_runtime_outputs,
    _lookup_binding,
    _resolve_principal_from_binding,
    _resolve_project_ref_or_404,
    _load_manifest_from_snapshot,
    _try_advance_head,
    _fold_back,
    _update_snapshot_cache,
    _validate_manifest_graph,
    MAX_FOLD_BACK_RETRIES,
)
from ryeos_remote.refs import resolve_project_ref, advance_project_ref, resolve_user_space_ref, init_project_ref
from ryeos_remote.executions import register_execution, complete_execution, get_execution, store_conflict_record

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
        cas_base_path=str(cas_base),
        signing_key_dir=str(signing_dir),
    )
    kwargs.update(overrides)
    return Settings(**kwargs)


TEST_PRINCIPAL = Principal(
    fingerprint="test-user",
    capabilities=["rye.*"],
    owner="tester",
)


@pytest.fixture
def cas_env(tmp_path):
    """Set up temp CAS, override auth + settings, yield (TestClient, user_cas_root, tmp_path)."""
    cas_base = tmp_path / "cas"
    user_cas = cas_base / "test-user" / ".ai" / "objects"
    user_cas.mkdir(parents=True)

    signing_dir = tmp_path / "signing"
    signing_dir.mkdir()

    settings = _make_settings(cas_base, signing_dir)

    app.dependency_overrides[get_current_principal] = lambda: TEST_PRINCIPAL
    app.dependency_overrides[get_settings] = lambda: settings

    async def _mock_resolve_execution(request: Request):
        """Test override: parse body like signed-request path, skip real auth.

        Replicates validation from the real resolve_execution so thread
        enforcement and field validation tests still work.
        """
        import json as _json
        from fastapi import HTTPException as _HTTPException
        raw = await request.body()
        body = _json.loads(raw)
        item_type = body.get("item_type")
        item_id = body.get("item_id")
        project_path = body.get("project_path")
        parameters = body.get("parameters", {})
        thread = body.get("thread")

        if not item_type or not item_id:
            raise _HTTPException(400, "item_type and item_id are required")
        if item_type not in ("tool", "directive"):
            raise _HTTPException(400, f"item_type must be 'tool' or 'directive', got {item_type!r}")
        if not project_path:
            raise _HTTPException(400, "project_path is required")
        if not isinstance(parameters, dict):
            raise _HTTPException(400, "parameters must be an object")
        if not thread:
            thread = "fork" if item_type == "directive" else "inline"
        if item_type == "directive" and thread != "fork":
            raise _HTTPException(400, f"Directives must use thread=fork on remote, got thread={thread!r}")
        if item_type == "tool" and thread != "inline":
            raise _HTTPException(400, f"Tools must use thread=inline on remote, got thread={thread!r}")

        return ResolvedExecution(
            principal=TEST_PRINCIPAL,
            item_type=item_type,
            item_id=item_id,
            project_path=project_path,
            parameters=parameters,
            thread=thread,
        )
    app.dependency_overrides[resolve_execution] = _mock_resolve_execution

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


def _build_manifests_with_snapshot(tmp_path, user_cas_root):
    """Build manifests + snapshot, return (ph, uh, snapshot_hash, mock_ref, mock_user_ref)."""
    ph, uh = _build_manifests(tmp_path, user_cas_root)
    snapshot = ProjectSnapshot(
        project_manifest_hash=ph,
        user_manifest_hash=uh,
        source="push",
    )
    snapshot_hash = cas.store_object(snapshot.to_dict(), user_cas_root)
    mock_ref = {
        "project_manifest_hash": ph,
        "system_version": get_system_version(),
        "snapshot_hash": snapshot_hash,
        "project_path": "test-project",
    }
    mock_user_ref = {
        "user_manifest_hash": uh,
        "pushed_at": "2026-01-01T00:00:00+00:00",
    }
    return ph, uh, snapshot_hash, mock_ref, mock_user_ref


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
        from unittest.mock import patch

        ph, uh, snapshot_hash, mock_ref, mock_user_ref = _build_manifests_with_snapshot(tmp_path, root)

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref):
            r = c.post("/execute", json={
                "project_path": "test-project",
                "item_type": "tool",
                "item_id": "x",
                "parameters": {},
            })
        assert r.status_code == 200
        body = r.json()
        assert "execution_snapshot_hash" in body
        assert len(body["execution_snapshot_hash"]) == 64
        assert "result" in body

    def test_system_version_returned(self, cas_env):
        c, root, tmp_path = cas_env
        from unittest.mock import patch

        ph, uh, snapshot_hash, mock_ref, mock_user_ref = _build_manifests_with_snapshot(tmp_path, root)

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref):
            r = c.post("/execute", json={
                "project_path": "test-project",
                "item_type": "tool",
                "item_id": "x",
                "parameters": {},
            })
        body = r.json()
        assert body["system_version"] == get_system_version()

    def test_new_object_hashes_returned(self, cas_env):
        c, root, tmp_path = cas_env
        from unittest.mock import patch

        ph, uh, snapshot_hash, mock_ref, mock_user_ref = _build_manifests_with_snapshot(tmp_path, root)

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref):
            r = c.post("/execute", json={
                "project_path": "test-project",
                "item_type": "tool",
                "item_id": "x",
                "parameters": {},
            })
        body = r.json()
        assert "new_object_hashes" in body
        assert isinstance(body["new_object_hashes"], list)

    def test_missing_project(self, cas_env):
        c, _, _ = cas_env
        r = c.post("/execute", json={
            "item_type": "tool",
            "item_id": "x",
            "parameters": {},
        })
        assert r.status_code == 400

    def test_missing_item_type(self, cas_env):
        c, _, _ = cas_env
        r = c.post("/execute", json={
            "project_path": "test-project",
            "item_id": "x",
        })
        assert r.status_code == 400


# ============================================================================
# Thread Enforcement
# ============================================================================


class TestExecuteThreadValidation:
    """Server-side thread enforcement on /execute endpoint."""

    def test_directive_inline_rejected(self, cas_env):
        """Directive + thread=inline → 400 (directives must fork on remote)."""
        c, root, tmp_path = cas_env

        r = c.post("/execute", json={
            "project_path": "test-project",
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

        r = c.post("/execute", json={
            "project_path": "test-project",
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
        from unittest.mock import patch

        ph, uh, snapshot_hash, mock_ref, mock_user_ref = _build_manifests_with_snapshot(tmp_path, root)

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref):
            r = c.post("/execute", json={
                "project_path": "test-project",
                "item_type": "tool",
                "item_id": "x",
                "parameters": {},
                "thread": "inline",
            })
        assert r.status_code == 200

    def test_thread_auto_derived_for_tool(self, cas_env):
        """Omitting thread field for tool → auto-derives 'inline'."""
        c, root, tmp_path = cas_env
        from unittest.mock import patch

        ph, uh, snapshot_hash, mock_ref, mock_user_ref = _build_manifests_with_snapshot(tmp_path, root)

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref):
            r = c.post("/execute", json={
                "project_path": "test-project",
                "item_type": "tool",
                "item_id": "x",
                "parameters": {},
            })
        assert r.status_code == 200


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

        user = Principal(fingerprint="test-user", capabilities=["rye.*"], owner="tester")
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
        app.dependency_overrides[get_current_principal] = lambda: Principal(
            fingerprint="test-user", capabilities=["rye.*"], owner="tester",
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
        ts.add_key(public_pem, owner="local", space="user", version="1.0.0")
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
        ts.add_key(public_pem, owner="local", space="user", version="1.0.0")
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


# ============================================================================
# Step D: Checkout-based Execution + Fold-Back
# ============================================================================


def _create_snapshot_chain(root, items=None, files=None, parent_hashes=None, source="push"):
    """Create a ProjectSnapshot backed by a real manifest in CAS.

    Returns (snapshot_hash, manifest_hash).
    """
    items = items or {}
    files = files or {}
    parent_hashes = parent_hashes or []

    manifest = SourceManifest(space="project", items=items, files=files)
    manifest_hash = cas.store_object(manifest.to_dict(), root)

    user_manifest = SourceManifest(space="user")
    user_manifest_hash = cas.store_object(user_manifest.to_dict(), root)

    snapshot = ProjectSnapshot(
        project_manifest_hash=manifest_hash,
        user_manifest_hash=user_manifest_hash,
        parent_hashes=parent_hashes,
        source=source,
    )
    snapshot_hash = cas.store_object(snapshot.to_dict(), root)
    return snapshot_hash, manifest_hash


class TestLoadManifestFromSnapshot:
    def test_loads_manifest(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()

        snapshot_hash, manifest_hash = _create_snapshot_chain(root)
        manifest = _load_manifest_from_snapshot(snapshot_hash, root)
        assert manifest["kind"] == "source_manifest"
        assert manifest["space"] == "project"

    def test_missing_snapshot_raises(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()

        with pytest.raises(FileNotFoundError, match="Snapshot"):
            _load_manifest_from_snapshot("0" * 64, root)

    def test_missing_manifest_raises(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()

        # Create a snapshot pointing to a non-existent manifest
        snapshot = ProjectSnapshot(
            project_manifest_hash="0" * 64,
            user_manifest_hash="0" * 64,
        )
        snapshot_hash = cas.store_object(snapshot.to_dict(), root)

        with pytest.raises(FileNotFoundError, match="Manifest"):
            _load_manifest_from_snapshot(snapshot_hash, root)


class TestTryAdvanceHead:
    def test_advances_on_matching_hash(self, tmp_path):
        cas_base = tmp_path / "cas"
        cas_base.mkdir()
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()
        settings = _make_settings(cas_base, signing_dir)
        user = Principal(fingerprint="test-user", capabilities=["rye.*"], owner="tester")

        # Set up initial ref
        init_project_ref(str(cas_base), user.fingerprint, "my-project", "old_hash")

        result = _try_advance_head(settings, user, "my-project", "new_hash", "old_hash")
        assert result is True

        ref = resolve_project_ref(str(cas_base), user.fingerprint, "my-project")
        assert ref["snapshot_hash"] == "new_hash"

    def test_fails_on_hash_mismatch(self, tmp_path):
        cas_base = tmp_path / "cas"
        cas_base.mkdir()
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()
        settings = _make_settings(cas_base, signing_dir)
        user = Principal(fingerprint="test-user", capabilities=["rye.*"], owner="tester")

        init_project_ref(str(cas_base), user.fingerprint, "my-project", "old_hash")

        result = _try_advance_head(settings, user, "my-project", "new_hash", "wrong_hash")
        assert result is False


class TestFoldBack:
    """Tests for _fold_back: fast-forward, three-way merge, conflicts, retry."""

    def _make_env(self, tmp_path):
        cas_base = tmp_path / "cas"
        cas_base.mkdir()
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()
        settings = _make_settings(cas_base, signing_dir)
        user = Principal(fingerprint="test-user", capabilities=["rye.*"], owner="tester")
        root = settings.user_cas_root(user.fingerprint)
        root.mkdir(parents=True)
        cache = settings.cache_root(user.fingerprint)
        cache.mkdir(parents=True)
        return settings, user, root, cache

    @pytest.mark.asyncio
    async def test_fast_forward(self, tmp_path):
        """HEAD unchanged → fast-forward."""
        settings, user, root, cache = self._make_env(tmp_path)

        base_hash, _ = _create_snapshot_chain(root)
        exec_hash, _ = _create_snapshot_chain(root, parent_hashes=[base_hash], source="execution")

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": base_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.advance_project_ref", return_value=True), \
             patch("ryeos_remote.server._update_snapshot_cache"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "fast-forward"
        assert result["snapshot_hash"] == exec_hash

    @pytest.mark.asyncio
    async def test_three_way_merge(self, tmp_path):
        """HEAD moved → three-way merge, no conflicts."""
        settings, user, root, cache = self._make_env(tmp_path)

        # Base: file_a.txt
        blob_a = cas.store_blob(b"content a", root)
        base_hash, _ = _create_snapshot_chain(root, files={"file_a.txt": blob_a})

        # HEAD moved: added file_b.txt (ours)
        blob_b = cas.store_blob(b"content b", root)
        head_hash, _ = _create_snapshot_chain(
            root, files={"file_a.txt": blob_a, "file_b.txt": blob_b},
            parent_hashes=[base_hash],
        )

        # Execution: added file_c.txt (theirs)
        blob_c = cas.store_blob(b"content c", root)
        exec_hash, _ = _create_snapshot_chain(
            root, files={"file_a.txt": blob_a, "file_c.txt": blob_c},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.advance_project_ref", return_value=True), \
             patch("ryeos_remote.server._update_snapshot_cache"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "merge"
        # Verify merged snapshot has both parents
        merged = cas.get_object(result["snapshot_hash"], root)
        assert merged["kind"] == "project_snapshot"
        assert merged["source"] == "merge"
        assert len(merged["parent_hashes"]) == 2
        assert merged["parent_hashes"][0] == head_hash
        assert merged["parent_hashes"][1] == exec_hash

        # Verify merged manifest contains all 3 files
        merged_manifest = cas.get_object(merged["project_manifest_hash"], root)
        assert "file_a.txt" in merged_manifest["files"]
        assert "file_b.txt" in merged_manifest["files"]
        assert "file_c.txt" in merged_manifest["files"]

    @pytest.mark.asyncio
    async def test_conflict_detected(self, tmp_path):
        """Both sides modify same file differently → conflict."""
        settings, user, root, cache = self._make_env(tmp_path)

        blob_base = cas.store_blob(b"base content", root)
        base_hash, _ = _create_snapshot_chain(root, files={"shared.txt": blob_base})

        blob_ours = cas.store_blob(b"ours content", root)
        head_hash, _ = _create_snapshot_chain(
            root, files={"shared.txt": blob_ours},
            parent_hashes=[base_hash],
        )

        blob_theirs = cas.store_blob(b"theirs content", root)
        exec_hash, _ = _create_snapshot_chain(
            root, files={"shared.txt": blob_theirs},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.store_conflict_record"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "conflict"
        assert result["snapshot_hash"] == head_hash  # HEAD unchanged
        assert result["unmerged_snapshot"] == exec_hash
        assert "shared.txt" in result["conflicts"]

    @pytest.mark.asyncio
    async def test_delete_modify_conflict(self, tmp_path):
        """One side deletes, other modifies → conflict."""
        settings, user, root, cache = self._make_env(tmp_path)

        blob_base = cas.store_blob(b"original", root)
        base_hash, _ = _create_snapshot_chain(root, files={"target.txt": blob_base})

        # HEAD: delete target.txt
        head_hash, _ = _create_snapshot_chain(
            root, files={}, parent_hashes=[base_hash],
        )

        # Execution: modify target.txt
        blob_mod = cas.store_blob(b"modified", root)
        exec_hash, _ = _create_snapshot_chain(
            root, files={"target.txt": blob_mod},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.store_conflict_record"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "conflict"
        assert "target.txt" in result["conflicts"]
        assert result["conflicts"]["target.txt"]["type"] in ("delete/modify", "modify/delete")

    @pytest.mark.asyncio
    async def test_add_add_conflict(self, tmp_path):
        """Both sides add same new file with different content → conflict."""
        settings, user, root, cache = self._make_env(tmp_path)

        base_hash, _ = _create_snapshot_chain(root)

        blob_ours = cas.store_blob(b"ours version", root)
        head_hash, _ = _create_snapshot_chain(
            root, files={"new_file.txt": blob_ours},
            parent_hashes=[base_hash],
        )

        blob_theirs = cas.store_blob(b"theirs version", root)
        exec_hash, _ = _create_snapshot_chain(
            root, files={"new_file.txt": blob_theirs},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.store_conflict_record"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "conflict"
        assert "new_file.txt" in result["conflicts"]
        assert result["conflicts"]["new_file.txt"]["type"] == "add/add"

    @pytest.mark.asyncio
    async def test_text_merge_non_overlapping(self, tmp_path):
        """Both sides modify same file but different lines → auto-resolved."""
        settings, user, root, cache = self._make_env(tmp_path)

        base_text = "line1\nline2\nline3\nline4\n"
        blob_base = cas.store_blob(base_text.encode(), root)
        base_hash, _ = _create_snapshot_chain(root, files={"doc.txt": blob_base})

        # HEAD modifies line1
        ours_text = "LINE1_MODIFIED\nline2\nline3\nline4\n"
        blob_ours = cas.store_blob(ours_text.encode(), root)
        head_hash, _ = _create_snapshot_chain(
            root, files={"doc.txt": blob_ours}, parent_hashes=[base_hash],
        )

        # Execution modifies line4
        theirs_text = "line1\nline2\nline3\nLINE4_MODIFIED\n"
        blob_theirs = cas.store_blob(theirs_text.encode(), root)
        exec_hash, _ = _create_snapshot_chain(
            root, files={"doc.txt": blob_theirs},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.advance_project_ref", return_value=True), \
             patch("ryeos_remote.server._update_snapshot_cache"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "merge"
        # Verify merged content has both changes
        merged = cas.get_object(result["snapshot_hash"], root)
        merged_manifest = cas.get_object(merged["project_manifest_hash"], root)
        merged_blob_hash = merged_manifest["files"]["doc.txt"]
        merged_content = cas.get_blob(merged_blob_hash, root).decode()
        assert "LINE1_MODIFIED" in merged_content
        assert "LINE4_MODIFIED" in merged_content

    @pytest.mark.asyncio
    async def test_text_merge_conflict_same_lines(self, tmp_path):
        """Both sides modify same lines → conflict (not auto-resolved)."""
        settings, user, root, cache = self._make_env(tmp_path)

        base_text = "line1\nline2\nline3\n"
        blob_base = cas.store_blob(base_text.encode(), root)
        base_hash, _ = _create_snapshot_chain(root, files={"doc.txt": blob_base})

        ours_text = "OURS_LINE1\nline2\nline3\n"
        blob_ours = cas.store_blob(ours_text.encode(), root)
        head_hash, _ = _create_snapshot_chain(
            root, files={"doc.txt": blob_ours}, parent_hashes=[base_hash],
        )

        theirs_text = "THEIRS_LINE1\nline2\nline3\n"
        blob_theirs = cas.store_blob(theirs_text.encode(), root)
        exec_hash, _ = _create_snapshot_chain(
            root, files={"doc.txt": blob_theirs},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.store_conflict_record"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "conflict"
        assert "doc.txt" in result["conflicts"]
        assert result["conflicts"]["doc.txt"]["type"] == "content"

    @pytest.mark.asyncio
    async def test_binary_conflict(self, tmp_path):
        """Non-UTF-8 blobs can't text-merge → conflict."""
        settings, user, root, cache = self._make_env(tmp_path)

        blob_base = cas.store_blob(b"\x00\x01\x02\x03", root)
        base_hash, _ = _create_snapshot_chain(root, files={"data.bin": blob_base})

        blob_ours = cas.store_blob(b"\x00\x01\xff\x03", root)
        head_hash, _ = _create_snapshot_chain(
            root, files={"data.bin": blob_ours}, parent_hashes=[base_hash],
        )

        blob_theirs = cas.store_blob(b"\x00\x01\x02\xfe", root)
        exec_hash, _ = _create_snapshot_chain(
            root, files={"data.bin": blob_theirs},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.store_conflict_record"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "conflict"
        assert "data.bin" in result["conflicts"]

    @pytest.mark.asyncio
    async def test_large_file_merge_skip(self, tmp_path):
        """Blobs > 1MB can't text-merge → conflict."""
        settings, user, root, cache = self._make_env(tmp_path)

        # 1.1 MB blobs
        base_data = b"A" * (1_100_000)
        blob_base = cas.store_blob(base_data, root)
        base_hash, _ = _create_snapshot_chain(root, files={"big.txt": blob_base})

        ours_data = b"B" * (1_100_000)
        blob_ours = cas.store_blob(ours_data, root)
        head_hash, _ = _create_snapshot_chain(
            root, files={"big.txt": blob_ours}, parent_hashes=[base_hash],
        )

        theirs_data = b"C" * (1_100_000)
        blob_theirs = cas.store_blob(theirs_data, root)
        exec_hash, _ = _create_snapshot_chain(
            root, files={"big.txt": blob_theirs},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.store_conflict_record"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "conflict"

    @pytest.mark.asyncio
    async def test_retry_exhaustion(self, tmp_path):
        """_try_advance_head always fails → retry_exhausted after MAX retries."""
        settings, user, root, cache = self._make_env(tmp_path)

        base_hash, _ = _create_snapshot_chain(root)
        exec_hash, _ = _create_snapshot_chain(root, parent_hashes=[base_hash], source="execution")

        from unittest.mock import patch

        # HEAD unchanged, but advance always fails (concurrent writer)
        mock_ref = {
            "snapshot_hash": base_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.advance_project_ref", return_value=False), \
             patch("asyncio.sleep", return_value=None):  # skip actual delays
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "retry_exhausted"
        assert result["unmerged_snapshot"] == exec_hash

    @pytest.mark.asyncio
    async def test_both_sides_same_change(self, tmp_path):
        """Both sides make identical change → auto-resolved (not a conflict)."""
        settings, user, root, cache = self._make_env(tmp_path)

        blob_base = cas.store_blob(b"original", root)
        base_hash, _ = _create_snapshot_chain(root, files={"shared.txt": blob_base})

        blob_same = cas.store_blob(b"same change", root)
        head_hash, _ = _create_snapshot_chain(
            root, files={"shared.txt": blob_same}, parent_hashes=[base_hash],
        )
        exec_hash, _ = _create_snapshot_chain(
            root, files={"shared.txt": blob_same},
            parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.advance_project_ref", return_value=True), \
             patch("ryeos_remote.server._update_snapshot_cache"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "merge"
        merged = cas.get_object(result["snapshot_hash"], root)
        merged_manifest = cas.get_object(merged["project_manifest_hash"], root)
        assert merged_manifest["files"]["shared.txt"] == blob_same

    @pytest.mark.asyncio
    async def test_both_sides_delete(self, tmp_path):
        """Both sides delete same file → auto-resolved (file removed)."""
        settings, user, root, cache = self._make_env(tmp_path)

        blob_base = cas.store_blob(b"to delete", root)
        base_hash, _ = _create_snapshot_chain(root, files={"gone.txt": blob_base})

        # Both sides remove gone.txt
        head_hash, _ = _create_snapshot_chain(
            root, files={}, parent_hashes=[base_hash],
        )
        exec_hash, _ = _create_snapshot_chain(
            root, files={}, parent_hashes=[base_hash], source="execution",
        )

        from unittest.mock import patch

        mock_ref = {
            "snapshot_hash": head_hash,
            "project_path": "test-project",
        }

        with patch("ryeos_remote.server.resolve_project_ref", return_value=mock_ref), \
             patch("ryeos_remote.server.advance_project_ref", return_value=True), \
             patch("ryeos_remote.server._update_snapshot_cache"):
            result = await _fold_back(
                user, settings, "test-project",
                base_hash, exec_hash, root, cache, "thread-1",
            )

        assert result["merge_type"] == "merge"
        merged = cas.get_object(result["snapshot_hash"], root)
        merged_manifest = cas.get_object(merged["project_manifest_hash"], root)
        assert "gone.txt" not in merged_manifest.get("files", {})


class TestExecuteFromHead:
    """Integration tests for /execute (always from HEAD)."""

    def test_noop_execution(self, cas_env):
        """Manifest unchanged after execution → no-op, skip fold-back."""
        c, root, tmp_path = cas_env

        from unittest.mock import patch

        ph, uh, snapshot_hash, mock_ref, mock_user_ref = _build_manifests_with_snapshot(tmp_path, root)

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref):
            r = c.post("/execute", json={
                "project_path": "test-project",
                "item_type": "tool",
                "item_id": "x",
                "parameters": {},
            })

        assert r.status_code == 200
        body = r.json()
        assert body["merge_type"] == "no-op"
        assert body["snapshot_hash"] == snapshot_hash

    def test_fast_forward(self, cas_env):
        """Execution modifies project → fast-forward fold-back."""
        c, root, tmp_path = cas_env

        from unittest.mock import patch

        ph, uh, snapshot_hash, mock_ref, mock_user_ref = _build_manifests_with_snapshot(tmp_path, root)

        async def mock_handle(self, item_type, item_id, project_path, parameters, thread):
            new_file = Path(project_path) / AI_DIR / "knowledge" / "new_knowledge.md"
            new_file.parent.mkdir(parents=True, exist_ok=True)
            new_file.write_text("# New Knowledge\n")
            return {"status": "success", "body": "done"}

        # Also need to patch fold-back's resolve + advance for the HEAD advance
        mock_fold_ref = {"snapshot_hash": snapshot_hash, "project_path": "test-project"}

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref), \
             patch("ryeos_remote.server.resolve_project_ref", return_value=mock_fold_ref), \
             patch("ryeos_remote.server.advance_project_ref", return_value=True), \
             patch("ryeos_remote.server._update_snapshot_cache"), \
             patch.object(
                 __import__("rye.actions.execute", fromlist=["ExecuteTool"]).ExecuteTool,
                 "handle", mock_handle,
             ):
            r = c.post("/execute", json={
                "project_path": "test-project",
                "item_type": "tool",
                "item_id": "x",
                "parameters": {},
            })

        assert r.status_code == 200
        body = r.json()
        assert body["merge_type"] == "fast-forward"
        assert body["snapshot_hash"] != snapshot_hash

    def test_missing_snapshot_rejected(self, cas_env):
        """Project ref with no snapshot_hash → 400."""
        c, root, tmp_path = cas_env

        from unittest.mock import patch

        mock_ref = {
            "project_manifest_hash": "a" * 64,
            "system_version": get_system_version(),
            "snapshot_hash": None,
        }
        mock_user_ref = {
            "user_manifest_hash": "b" * 64,
            "pushed_at": "2026-01-01T00:00:00+00:00",
        }

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref):
            r = c.post("/execute", json={
                "project_path": "test-project",
                "item_type": "tool",
                "item_id": "x",
                "parameters": {},
            })

        assert r.status_code == 400
        assert "snapshot" in r.json()["detail"].lower()


class TestConcurrentExecution:
    """Integration test: two concurrent executions against the same project.

    Verifies the core Phase 2.5 promise — concurrent modifications are
    merged via three-way merge with a merge commit (two parents).
    Uses real CAS operations; only filesystem state calls are mocked.
    """

    def _make_env(self, tmp_path):
        cas_base = tmp_path / "cas"
        cas_base.mkdir()
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()
        settings = _make_settings(cas_base, signing_dir)
        user = Principal(fingerprint="test-user", capabilities=["rye.*"], owner="tester")
        root = settings.user_cas_root(user.fingerprint)
        root.mkdir(parents=True)
        cache = settings.cache_root(user.fingerprint)
        cache.mkdir(parents=True)
        exec_dir = settings.exec_root(user.fingerprint)
        exec_dir.mkdir(parents=True)
        return settings, user, root, cache, exec_dir

    @pytest.mark.asyncio
    async def test_concurrent_executions_merge(self, tmp_path):
        """Two concurrent executions: first fast-forwards, second three-way merges."""
        import asyncio
        from unittest.mock import AsyncMock, MagicMock, patch
        from rye.cas.store import cas_root as get_cas_root

        settings, user, root, cache, exec_dir = self._make_env(tmp_path)

        # --- Set up base project with one file ---
        blob_base = cas.store_blob(b"base content\n", root)
        base_item = ItemSource(
            item_type="knowledge",
            item_id="base_doc",
            content_blob_hash=blob_base,
            integrity=f"sha256:{hashlib.sha256(b'base content\n').hexdigest()}",
        )
        base_item_hash = cas.store_object(base_item.to_dict(), root)

        base_manifest = SourceManifest(
            space="project",
            items={f"{AI_DIR}/knowledge/base_doc.md": base_item_hash},
            files={},
        )
        base_manifest_hash = cas.store_object(base_manifest.to_dict(), root)

        user_manifest = SourceManifest(space="user")
        user_manifest_hash = cas.store_object(user_manifest.to_dict(), root)

        base_snapshot = ProjectSnapshot(
            project_manifest_hash=base_manifest_hash,
            user_manifest_hash=user_manifest_hash,
            source="push",
        )
        base_snapshot_hash = cas.store_object(base_snapshot.to_dict(), root)

        # Pre-cache the snapshot so create_execution_space works
        from rye.cas.checkout import ensure_snapshot_cached
        ensure_snapshot_cached(base_snapshot_hash, root, cache)

        # --- Mutable state to simulate HEAD advancement ---
        # Tracks current HEAD — shared across concurrent calls.
        # The first execution to advance HEAD updates this; the second sees the new HEAD.
        head_state = {
            "snapshot_hash": base_snapshot_hash,
            "project_path": "test-project",
        }

        def mock_resolve_project_ref_or_404(settings, user, project_path):
            return dict(head_state)

        def mock_resolve_project_ref(cas_base, user_fp, project_path):
            return dict(head_state)

        def mock_advance_project_ref(cas_base, user_fp, project_path, new_hash, expected_hash):
            """Simulate optimistic CAS — succeed only if hash matches."""
            if head_state["snapshot_hash"] != expected_hash:
                return False
            head_state["snapshot_hash"] = new_hash
            return True

        mock_user_ref = {
            "user_manifest_hash": user_manifest_hash,
            "pushed_at": "2026-01-01T00:00:00+00:00",
        }

        # Barrier to synchronize both executions — both must complete their
        # execution phase before either attempts fold-back.
        barrier = asyncio.Barrier(2)

        exec_call_count = 0

        async def mock_handle(self_tool, item_type, item_id, project_path, parameters, thread):
            """Mock ExecuteTool.handle: write a unique knowledge file per call."""
            nonlocal exec_call_count
            exec_call_count += 1
            call_num = exec_call_count

            knowledge_dir = Path(project_path) / AI_DIR / "knowledge"
            knowledge_dir.mkdir(parents=True, exist_ok=True)

            if call_num == 1:
                (knowledge_dir / "exec_a.md").write_text("# Knowledge from execution A\n")
            else:
                (knowledge_dir / "exec_b.md").write_text("# Knowledge from execution B\n")

            # Wait for both executions to finish modifying before either proceeds
            await barrier.wait()
            return {"status": "success", "body": "done"}

        from ryeos_remote.server import _execute_from_head

        with patch("ryeos_remote.server._resolve_project_ref_or_404", side_effect=mock_resolve_project_ref_or_404), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref), \
             patch("ryeos_remote.server.resolve_project_ref", side_effect=mock_resolve_project_ref), \
             patch("ryeos_remote.server.advance_project_ref", side_effect=mock_advance_project_ref), \
             patch("ryeos_remote.server.register_execution"), \
             patch("ryeos_remote.server.complete_execution"), \
             patch("ryeos_remote.server._update_snapshot_cache"), \
             patch("ryeos_remote.server._check_user_quota"), \
             patch.object(
                 __import__("rye.actions.execute", fromlist=["ExecuteTool"]).ExecuteTool,
                 "handle", mock_handle,
             ):
            result_a, result_b = await asyncio.gather(
                _execute_from_head(
                    user, settings, "test-project",
                    item_type="tool", item_id="x",
                    parameters={}, thread="inline",
                ),
                _execute_from_head(
                    user, settings, "test-project",
                    item_type="tool", item_id="y",
                    parameters={}, thread="inline",
                ),
            )

        # --- Verify results ---
        # One should be fast-forward, the other should be merge
        merge_types = {result_a["merge_type"], result_b["merge_type"]}
        assert merge_types == {"fast-forward", "merge"}, (
            f"Expected one fast-forward and one merge, got: "
            f"{result_a['merge_type']}, {result_b['merge_type']}"
        )

        # Find which is which
        if result_a["merge_type"] == "merge":
            merge_result, ff_result = result_a, result_b
        else:
            merge_result, ff_result = result_b, result_a

        # The merge commit's snapshot should be the final HEAD
        final_snapshot_hash = head_state["snapshot_hash"]
        assert merge_result["snapshot_hash"] == final_snapshot_hash

        # Verify the merge snapshot has two parents (merge commit)
        final_snapshot = cas.get_object(final_snapshot_hash, root)
        assert final_snapshot is not None
        assert final_snapshot["kind"] == "project_snapshot"
        assert final_snapshot["source"] == "merge"
        assert len(final_snapshot["parent_hashes"]) == 2

        # Verify both changes present in merged manifest
        final_manifest = cas.get_object(final_snapshot["project_manifest_hash"], root)
        assert final_manifest is not None

        # All items should be in the manifest — base + exec_a + exec_b
        item_paths = set(final_manifest["items"].keys())
        assert f"{AI_DIR}/knowledge/base_doc.md" in item_paths
        assert f"{AI_DIR}/knowledge/exec_a.md" in item_paths
        assert f"{AI_DIR}/knowledge/exec_b.md" in item_paths

        # Verify exec_a and exec_b content blobs exist and have correct content
        exec_a_item = cas.get_object(final_manifest["items"][f"{AI_DIR}/knowledge/exec_a.md"], root)
        assert exec_a_item is not None
        exec_a_content = cas.get_blob(exec_a_item["content_blob_hash"], root)
        assert exec_a_content == b"# Knowledge from execution A\n"

        exec_b_item = cas.get_object(final_manifest["items"][f"{AI_DIR}/knowledge/exec_b.md"], root)
        assert exec_b_item is not None
        exec_b_content = cas.get_blob(exec_b_item["content_blob_hash"], root)
        assert exec_b_content == b"# Knowledge from execution B\n"

    @pytest.mark.asyncio
    async def test_concurrent_same_file_conflict(self, tmp_path):
        """Two concurrent executions modifying the same .ai/ file → one gets conflict."""
        import asyncio
        from unittest.mock import patch
        from rye.cas.checkout import ensure_snapshot_cached

        settings, user, root, cache, exec_dir = self._make_env(tmp_path)

        # Base project with a shared knowledge file
        base_content = b"# Shared Knowledge\noriginal content\n"
        blob_base = cas.store_blob(base_content, root)
        base_item = ItemSource(
            item_type="knowledge",
            item_id="shared",
            content_blob_hash=blob_base,
            integrity=f"sha256:{hashlib.sha256(base_content).hexdigest()}",
        )
        base_item_hash = cas.store_object(base_item.to_dict(), root)

        base_manifest = SourceManifest(
            space="project",
            items={f"{AI_DIR}/knowledge/shared.md": base_item_hash},
            files={},
        )
        base_manifest_hash = cas.store_object(base_manifest.to_dict(), root)

        user_manifest = SourceManifest(space="user")
        user_manifest_hash = cas.store_object(user_manifest.to_dict(), root)

        base_snapshot = ProjectSnapshot(
            project_manifest_hash=base_manifest_hash,
            user_manifest_hash=user_manifest_hash,
            source="push",
        )
        base_snapshot_hash = cas.store_object(base_snapshot.to_dict(), root)
        ensure_snapshot_cached(base_snapshot_hash, root, cache)

        head_state = {
            "snapshot_hash": base_snapshot_hash,
            "project_path": "test-project",
        }

        def mock_resolve_project_ref_or_404(settings, user, project_path):
            return dict(head_state)

        def mock_resolve_project_ref(cas_base, user_fp, project_path):
            return dict(head_state)

        def mock_advance_project_ref(cas_base, user_fp, project_path, new_hash, expected_hash):
            if head_state["snapshot_hash"] != expected_hash:
                return False
            head_state["snapshot_hash"] = new_hash
            return True

        mock_user_ref = {
            "user_manifest_hash": user_manifest_hash,
            "pushed_at": "2026-01-01T00:00:00+00:00",
        }

        barrier = asyncio.Barrier(2)
        exec_call_count = 0

        async def mock_handle(self_tool, item_type, item_id, project_path, parameters, thread):
            nonlocal exec_call_count
            exec_call_count += 1
            call_num = exec_call_count

            # Both executions rewrite the SAME line differently → content conflict
            shared = Path(project_path) / AI_DIR / "knowledge" / "shared.md"
            if call_num == 1:
                shared.write_text("# Shared Knowledge\nEXECUTION A CONTENT\n")
            else:
                shared.write_text("# Shared Knowledge\nEXECUTION B CONTENT\n")

            await barrier.wait()
            return {"status": "success", "body": "done"}

        from ryeos_remote.server import _execute_from_head

        with patch("ryeos_remote.server._resolve_project_ref_or_404", side_effect=mock_resolve_project_ref_or_404), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref), \
             patch("ryeos_remote.server.resolve_project_ref", side_effect=mock_resolve_project_ref), \
             patch("ryeos_remote.server.advance_project_ref", side_effect=mock_advance_project_ref), \
             patch("ryeos_remote.server.register_execution"), \
             patch("ryeos_remote.server.complete_execution"), \
             patch("ryeos_remote.server._update_snapshot_cache"), \
             patch("ryeos_remote.server._check_user_quota"), \
             patch("ryeos_remote.server.store_conflict_record"), \
             patch.object(
                 __import__("rye.actions.execute", fromlist=["ExecuteTool"]).ExecuteTool,
                 "handle", mock_handle,
             ):
            result_a, result_b = await asyncio.gather(
                _execute_from_head(
                    user, settings, "test-project",
                    item_type="tool", item_id="x",
                    parameters={}, thread="inline",
                ),
                _execute_from_head(
                    user, settings, "test-project",
                    item_type="tool", item_id="y",
                    parameters={}, thread="inline",
                ),
            )

        merge_types = {result_a["merge_type"], result_b["merge_type"]}
        # One fast-forwards, the other hits a content conflict (same line modified differently)
        assert "fast-forward" in merge_types
        assert "conflict" in merge_types

        if result_a["merge_type"] == "conflict":
            conflict_result = result_a
        else:
            conflict_result = result_b

        # The conflict should be on the shared knowledge file's item path
        conflict_paths = set(conflict_result["conflicts"].keys())
        shared_path = f"{AI_DIR}/knowledge/shared.md"
        assert shared_path in conflict_paths
        assert conflict_result["conflicts"][shared_path]["type"] == "content"

    @pytest.mark.asyncio
    async def test_concurrent_non_ai_files_merge(self, tmp_path):
        """Two concurrent executions adding different non-.ai files → merged via files dict.

        Requires sync config with include patterns so build_manifest picks up
        non-.ai files.
        """
        import asyncio
        from unittest.mock import patch
        from rye.cas.checkout import ensure_snapshot_cached

        settings, user, root, cache, exec_dir = self._make_env(tmp_path)

        # Base project: one .ai item + sync config to include *.txt
        sync_yaml = "sync:\n  include:\n    - '*.txt'\n"
        sync_blob = cas.store_blob(sync_yaml.encode(), root)
        sync_item = ItemSource(
            item_type="config",
            item_id="cas/remote",
            content_blob_hash=sync_blob,
            integrity=f"sha256:{hashlib.sha256(sync_yaml.encode()).hexdigest()}",
        )
        sync_item_hash = cas.store_object(sync_item.to_dict(), root)

        base_content = b"# Base\n"
        base_blob = cas.store_blob(base_content, root)
        base_item = ItemSource(
            item_type="knowledge",
            item_id="base",
            content_blob_hash=base_blob,
            integrity=f"sha256:{hashlib.sha256(base_content).hexdigest()}",
        )
        base_item_hash = cas.store_object(base_item.to_dict(), root)

        base_file_blob = cas.store_blob(b"base file\n", root)

        base_manifest = SourceManifest(
            space="project",
            items={
                f"{AI_DIR}/knowledge/base.md": base_item_hash,
                f"{AI_DIR}/config/cas/remote.yaml": sync_item_hash,
            },
            files={"readme.txt": base_file_blob},
        )
        base_manifest_hash = cas.store_object(base_manifest.to_dict(), root)

        user_manifest = SourceManifest(space="user")
        user_manifest_hash = cas.store_object(user_manifest.to_dict(), root)

        base_snapshot = ProjectSnapshot(
            project_manifest_hash=base_manifest_hash,
            user_manifest_hash=user_manifest_hash,
            source="push",
        )
        base_snapshot_hash = cas.store_object(base_snapshot.to_dict(), root)
        ensure_snapshot_cached(base_snapshot_hash, root, cache)

        head_state = {
            "snapshot_hash": base_snapshot_hash,
            "project_path": "test-project",
        }

        def mock_resolve_project_ref_or_404(settings, user, project_path):
            return dict(head_state)

        def mock_resolve_project_ref(cas_base, user_fp, project_path):
            return dict(head_state)

        def mock_advance_project_ref(cas_base, user_fp, project_path, new_hash, expected_hash):
            if head_state["snapshot_hash"] != expected_hash:
                return False
            head_state["snapshot_hash"] = new_hash
            return True

        mock_user_ref = {
            "user_manifest_hash": user_manifest_hash,
            "pushed_at": "2026-01-01T00:00:00+00:00",
        }

        barrier = asyncio.Barrier(2)
        exec_call_count = 0

        async def mock_handle(self_tool, item_type, item_id, project_path, parameters, thread):
            nonlocal exec_call_count
            exec_call_count += 1
            call_num = exec_call_count

            pp = Path(project_path)
            if call_num == 1:
                (pp / "alpha.txt").write_text("alpha output\n")
            else:
                (pp / "beta.txt").write_text("beta output\n")

            await barrier.wait()
            return {"status": "success", "body": "done"}

        from ryeos_remote.server import _execute_from_head

        with patch("ryeos_remote.server._resolve_project_ref_or_404", side_effect=mock_resolve_project_ref_or_404), \
             patch("ryeos_remote.server.resolve_user_space_ref", return_value=mock_user_ref), \
             patch("ryeos_remote.server.resolve_project_ref", side_effect=mock_resolve_project_ref), \
             patch("ryeos_remote.server.advance_project_ref", side_effect=mock_advance_project_ref), \
             patch("ryeos_remote.server.register_execution"), \
             patch("ryeos_remote.server.complete_execution"), \
             patch("ryeos_remote.server._update_snapshot_cache"), \
             patch("ryeos_remote.server._check_user_quota"), \
             patch.object(
                 __import__("rye.actions.execute", fromlist=["ExecuteTool"]).ExecuteTool,
                 "handle", mock_handle,
             ):
            result_a, result_b = await asyncio.gather(
                _execute_from_head(
                    user, settings, "test-project",
                    item_type="tool", item_id="x",
                    parameters={}, thread="inline",
                ),
                _execute_from_head(
                    user, settings, "test-project",
                    item_type="tool", item_id="y",
                    parameters={}, thread="inline",
                ),
            )

        merge_types = {result_a["merge_type"], result_b["merge_type"]}
        assert merge_types == {"fast-forward", "merge"}, (
            f"Expected one fast-forward and one merge, got: "
            f"{result_a['merge_type']}, {result_b['merge_type']}"
        )

        if result_a["merge_type"] == "merge":
            merge_result = result_a
        else:
            merge_result = result_b

        final_snapshot = cas.get_object(merge_result["snapshot_hash"], root)
        assert final_snapshot["source"] == "merge"
        assert len(final_snapshot["parent_hashes"]) == 2

        final_manifest = cas.get_object(final_snapshot["project_manifest_hash"], root)
        all_files = set(final_manifest.get("files", {}).keys())

        # Both new .txt files should be in the merged files dict
        assert "alpha.txt" in all_files, f"alpha.txt not in files: {all_files}"
        assert "beta.txt" in all_files, f"beta.txt not in files: {all_files}"
        assert "readme.txt" in all_files, f"base readme.txt not in files: {all_files}"

        # Verify content
        alpha_blob = cas.get_blob(final_manifest["files"]["alpha.txt"], root)
        assert alpha_blob == b"alpha output\n"
        beta_blob = cas.get_blob(final_manifest["files"]["beta.txt"], root)
        assert beta_blob == b"beta output\n"


class TestStoreConflictRecord:
    def test_stores_conflict(self, tmp_path):
        cas_base = tmp_path / "cas"
        cas_base.mkdir()
        user_fp = "test-user"
        exec_dir = cas_base / user_fp / "executions" / "by-id"
        exec_dir.mkdir(parents=True)

        store_conflict_record(
            str(cas_base), user_fp,
            thread_id="thread-1",
            conflicts={"file.txt": {"type": "content"}},
            unmerged_snapshot="snap123",
        )

        record_file = exec_dir / "thread-1"
        assert record_file.exists()
        import json
        record = json.loads(record_file.read_text())
        assert record["merge_conflicts"] == {"file.txt": {"type": "content"}}
        assert record["unmerged_snapshot_hash"] == "snap123"

    def test_handles_error_gracefully(self, tmp_path):
        cas_base = tmp_path / "cas"
        # Don't create dirs — should trigger an error path but not raise
        store_conflict_record(
            str(cas_base), "test-user",
            thread_id="thread-1",
            conflicts={},
            unmerged_snapshot="snap123",
        )
        # Should not raise — logs warning instead


class TestUpdateSnapshotCache:
    def test_caches_snapshot(self, tmp_path):
        cas_base = tmp_path / "cas"
        cas_base.mkdir()
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()
        settings = _make_settings(cas_base, signing_dir)
        user = Principal(fingerprint="test-user", capabilities=["rye.*"], owner="tester")
        root = settings.user_cas_root(user.fingerprint)
        root.mkdir(parents=True)
        cache = settings.cache_root(user.fingerprint)
        cache.mkdir(parents=True)

        snapshot_hash, _ = _create_snapshot_chain(root)

        _update_snapshot_cache(
            settings, user, "project",
            snapshot_hash, root, cache,
        )

        cached = cache / "snapshots" / snapshot_hash
        assert cached.exists()

    def test_handles_error_gracefully(self, tmp_path):
        cas_base = tmp_path / "cas"
        cas_base.mkdir()
        signing_dir = tmp_path / "signing"
        signing_dir.mkdir()
        settings = _make_settings(cas_base, signing_dir)
        user = Principal(fingerprint="test-user", capabilities=["rye.*"], owner="tester")
        root = settings.user_cas_root(user.fingerprint)
        root.mkdir(parents=True)
        cache = settings.cache_root(user.fingerprint)
        cache.mkdir(parents=True)

        # Non-existent snapshot → ensure_snapshot_cached will raise
        _update_snapshot_cache(
            settings, user, "project",
            "0" * 64, root, cache,
        )
        # Should not raise — logs warning instead


class TestSettingsCacheExecRoots:
    def test_cache_root(self):
        s = _make_settings("/cas", "/signing")
        assert s.cache_root("user-1") == Path("/cas/user-1/cache")

    def test_exec_root(self):
        s = _make_settings("/cas", "/signing")
        assert s.exec_root("user-1") == Path("/cas/user-1/executions")


# ============================================================================
# HMAC Webhook Verification
# ============================================================================


class TestVerifyTimestamp:
    def test_valid_timestamp(self):
        from ryeos_remote.auth import verify_timestamp
        verify_timestamp(str(int(time.time())))  # should not raise

    def test_empty_timestamp(self):
        from ryeos_remote.auth import verify_timestamp
        with pytest.raises(HTTPException) as exc_info:
            verify_timestamp("")
        assert exc_info.value.status_code == 401

    def test_non_numeric_timestamp(self):
        from ryeos_remote.auth import verify_timestamp
        with pytest.raises(HTTPException) as exc_info:
            verify_timestamp("not-a-number")
        assert exc_info.value.status_code == 401

    def test_stale_timestamp(self):
        from ryeos_remote.auth import verify_timestamp
        stale = str(int(time.time()) - 600)
        with pytest.raises(HTTPException) as exc_info:
            verify_timestamp(stale)
        assert exc_info.value.status_code == 401

    def test_future_timestamp(self):
        from ryeos_remote.auth import verify_timestamp
        future = str(int(time.time()) + 60)
        with pytest.raises(HTTPException) as exc_info:
            verify_timestamp(future)
        assert exc_info.value.status_code == 401


class TestVerifyHmac:
    def _sign(self, timestamp, body, secret):
        import hmac as _hmac
        signed = timestamp.encode() + b"." + body
        sig = _hmac.new(secret.encode(), signed, hashlib.sha256).hexdigest()
        return f"sha256={sig}"

    def test_valid_hmac(self):
        from ryeos_remote.auth import verify_hmac
        ts = str(int(time.time()))
        body = b'{"hook_id": "wh_test"}'
        secret = "whsec_test123"
        sig = self._sign(ts, body, secret)
        verify_hmac(ts, body, secret, sig)  # should not raise

    def test_wrong_secret(self):
        from ryeos_remote.auth import verify_hmac
        ts = str(int(time.time()))
        body = b'{"hook_id": "wh_test"}'
        sig = self._sign(ts, body, "correct_secret")
        with pytest.raises(HTTPException) as exc_info:
            verify_hmac(ts, body, "wrong_secret", sig)
        assert exc_info.value.status_code == 401

    def test_tampered_body(self):
        from ryeos_remote.auth import verify_hmac
        ts = str(int(time.time()))
        secret = "whsec_test123"
        sig = self._sign(ts, b'{"hook_id": "wh_test"}', secret)
        with pytest.raises(HTTPException) as exc_info:
            verify_hmac(ts, b'{"hook_id": "wh_tampered"}', secret, sig)
        assert exc_info.value.status_code == 401

    def test_missing_signature(self):
        from ryeos_remote.auth import verify_hmac
        with pytest.raises(HTTPException) as exc_info:
            verify_hmac("123", b"body", "secret", "")
        assert exc_info.value.status_code == 401

    def test_wrong_prefix(self):
        from ryeos_remote.auth import verify_hmac
        with pytest.raises(HTTPException) as exc_info:
            verify_hmac("123", b"body", "secret", "md5=abcd")
        assert exc_info.value.status_code == 401

    def test_wrong_length(self):
        from ryeos_remote.auth import verify_hmac
        with pytest.raises(HTTPException) as exc_info:
            verify_hmac("123", b"body", "secret", "sha256=tooshort")
        assert exc_info.value.status_code == 401


# ============================================================================
# Webhook Binding Lookup
# ============================================================================


class TestLookupBinding:
    def test_active_binding(self):
        from unittest.mock import patch
        settings = _make_settings("/cas", "/signing")
        binding = {
            "hook_id": "wh_test",
            "user_id": "user-1",
            "remote_name": "default",
            "item_type": "directive",
            "item_id": "email/handle_inbound",
            "project_path": "test-project",
            "hmac_secret": "whsec_secret",
            "revoked_at": None,
        }

        with patch("ryeos_remote.server.resolve_binding", return_value=binding):
            result = _lookup_binding("wh_test", settings)
        assert result["item_id"] == "email/handle_inbound"

    def test_missing_binding(self):
        from unittest.mock import patch
        settings = _make_settings("/cas", "/signing")

        with patch("ryeos_remote.server.resolve_binding", return_value=None):
            with pytest.raises(HTTPException) as exc_info:
                _lookup_binding("wh_nonexistent", settings)
        assert exc_info.value.status_code == 401
        assert "Invalid webhook auth" in exc_info.value.detail

    def test_revoked_binding(self):
        from unittest.mock import patch
        settings = _make_settings("/cas", "/signing")

        # resolve_binding returns None for revoked bindings
        with patch("ryeos_remote.server.resolve_binding", return_value=None):
            with pytest.raises(HTTPException) as exc_info:
                _lookup_binding("wh_revoked", settings)
        assert exc_info.value.status_code == 401


class TestResolvePrincipalFromBinding:
    def test_valid_principal(self):
        binding = {"user_id": "fp-abc123", "capabilities": ["rye.execute.*"], "owner": "leo"}
        principal = _resolve_principal_from_binding(binding)
        assert principal.fingerprint == "fp-abc123"
        assert principal.owner == "leo"
        assert principal.capabilities == ["rye.execute.*"]

    def test_default_capabilities(self):
        binding = {"user_id": "fp-abc123"}
        principal = _resolve_principal_from_binding(binding)
        assert principal.fingerprint == "fp-abc123"
        assert principal.capabilities == ["rye.execute.*"]


# ============================================================================
# Webhook Binding Management Endpoints
# ============================================================================


class TestWebhookBindings:
    def test_create_binding(self, cas_env):
        c, root, tmp_path = cas_env
        from unittest.mock import patch

        mock_result = {
            "hook_id": "wh_" + "a" * 32,
            "hmac_secret": "whsec_" + "b" * 64,
            "item_type": "directive",
            "item_id": "email/handle_inbound",
            "project_path": "campaign-kiwi",
        }

        with patch("ryeos_remote.server.create_binding", return_value=mock_result):
            r = c.post("/webhook-bindings", json={
                "item_type": "directive",
                "item_id": "email/handle_inbound",
                "project_path": "campaign-kiwi",
                "description": "Inbound email processing",
            })
        assert r.status_code == 200
        body = r.json()
        assert body["hook_id"].startswith("wh_")
        assert body["hmac_secret"].startswith("whsec_")
        assert body["item_type"] == "directive"
        assert body["item_id"] == "email/handle_inbound"
        assert body["project_path"] == "campaign-kiwi"

    def test_create_binding_invalid_item_type(self, cas_env):
        c, root, tmp_path = cas_env
        r = c.post("/webhook-bindings", json={
            "item_type": "knowledge",
            "item_id": "x",
            "project_path": "test",
        })
        assert r.status_code == 400

    def test_list_bindings(self, cas_env):
        c, root, tmp_path = cas_env
        from unittest.mock import patch

        bindings = [
            {"hook_id": "wh_abc", "item_type": "directive", "item_id": "email/send",
             "project_path": "proj", "description": "test", "created_at": "2026-01-01", "revoked_at": None},
        ]

        with patch("ryeos_remote.server.list_bindings", return_value=bindings):
            r = c.get("/webhook-bindings")
        assert r.status_code == 200
        body = r.json()
        assert len(body["bindings"]) == 1
        assert body["bindings"][0]["hook_id"] == "wh_abc"

    def test_revoke_binding(self, cas_env):
        c, root, tmp_path = cas_env
        from unittest.mock import patch

        with patch("ryeos_remote.server.revoke_binding", return_value=True):
            r = c.delete("/webhook-bindings/wh_abc")
        assert r.status_code == 200
        assert r.json()["revoked"] == "wh_abc"

    def test_revoke_nonexistent(self, cas_env):
        c, root, tmp_path = cas_env
        from unittest.mock import patch

        with patch("ryeos_remote.server.revoke_binding", return_value=False):
            r = c.delete("/webhook-bindings/wh_nonexistent")
        assert r.status_code == 404


# ============================================================================
# Push Deep Validation (_validate_manifest_graph)
# ============================================================================


class TestValidateManifestGraph:
    def _make_valid_manifest(self, root, space="project"):
        """Create a valid manifest with one item and one file in CAS."""
        # Store a content blob
        content = b"print('hello')\n"
        blob_hash = cas.store_blob(content, root)

        # Store an item_source object pointing to the blob
        item_source = ItemSource(
            item_type="tool",
            item_id="test/tool",
            content_blob_hash=blob_hash,
            integrity=f"sha256:{hashlib.sha256(content).hexdigest()}",
        )
        item_hash = cas.store_object(item_source.to_dict(), root)

        # Store a file blob
        file_content = b"some data"
        file_blob_hash = cas.store_blob(file_content, root)

        # Store the manifest
        manifest = SourceManifest(
            space=space,
            items={".ai/tools/test/tool.py": item_hash},
            files={"data.txt": file_blob_hash},
        )
        manifest_hash = cas.store_object(manifest.to_dict(), root)
        return manifest_hash, item_hash, blob_hash, file_blob_hash

    def test_valid_manifest(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        manifest_hash, _, _, _ = self._make_valid_manifest(root)
        result = _validate_manifest_graph(
            manifest_hash, root, expected_space="project", label="test",
        )
        assert result["kind"] == "source_manifest"
        assert result["space"] == "project"

    def test_missing_manifest_object(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        with pytest.raises(HTTPException) as exc_info:
            _validate_manifest_graph(
                "deadbeef" * 8, root, expected_space="project", label="test",
            )
        assert exc_info.value.status_code == 400
        assert "not found in CAS" in exc_info.value.detail

    def test_wrong_kind(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        # Store an object that is not a source_manifest
        obj = {"kind": "execution_snapshot", "schema": 1}
        obj_hash = cas.store_object(obj, root)
        with pytest.raises(HTTPException) as exc_info:
            _validate_manifest_graph(
                obj_hash, root, expected_space="project", label="test",
            )
        assert exc_info.value.status_code == 400
        assert "expected 'source_manifest'" in exc_info.value.detail

    def test_wrong_schema_version(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        obj = {"kind": "source_manifest", "schema": 999, "space": "project", "items": {}, "files": {}}
        obj_hash = cas.store_object(obj, root)
        with pytest.raises(HTTPException) as exc_info:
            _validate_manifest_graph(
                obj_hash, root, expected_space="project", label="test",
            )
        assert exc_info.value.status_code == 400
        assert "unsupported schema version" in exc_info.value.detail

    def test_wrong_space(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        manifest_hash, _, _, _ = self._make_valid_manifest(root, space="user")
        with pytest.raises(HTTPException) as exc_info:
            _validate_manifest_graph(
                manifest_hash, root, expected_space="project", label="test",
            )
        assert exc_info.value.status_code == 400
        assert "expected 'project'" in exc_info.value.detail

    def test_missing_item_object(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        manifest = SourceManifest(
            space="project",
            items={".ai/tools/missing.py": "deadbeef" * 8},
            files={},
        )
        manifest_hash = cas.store_object(manifest.to_dict(), root)
        with pytest.raises(HTTPException) as exc_info:
            _validate_manifest_graph(
                manifest_hash, root, expected_space="project", label="test",
            )
        assert exc_info.value.status_code == 400
        assert "missing object" in exc_info.value.detail

    def test_item_wrong_kind(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        # Store a non-item_source object in the item slot
        wrong_obj = {"kind": "config_snapshot", "schema": 1, "resolved_config": {}}
        wrong_hash = cas.store_object(wrong_obj, root)
        manifest = SourceManifest(
            space="project",
            items={".ai/tools/bad.py": wrong_hash},
            files={},
        )
        manifest_hash = cas.store_object(manifest.to_dict(), root)
        with pytest.raises(HTTPException) as exc_info:
            _validate_manifest_graph(
                manifest_hash, root, expected_space="project", label="test",
            )
        assert exc_info.value.status_code == 400
        assert "expected 'item_source'" in exc_info.value.detail

    def test_item_missing_content_blob(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        # item_source pointing to a blob that doesn't exist
        item_source = ItemSource(
            item_type="tool",
            item_id="test/tool",
            content_blob_hash="deadbeef" * 8,
            integrity="sha256:fake",
        )
        item_hash = cas.store_object(item_source.to_dict(), root)
        manifest = SourceManifest(
            space="project",
            items={".ai/tools/test.py": item_hash},
            files={},
        )
        manifest_hash = cas.store_object(manifest.to_dict(), root)
        with pytest.raises(HTTPException) as exc_info:
            _validate_manifest_graph(
                manifest_hash, root, expected_space="project", label="test",
            )
        assert exc_info.value.status_code == 400
        assert "missing blob" in exc_info.value.detail

    def test_missing_file_blob(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        manifest = SourceManifest(
            space="project",
            items={},
            files={"data.txt": "deadbeef" * 8},
        )
        manifest_hash = cas.store_object(manifest.to_dict(), root)
        with pytest.raises(HTTPException) as exc_info:
            _validate_manifest_graph(
                manifest_hash, root, expected_space="project", label="test",
            )
        assert exc_info.value.status_code == 400
        assert "missing blob" in exc_info.value.detail

    def test_user_space_validation(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        manifest_hash, _, _, _ = self._make_valid_manifest(root, space="user")
        result = _validate_manifest_graph(
            manifest_hash, root, expected_space="user", label="user_manifest",
        )
        assert result["space"] == "user"

    def test_empty_manifest_valid(self, tmp_path):
        root = tmp_path / "cas"
        root.mkdir()
        manifest = SourceManifest(space="project", items={}, files={})
        manifest_hash = cas.store_object(manifest.to_dict(), root)
        result = _validate_manifest_graph(
            manifest_hash, root, expected_space="project", label="test",
        )
        assert result["items"] == {}
        assert result["files"] == {}

    def test_deduped_items_not_double_checked(self, tmp_path):
        """Two items sharing the same object_hash should only validate once."""
        root = tmp_path / "cas"
        root.mkdir()
        content = b"shared content"
        blob_hash = cas.store_blob(content, root)
        item_source = ItemSource(
            item_type="tool",
            item_id="shared",
            content_blob_hash=blob_hash,
            integrity=f"sha256:{hashlib.sha256(content).hexdigest()}",
        )
        item_hash = cas.store_object(item_source.to_dict(), root)
        manifest = SourceManifest(
            space="project",
            items={
                ".ai/tools/a.py": item_hash,
                ".ai/tools/b.py": item_hash,
            },
            files={},
        )
        manifest_hash = cas.store_object(manifest.to_dict(), root)
        result = _validate_manifest_graph(
            manifest_hash, root, expected_space="project", label="test",
        )
        assert len(result["items"]) == 2


# ============================================================================
# History — get_history() helper
# ============================================================================


class TestGetHistory:
    def test_linear_chain(self, tmp_path):
        """Walk a 3-deep linear chain, returns all snapshots in order."""
        root = tmp_path / "cas"
        root.mkdir()

        s0 = ProjectSnapshot(
            project_manifest_hash="m0", source="push", timestamp="t0",
        )
        h0 = cas.store_object(s0.to_dict(), root)

        s1 = ProjectSnapshot(
            project_manifest_hash="m1", parent_hashes=[h0],
            source="push", timestamp="t1",
        )
        h1 = cas.store_object(s1.to_dict(), root)

        s2 = ProjectSnapshot(
            project_manifest_hash="m2", parent_hashes=[h1],
            source="execution", timestamp="t2",
        )
        h2 = cas.store_object(s2.to_dict(), root)

        history = get_history(h2, root, limit=50)
        assert len(history) == 3
        assert history[0]["_hash"] == h2
        assert history[1]["_hash"] == h1
        assert history[2]["_hash"] == h0
        assert history[0]["project_manifest_hash"] == "m2"
        assert history[2]["parent_hashes"] == []

    def test_merge_commit_follows_first_parent(self, tmp_path):
        """Merge snapshot with two parents — follows parent_hashes[0] only."""
        root = tmp_path / "cas"
        root.mkdir()

        s_base = ProjectSnapshot(
            project_manifest_hash="base", source="push", timestamp="t0",
        )
        h_base = cas.store_object(s_base.to_dict(), root)

        s_branch = ProjectSnapshot(
            project_manifest_hash="branch", parent_hashes=[h_base],
            source="execution", timestamp="t1",
        )
        h_branch = cas.store_object(s_branch.to_dict(), root)

        s_main = ProjectSnapshot(
            project_manifest_hash="main", parent_hashes=[h_base],
            source="push", timestamp="t2",
        )
        h_main = cas.store_object(s_main.to_dict(), root)

        s_merge = ProjectSnapshot(
            project_manifest_hash="merged", parent_hashes=[h_main, h_branch],
            source="merge", timestamp="t3",
        )
        h_merge = cas.store_object(s_merge.to_dict(), root)

        history = get_history(h_merge, root, limit=50)
        assert len(history) == 3
        assert [h["_hash"] for h in history] == [h_merge, h_main, h_base]

    def test_missing_snapshot_stops(self, tmp_path):
        """Chain with missing intermediate object stops at the gap."""
        root = tmp_path / "cas"
        root.mkdir()

        s1 = ProjectSnapshot(
            project_manifest_hash="m1",
            parent_hashes=["nonexistent_hash"],
            source="push", timestamp="t1",
        )
        h1 = cas.store_object(s1.to_dict(), root)

        history = get_history(h1, root)
        assert len(history) == 1
        assert history[0]["_hash"] == h1

    def test_limit_respected(self, tmp_path):
        """Only returns up to limit snapshots."""
        root = tmp_path / "cas"
        root.mkdir()

        prev = None
        for i in range(10):
            s = ProjectSnapshot(
                project_manifest_hash=f"m{i}",
                parent_hashes=[prev] if prev else [],
                source="push", timestamp=f"t{i}",
            )
            prev = cas.store_object(s.to_dict(), root)

        history = get_history(prev, root, limit=3)
        assert len(history) == 3

    def test_nonexistent_hash(self, tmp_path):
        """Starting from a non-existent hash returns empty list."""
        root = tmp_path / "cas"
        root.mkdir()
        history = get_history("does_not_exist", root)
        assert history == []


# ============================================================================
# History — GET /history endpoint
# ============================================================================


class TestHistoryEndpoint:
    def test_happy_path(self, cas_env):
        """GET /history returns snapshot chain for a project."""
        c, user_cas, tmp_path = cas_env
        from unittest.mock import MagicMock, patch

        # Build a 2-snapshot chain in CAS
        s0 = ProjectSnapshot(
            project_manifest_hash="m0", source="push", timestamp="t0",
        )
        h0 = cas.store_object(s0.to_dict(), user_cas)
        s1 = ProjectSnapshot(
            project_manifest_hash="m1", parent_hashes=[h0],
            source="push", timestamp="t1",
        )
        h1 = cas.store_object(s1.to_dict(), user_cas)

        mock_ref = {
            "snapshot_hash": h1,
            "project_path": "my-project",
        }

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref):
            r = c.get("/history", params={"project_path": "my-project"})

        assert r.status_code == 200
        body = r.json()
        assert body["project_path"] == "my-project"
        assert body["head"] == h1
        assert len(body["snapshots"]) == 2
        assert body["snapshots"][0]["_hash"] == h1
        assert body["snapshots"][1]["_hash"] == h0

    def test_no_project_returns_404(self, cas_env):
        """GET /history for non-existent project returns 404."""
        c, _, _ = cas_env
        from unittest.mock import patch
        from fastapi import HTTPException

        def raise_404(*args, **kwargs):
            raise HTTPException(404, "No project ref found")

        with patch("ryeos_remote.server._resolve_project_ref_or_404", side_effect=raise_404):
            r = c.get("/history", params={"project_path": "nonexistent"})

        assert r.status_code == 404

    def test_limit_param(self, cas_env):
        """GET /history respects limit query param."""
        c, user_cas, tmp_path = cas_env
        from unittest.mock import patch

        # Build 5-snapshot chain
        prev = None
        for i in range(5):
            s = ProjectSnapshot(
                project_manifest_hash=f"m{i}",
                parent_hashes=[prev] if prev else [],
                source="push", timestamp=f"t{i}",
            )
            prev = cas.store_object(s.to_dict(), user_cas)

        mock_ref = {
            "snapshot_hash": prev,
            "project_path": "proj",
        }

        with patch("ryeos_remote.server._resolve_project_ref_or_404", return_value=mock_ref):
            r = c.get("/history", params={"project_path": "proj", "limit": 2})

        assert r.status_code == 200
        assert len(r.json()["snapshots"]) == 2

    def test_missing_project_path_param(self, cas_env):
        """GET /history without project_path returns 422."""
        c, _, _ = cas_env
        r = c.get("/history")
        assert r.status_code == 422
