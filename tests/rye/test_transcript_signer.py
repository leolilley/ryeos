"""Tests for transcript checkpoint signing and JSON signing utilities."""

import importlib.util
import json
from pathlib import Path

import pytest

from lillux.primitives.signing import compute_key_fingerprint
from rye.constants import AI_DIR

PROJECT_ROOT = Path(__file__).parent.parent.parent

SIGNER_PATH = (
    PROJECT_ROOT
    / "ryeos" / "bundles" / "standard" / "ryeos_std" / ".ai" / "tools" / "rye" / "agent" / "threads"
    / "persistence" / "transcript_signer.py"
)
_spec = importlib.util.spec_from_file_location("transcript_signer", SIGNER_PATH)
_signer_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_signer_mod)
TranscriptSigner = _signer_mod.TranscriptSigner
sign_json = _signer_mod.sign_json
verify_json = _signer_mod.verify_json


@pytest.fixture
def temp_env(tmp_path, _setup_user_space):
    """Set up thread dir for transcript signing tests.

    Keys and trust store are provided by conftest's _setup_user_space fixture.
    """
    user_space = _setup_user_space
    key_dir = user_space / AI_DIR / "keys"
    private_pem = (key_dir / "private_key.pem").read_text()
    public_pem = (key_dir / "public_key.pem").read_text()
    fp = compute_key_fingerprint(public_pem.encode() if isinstance(public_pem, str) else public_pem)

    thread_dir = tmp_path / "project" / AI_DIR / "agent" / "threads" / "test-thread"
    thread_dir.mkdir(parents=True)

    return {
        "thread_dir": thread_dir,
        "private_pem": private_pem,
        "public_pem": public_pem,
        "fp": fp,
    }


def _write_event(jsonl_path, event_type, payload, thread_id="test-thread"):
    entry = {
        "timestamp": 1700000000,
        "thread_id": thread_id,
        "event_type": event_type,
        "payload": payload,
    }
    with open(jsonl_path, "a") as f:
        f.write(json.dumps(entry) + "\n")


class TestTranscriptSigner:
    def test_checkpoint_creates_event(self, temp_env):
        td = temp_env["thread_dir"]
        jsonl = td / "transcript.jsonl"
        _write_event(jsonl, "cognition_in", {"text": "hello", "role": "user"})
        _write_event(jsonl, "cognition_out", {"text": "hi there"})

        signer = TranscriptSigner("test-thread", td)
        signer.checkpoint(1)

        events = [json.loads(line) for line in jsonl.read_text().splitlines()]
        assert len(events) == 3
        cp = events[2]
        assert cp["event_type"] == "checkpoint"
        assert cp["payload"]["turn"] == 1
        assert "hash" in cp["payload"]
        assert "sig" in cp["payload"]
        assert "fp" in cp["payload"]

    def test_verify_valid_checkpoint(self, temp_env):
        td = temp_env["thread_dir"]
        jsonl = td / "transcript.jsonl"
        _write_event(jsonl, "cognition_in", {"text": "hello", "role": "user"})
        _write_event(jsonl, "cognition_out", {"text": "hi"})

        signer = TranscriptSigner("test-thread", td)
        signer.checkpoint(1)

        result = signer.verify()
        assert result["valid"] is True
        assert result["checkpoints"] == 1

    def test_verify_detects_tampering(self, temp_env):
        td = temp_env["thread_dir"]
        jsonl = td / "transcript.jsonl"
        _write_event(jsonl, "cognition_in", {"text": "hello", "role": "user"})
        _write_event(jsonl, "cognition_out", {"text": "hi"})

        signer = TranscriptSigner("test-thread", td)
        signer.checkpoint(1)

        # Tamper with the transcript before the checkpoint
        content = jsonl.read_text()
        lines = content.splitlines()
        event = json.loads(lines[0])
        event["payload"]["text"] = "INJECTED"
        lines[0] = json.dumps(event)
        jsonl.write_text("\n".join(lines) + "\n")

        result = signer.verify()
        assert result["valid"] is False
        assert "hash mismatch" in result["error"]

    def test_verify_detects_unsigned_trailing(self, temp_env):
        td = temp_env["thread_dir"]
        jsonl = td / "transcript.jsonl"
        _write_event(jsonl, "cognition_in", {"text": "hello", "role": "user"})

        signer = TranscriptSigner("test-thread", td)
        signer.checkpoint(1)

        # Append unsigned content after checkpoint
        _write_event(jsonl, "cognition_out", {"text": "injected after checkpoint"})

        result = signer.verify()
        assert result["valid"] is False
        assert "Unsigned content" in result["error"]

    def test_verify_lenient_allows_trailing(self, temp_env):
        td = temp_env["thread_dir"]
        jsonl = td / "transcript.jsonl"
        _write_event(jsonl, "cognition_in", {"text": "hello", "role": "user"})

        signer = TranscriptSigner("test-thread", td)
        signer.checkpoint(1)

        _write_event(jsonl, "cognition_out", {"text": "trailing"})

        result = signer.verify(allow_unsigned_trailing=True)
        assert result["valid"] is True

    def test_verify_no_file(self, temp_env):
        signer = TranscriptSigner("test-thread", temp_env["thread_dir"])
        result = signer.verify()
        assert result["valid"] is True
        assert result["unsigned"] is True

    def test_verify_no_checkpoints(self, temp_env):
        td = temp_env["thread_dir"]
        jsonl = td / "transcript.jsonl"
        _write_event(jsonl, "cognition_in", {"text": "hello", "role": "user"})

        signer = TranscriptSigner("test-thread", td)
        result = signer.verify()
        assert result["valid"] is True
        assert result["unsigned"] is True

    def test_multiple_checkpoints(self, temp_env):
        td = temp_env["thread_dir"]
        jsonl = td / "transcript.jsonl"

        _write_event(jsonl, "cognition_in", {"text": "turn1", "role": "user"})
        _write_event(jsonl, "cognition_out", {"text": "response1"})

        signer = TranscriptSigner("test-thread", td)
        signer.checkpoint(1)

        _write_event(jsonl, "cognition_in", {"text": "turn2", "role": "user"})
        _write_event(jsonl, "cognition_out", {"text": "response2"})
        signer.checkpoint(2)

        result = signer.verify()
        assert result["valid"] is True
        assert result["checkpoints"] == 2


class TestJsonSigning:
    def test_sign_and_verify(self, temp_env):
        data = {"thread_id": "test", "status": "running", "capabilities": ["read"]}
        signed = sign_json(data)

        assert "_signature" in signed
        assert signed["_signature"].startswith("rye:signed:")
        assert verify_json(signed) is True

    def test_verify_detects_tampering(self, temp_env):
        data = {"thread_id": "test", "capabilities": ["read"]}
        signed = sign_json(data)

        signed["capabilities"] = ["read", "write", "execute"]
        assert verify_json(signed) is False

    def test_verify_unsigned(self, temp_env):
        assert verify_json({"thread_id": "test"}) is False

    def test_verify_bad_format(self, temp_env):
        assert verify_json({"_signature": "garbage"}) is False

    def test_signature_excludes_itself(self, temp_env):
        data = {"a": 1, "b": 2}
        signed = sign_json(data)

        # Re-signing should produce a valid signature (old _signature excluded from hash)
        re_signed = sign_json(dict(signed))
        assert verify_json(re_signed) is True
