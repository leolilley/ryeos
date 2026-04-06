"""Tests for Rye-level CAS store and object model."""

import json

import pytest

from rye.primitives import cas
from rye.cas.objects import (
    ItemSource,
    SourceManifest,
    NodeInput,
    NodeResult,
    NodeReceipt,
    ExecutionSnapshot,
    StateSnapshot,
    ConfigSnapshot,
    ArtifactIndex,
    SCHEMA_VERSION,
)
from rye.cas.store import (
    cas_root,
    user_cas_root,
    ingest_item,
    materialize_item,
    write_ref,
    read_ref,
)
from rye.constants import AI_DIR


class TestCasRoot:
    def test_project_cas_root(self, tmp_path):
        assert cas_root(tmp_path) == tmp_path / AI_DIR / "state" / "objects"

    def test_user_cas_root(self, monkeypatch, tmp_path):
        monkeypatch.setenv("USER_SPACE", str(tmp_path))
        assert user_cas_root() == tmp_path / AI_DIR / "state" / "objects"


class TestIngestAndMaterialize:
    def _make_tool(self, project_path):
        """Create a minimal .ai/tools/test_tool.py file."""
        tools_dir = project_path / AI_DIR / "tools"
        tools_dir.mkdir(parents=True)
        tool_file = tools_dir / "test_tool.py"
        tool_file.write_text("# a simple tool\nprint('hello')\n")
        return tool_file

    def test_ingest_produces_valid_ref(self, tmp_path):
        tool_file = self._make_tool(tmp_path)
        ref = ingest_item("tool", tool_file, tmp_path)
        assert ref.blob_hash
        assert ref.object_hash
        assert ref.integrity
        # Unsigned file → no signature_info
        assert ref.signature_info is None

    def test_round_trip(self, tmp_path):
        tool_file = self._make_tool(tmp_path)
        original = tool_file.read_bytes()
        ref = ingest_item("tool", tool_file, tmp_path)

        out_path = tmp_path / "materialized" / "test_tool.py"
        materialize_item(ref.object_hash, out_path, cas_root(tmp_path))
        assert out_path.read_bytes() == original

    def test_materialize_missing_object(self, tmp_path):
        with pytest.raises(FileNotFoundError, match="Object.*not found"):
            materialize_item("0" * 64, tmp_path / "out.py", tmp_path)


class TestRefs:
    def test_write_and_read(self, tmp_path):
        ref_path = tmp_path / "refs" / "test.json"
        write_ref(ref_path, "abc123")
        assert read_ref(ref_path) == "abc123"

    def test_overwrite(self, tmp_path):
        ref_path = tmp_path / "refs" / "test.json"
        write_ref(ref_path, "first")
        write_ref(ref_path, "second")
        assert read_ref(ref_path) == "second"

    def test_read_missing(self, tmp_path):
        assert read_ref(tmp_path / "nonexistent.json") is None


class TestObjectModel:
    def test_item_source_to_dict(self):
        obj = ItemSource(
            item_type="tool",
            item_id="my_tool",
            content_blob_hash="a" * 64,
            integrity="b" * 64,
            signature_info=None,
        )
        d = obj.to_dict()
        assert d["schema"] == SCHEMA_VERSION
        assert d["kind"] == "item_source"
        assert d["signature_info"] is None

    def test_source_manifest_to_dict(self):
        obj = SourceManifest(
            space="project",
            items={".ai/tools/x.py": "hash1"},
            files={"src/main.py": "hash2"},
        )
        d = obj.to_dict()
        assert d["kind"] == "source_manifest"
        assert d["space"] == "project"
        assert len(d["items"]) == 1
        assert len(d["files"]) == 1

    def test_node_result_stores_full_dict(self):
        result = {"thread_id": "t-123", "summary": "done", "extra": [1, 2]}
        obj = NodeResult(result=result)
        d = obj.to_dict()
        assert d["result"] == result

    def test_execution_snapshot_has_system_version(self):
        obj = ExecutionSnapshot(
            graph_run_id="run-1",
            graph_id="g-1",
            system_version="0.5.0",
            step=3,
            status="completed",
            state_hash="c" * 64,
            node_receipts=["r1", "r2"],
        )
        d = obj.to_dict()
        assert d["system_version"] == "0.5.0"
        assert len(d["node_receipts"]) == 2

    def test_all_kinds_have_schema_and_kind(self):
        objects = [
            ItemSource(),
            SourceManifest(),
            ConfigSnapshot(),
            NodeInput(),
            NodeResult(),
            NodeReceipt(),
            ExecutionSnapshot(),
            StateSnapshot(),
            ArtifactIndex(),
        ]
        for obj in objects:
            d = obj.to_dict()
            assert d["schema"] == SCHEMA_VERSION
            assert "kind" in d and d["kind"]

    def test_objects_storable_in_cas(self, tmp_path):
        obj = NodeResult(result={"ok": True})
        h = cas.store_object(obj.to_dict(), tmp_path)
        retrieved = cas.get_object(h, tmp_path)
        assert retrieved == obj.to_dict()
