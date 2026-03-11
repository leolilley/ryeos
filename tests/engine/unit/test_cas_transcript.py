"""Tests for CAS-native transcript persistence (Step 7b)."""

import asyncio
import json
import sys
import tempfile
from pathlib import Path

import pytest

from conftest import get_bundle_path

_WALKER_DIR = str(get_bundle_path("core", "tools/rye/core/runtimes/state-graph"))


class TestPersistStateCAS:
    """Test CAS-based state persistence."""

    @pytest.mark.asyncio
    async def test_persist_creates_cas_objects(self, _setup_user_space):
        """_persist_state should create state_snapshot and execution_snapshot in CAS."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "objects").mkdir(parents=True)

            walker_dir = _WALKER_DIR
            sys.path.insert(0, walker_dir)
            try:
                from walker import _persist_state

                result = await _persist_state(
                    str(project), "test-graph", "run-123",
                    {"key": "value"}, "node1", "running", 3,
                )

                assert result is not None  # snapshot hash

                # Verify ref was written
                ref_path = project / ".ai" / "objects" / "refs" / "graphs" / "run-123.json"
                assert ref_path.exists()

                # Verify ref points to valid object
                from rye.cas.store import read_ref, cas_root
                from lillux.primitives import cas

                ref_hash = read_ref(ref_path)
                assert ref_hash == result

                snapshot = cas.get_object(ref_hash, cas_root(project))
                assert snapshot is not None
                assert snapshot["kind"] == "execution_snapshot"
                assert snapshot["graph_run_id"] == "run-123"
                assert snapshot["step"] == 3
                assert snapshot["status"] == "running"

                # Verify state is accessible
                state_obj = cas.get_object(snapshot["state_hash"], cas_root(project))
                assert state_obj is not None
                assert state_obj["kind"] == "state_snapshot"
                assert state_obj["state"] == {"key": "value"}
            finally:
                sys.path.remove(walker_dir)

    @pytest.mark.asyncio
    async def test_persist_updates_ref_on_each_call(self, _setup_user_space):
        """Each _persist_state call should update the mutable ref."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "objects").mkdir(parents=True)

            walker_dir = _WALKER_DIR
            sys.path.insert(0, walker_dir)
            try:
                from walker import _persist_state

                h1 = await _persist_state(
                    str(project), "g", "r1",
                    {"step": 1}, "n1", "running", 1,
                )
                h2 = await _persist_state(
                    str(project), "g", "r1",
                    {"step": 2}, "n2", "running", 2,
                )

                # Both should succeed
                assert h1 is not None
                assert h2 is not None
                # Different state → different hashes
                assert h1 != h2

                # Ref should point to latest
                from rye.cas.store import read_ref
                ref_path = project / ".ai" / "objects" / "refs" / "graphs" / "r1.json"
                assert read_ref(ref_path) == h2
            finally:
                sys.path.remove(walker_dir)


class TestCheckpointCAS:
    """Test CAS-native checkpoint."""

    def test_checkpoint_emits_state_checkpoint_event(self, _setup_user_space):
        """Checkpoint should emit state_checkpoint with state_hash."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "objects").mkdir(parents=True)

            walker_dir = _WALKER_DIR
            sys.path.insert(0, walker_dir)
            try:
                from walker import GraphTranscript

                transcript = GraphTranscript(str(project), "test-graph", "run-1", {})
                transcript.checkpoint(1, state={"x": 1}, current_node="n1")

                # Read JSONL
                jsonl_path = project / ".ai" / "agent" / "graphs" / "run-1" / "transcript.jsonl"
                events = []
                with open(jsonl_path) as f:
                    for line in f:
                        line = line.strip()
                        if line:
                            events.append(json.loads(line))

                # Find state_checkpoint event
                checkpoint_events = [e for e in events if e["event_type"] == "state_checkpoint"]
                assert len(checkpoint_events) == 1

                payload = checkpoint_events[0]["payload"]
                assert payload["step"] == 1
                assert payload["current_node"] == "n1"
                assert "state_hash" in payload
                assert payload["state_hash"]  # non-empty

                # Verify state is in CAS
                from lillux.primitives import cas
                from rye.cas.store import cas_root
                state_obj = cas.get_object(payload["state_hash"], cas_root(project))
                assert state_obj is not None
                assert state_obj["state"] == {"x": 1}
            finally:
                sys.path.remove(walker_dir)
