#!/usr/bin/env python3
"""Exercise the exact local worker through its real duplex session loop."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import tempfile
import unittest


PROTOCOL = "ryeos.persistent-session"
VERSION = 1
MAX_FRAME_BYTES = 16 * 1024 * 1024


def canonical_request(*, output_tokens: int = 6) -> dict[str, object]:
    body = {
        "model": "qwen3-0.6b",
        "messages": [
            {
                "role": "user",
                "content": "Reply with exactly `OK` and nothing else.\n",
            }
        ],
        "tools": [],
        "stream": True,
        "stream_options": {"include_usage": True},
        "max_tokens": output_tokens,
        "temperature": 0.0,
        "seed": 0,
        "enable_thinking": False,
    }
    encoded = json.dumps(body, ensure_ascii=False, separators=(",", ":"))
    return {
        "request_body": encoded,
        "request_body_sha256": hashlib.sha256(encoded.encode("utf-8")).hexdigest(),
        "requested_output_ceiling": 256,
    }


def write_frame(
    channel: socket.socket,
    kind: str,
    request_id: str | None,
    body: object,
) -> None:
    frame = {
        "protocol": PROTOCOL,
        "version": VERSION,
        "kind": kind,
        "request_id": request_id,
        "body": body,
    }
    encoded = json.dumps(
        frame,
        ensure_ascii=False,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")
    framed = struct.pack(">I", len(encoded)) + encoded
    written = 0
    while written < len(framed):
        count = os.write(channel.fileno(), framed[written:])
        if count == 0:
            raise EOFError("session worker closed its channel while writing")
        written += count


def read_exact(channel: socket.socket, length: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < length:
        readable, _, _ = select.select([channel], [], [], 120)
        if not readable:
            raise TimeoutError("session worker did not produce a frame before its deadline")
        chunk = os.read(channel.fileno(), length - len(chunks))
        if not chunk:
            raise EOFError("session worker closed its channel")
        chunks.extend(chunk)
    return bytes(chunks)


def read_frame(channel: socket.socket) -> dict[str, object]:
    length = struct.unpack(">I", read_exact(channel, 4))[0]
    if length == 0 or length > MAX_FRAME_BYTES:
        raise AssertionError(f"session frame length is invalid: {length}")
    frame = json.loads(read_exact(channel, length))
    if (
        not isinstance(frame, dict)
        or set(frame) != {"protocol", "version", "kind", "request_id", "body"}
        or frame["protocol"] != PROTOCOL
        or frame["version"] != VERSION
    ):
        raise AssertionError(f"session frame is not canonical: {frame!r}")
    return frame


class ExactSessionProcess:
    def __init__(self, workspace: Path):
        self.workspace = workspace
        self.parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        child.set_inheritable(True)
        self.stderr = tempfile.TemporaryFile(mode="w+b")
        environment = {
            "HOME": str(workspace / "scratch" / "home"),
            "PYTHONHOME": str(workspace / "runtime" / "python"),
            "PYTHONHASHSEED": "0",
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTHONNOUSERSITE": "1",
            "PYTHONSAFEPATH": "1",
            "PYTHONUNBUFFERED": "1",
            "RYEOS_SESSION_FD": str(child.fileno()),
            "PATH": "",
            "DEV": "CPU",
            "CACHELEVEL": "0",
            "CCACHE": "0",
            "LANG": "C",
            "LC_ALL": "C",
        }
        (workspace / "scratch" / "home").mkdir(parents=True, exist_ok=True)
        self.process = subprocess.Popen(
            [
                str(workspace / "runtime" / "lib" / "ld-musl-x86_64.so.1"),
                "--library-path",
                str(workspace / "runtime" / "lib"),
                str(workspace / "runtime" / "python" / "bin" / "python3.14"),
                "-P",
                "-S",
                str(workspace / "worker" / "bootstrap.py"),
            ],
            cwd=workspace,
            env=environment,
            pass_fds=(child.fileno(),),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=self.stderr,
        )
        child.close()

    def diagnostics(self) -> str:
        self.stderr.flush()
        self.stderr.seek(0)
        return self.stderr.read().decode("utf-8", errors="replace")

    def close(self) -> None:
        self.parent.close()
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        self.stderr.close()


class PersistentSessionProtocolTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        raw_workspace = os.environ.get("RYEOS_LOCAL_WORKER_TEST_WORKSPACE")
        if raw_workspace is None:
            raise RuntimeError("RYEOS_LOCAL_WORKER_TEST_WORKSPACE is absent")
        cls.workspace = Path(raw_workspace).resolve(strict=True)

    def test_real_session_streams_reuses_recovers_from_cancel_and_stays_ready(self) -> None:
        session = ExactSessionProcess(self.workspace)
        try:
            ready = read_frame(session.parent)
            self.assertEqual(
                ready,
                {
                    "protocol": PROTOCOL,
                    "version": VERSION,
                    "kind": "ready",
                    "request_id": None,
                    "body": None,
                },
                session.diagnostics(),
            )

            for request_id in ("first", "same-process-second"):
                write_frame(session.parent, "request", request_id, canonical_request())
                deltas: list[dict[str, object]] = []
                while True:
                    frame = read_frame(session.parent)
                    self.assertEqual(frame["request_id"], request_id)
                    if frame["kind"] == "delta":
                        self.assertIsInstance(frame["body"], dict)
                        deltas.append(frame["body"])
                        continue
                    self.assertEqual(frame["kind"], "final", session.diagnostics())
                    answer = frame["body"]
                    self.assertIsInstance(answer, dict)
                    self.assertEqual(answer["answer"]["message"]["content"], "OK")
                    self.assertEqual(answer["answer"]["finish_reason"], "stop")
                    self.assertTrue(deltas, "real session produced no streaming delta")
                    self.assertEqual(
                        "".join(
                            str(delta["text"])
                            for delta in deltas
                            if delta.get("kind") == "text_delta"
                        ),
                        "OK",
                    )
                    break

            write_frame(
                session.parent,
                "request",
                "cancelled",
                canonical_request(output_tokens=256),
            )
            write_frame(session.parent, "cancel", "cancelled", None)
            cancelled = read_frame(session.parent)
            while cancelled["kind"] == "delta":
                cancelled = read_frame(session.parent)
            self.assertEqual(cancelled["kind"], "error", session.diagnostics())
            self.assertIn("cancelled", cancelled["body"]["message"])

            write_frame(session.parent, "request", "after-cancel", canonical_request())
            terminal = read_frame(session.parent)
            while terminal["kind"] == "delta":
                terminal = read_frame(session.parent)
            self.assertEqual(terminal["kind"], "final", session.diagnostics())
            self.assertEqual(
                terminal["body"]["answer"]["message"]["content"],
                "OK",
            )
            self.assertIsNone(session.process.poll(), session.diagnostics())
        finally:
            session.close()


if __name__ == "__main__":
    unittest.main()
