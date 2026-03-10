"""Tests for sync protocol handlers."""

import base64
import hashlib
import json

import pytest

from lillux.primitives import cas
from lillux.primitives.integrity import canonical_json, compute_integrity
from rye.cas.sync import (
    ObjectEntry,
    handle_has_objects,
    handle_put_objects,
    handle_get_objects,
    collect_object_hashes,
    export_objects,
    import_objects,
)
from rye.cas.manifest import build_manifest
from rye.cas.store import cas_root
from rye.constants import AI_DIR


class TestHandleHasObjects:
    def test_partitions_present_and_missing(self, tmp_path):
        h1 = cas.store_blob(b"exists", tmp_path)
        h2 = "0" * 64
        result = handle_has_objects([h1, h2], tmp_path)
        assert h1 in result["present"]
        assert h2 in result["missing"]

    def test_empty_list(self, tmp_path):
        result = handle_has_objects([], tmp_path)
        assert result == {"present": [], "missing": []}


class TestHandlePutObjects:
    def test_stores_blob(self, tmp_path):
        data = b"hello"
        h = hashlib.sha256(data).hexdigest()
        entry = ObjectEntry(
            hash=h, kind="blob",
            data=base64.b64encode(data).decode(),
        ).to_dict()

        result = handle_put_objects([entry], tmp_path)
        assert h in result["stored"]
        assert cas.get_blob(h, tmp_path) == data

    def test_stores_object(self, tmp_path):
        obj = {"schema": 1, "kind": "test", "value": 42}
        raw = canonical_json(obj).encode("utf-8")
        h = compute_integrity(obj)
        entry = ObjectEntry(
            hash=h, kind="object",
            data=base64.b64encode(raw).decode(),
        ).to_dict()

        result = handle_put_objects([entry], tmp_path)
        assert h in result["stored"]
        assert cas.get_object(h, tmp_path) == obj

    def test_rejects_hash_mismatch(self, tmp_path):
        entry = ObjectEntry(
            hash="f" * 64, kind="blob",
            data=base64.b64encode(b"wrong").decode(),
        ).to_dict()

        result = handle_put_objects([entry], tmp_path)
        assert result["stored"] == []
        assert len(result["errors"]) == 1
        assert "mismatch" in result["errors"][0]["error"]


class TestHandleGetObjects:
    def test_retrieves_blob(self, tmp_path):
        h = cas.store_blob(b"data", tmp_path)
        result = handle_get_objects([h], tmp_path)
        assert len(result["entries"]) == 1
        entry = ObjectEntry.from_dict(result["entries"][0])
        assert entry.kind == "blob"
        assert base64.b64decode(entry.data) == b"data"

    def test_retrieves_object(self, tmp_path):
        obj = {"key": "val"}
        h = cas.store_object(obj, tmp_path)
        result = handle_get_objects([h], tmp_path)
        assert len(result["entries"]) == 1
        entry = ObjectEntry.from_dict(result["entries"][0])
        assert entry.kind == "object"

    def test_missing_skipped(self, tmp_path):
        result = handle_get_objects(["0" * 64], tmp_path)
        assert result["entries"] == []


class TestCollectObjectHashes:
    def test_collects_from_manifest(self, tmp_path):
        (tmp_path / AI_DIR / "tools").mkdir(parents=True)
        (tmp_path / AI_DIR / "tools" / "x.py").write_text("code\n")

        _, manifest = build_manifest(tmp_path, "project")
        root = cas_root(tmp_path)
        hashes = collect_object_hashes(manifest, root)

        # Should include item_source hash + content blob hash
        assert len(hashes) >= 2
        for h in hashes:
            assert cas.has(h, root)


class TestExportImport:
    def test_round_trip(self, tmp_path):
        src = tmp_path / "src"
        dst = tmp_path / "dst"
        src.mkdir()
        dst.mkdir()

        # Store some objects in src
        h1 = cas.store_blob(b"blob data", src)
        h2 = cas.store_object({"kind": "test"}, src)

        # Export from src
        entries = export_objects([h1, h2], src)
        assert len(entries) == 2

        # Import into dst
        stored = import_objects(entries, dst)
        assert set(stored) == {h1, h2}

        # Verify in dst
        assert cas.get_blob(h1, dst) == b"blob data"
        assert cas.get_object(h2, dst) == {"kind": "test"}
