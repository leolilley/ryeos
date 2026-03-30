"""Tests for kernel-level CAS primitives."""

import hashlib
import json

import pytest

from rye.primitives import cas
from rye.primitives.integrity import canonical_json, compute_integrity


class TestStoreBlob:
    def test_store_and_retrieve(self, tmp_path):
        data = b"hello world"
        h = cas.store_blob(data, tmp_path)
        assert len(h) == 64
        assert cas.get_blob(h, tmp_path) == data

    def test_idempotent(self, tmp_path):
        data = b"same content"
        h1 = cas.store_blob(data, tmp_path)
        h2 = cas.store_blob(data, tmp_path)
        assert h1 == h2

    def test_sharding(self, tmp_path):
        data = b"sharded"
        h = cas.store_blob(data, tmp_path)
        path = tmp_path / "blobs" / h[:2] / h[2:4] / h
        assert path.exists()

    def test_hash_matches_sha256(self, tmp_path):
        data = b"verify hash"
        h = cas.store_blob(data, tmp_path)
        assert h == hashlib.sha256(data).hexdigest()


class TestStoreObject:
    def test_store_and_retrieve(self, tmp_path):
        data = {"kind": "test", "value": 42}
        h = cas.store_object(data, tmp_path)
        assert len(h) == 64
        assert cas.get_object(h, tmp_path) == data

    def test_idempotent(self, tmp_path):
        data = {"a": 1, "b": 2}
        h1 = cas.store_object(data, tmp_path)
        h2 = cas.store_object(data, tmp_path)
        assert h1 == h2

    def test_hash_via_compute_integrity(self, tmp_path):
        data = {"schema": 1, "kind": "node_result", "result": {"ok": True}}
        h = cas.store_object(data, tmp_path)
        assert h == compute_integrity(data)

    def test_sharding_with_json_ext(self, tmp_path):
        data = {"key": "val"}
        h = cas.store_object(data, tmp_path)
        path = tmp_path / "objects" / h[:2] / h[2:4] / f"{h}.json"
        assert path.exists()

    def test_stored_as_canonical_json(self, tmp_path):
        data = {"z": 1, "a": 2}
        h = cas.store_object(data, tmp_path)
        path = tmp_path / "objects" / h[:2] / h[2:4] / f"{h}.json"
        assert path.read_text() == canonical_json(data)


class TestGetBlob:
    def test_missing_returns_none(self, tmp_path):
        assert cas.get_blob("0" * 64, tmp_path) is None


class TestGetObject:
    def test_missing_returns_none(self, tmp_path):
        assert cas.get_object("0" * 64, tmp_path) is None


class TestHas:
    def test_has_blob(self, tmp_path):
        h = cas.store_blob(b"exists", tmp_path)
        assert cas.has(h, tmp_path) is True

    def test_has_object(self, tmp_path):
        h = cas.store_object({"present": True}, tmp_path)
        assert cas.has(h, tmp_path) is True

    def test_missing(self, tmp_path):
        assert cas.has("f" * 64, tmp_path) is False


class TestHasMany:
    def test_batch(self, tmp_path):
        h1 = cas.store_blob(b"one", tmp_path)
        h2 = cas.store_object({"two": 2}, tmp_path)
        missing = "0" * 64
        result = cas.has_many([h1, h2, missing], tmp_path)
        assert result == {h1: True, h2: True, missing: False}
