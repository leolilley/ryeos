#!/usr/bin/env python3
"""Credential-free pinned App Server schema/registration probe.

This external E2E diagnostic uses the test host interpreter, not a production
RyeOS execution path. It starts no model turn, opens no RyeOS node, and never
inherits the caller's provider credentials. Registration is not tool execution
evidence. Experimental capability is toggled only in fresh disposable probes;
this does not enable it in any signed or installed worker profile.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile
import time

from probe_pinned_codex_broker import pinned_digest


MAX_WIRE_BYTES = 1024 * 1024
CAPABILITY_REFUSAL = {
    "code": -32600,
    "message": "thread/start.dynamicTools requires experimentalApi capability",
}


def observed_error(error):
    if error is None or error == CAPABILITY_REFUSAL:
        return error
    # Unexpected vendor diagnostics are not a reviewed public result surface.
    # Keep a comparison digest rather than paths or future account metadata.
    return {"unclassified_error_sha256": hashlib.sha256(
        json.dumps(error, sort_keys=True, separators=(",", ":")).encode()).hexdigest()}


class Server:
    def __init__(self, executable, root):
        self.root = root
        self.root.mkdir()
        self.environment = {
            "HOME": str(root), "CODEX_HOME": str(root),
            "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
        }
        self.process = subprocess.Popen(
            [str(executable), "app-server", "--listen", "stdio://"],
            cwd=root, env=self.environment, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = bytearray()
        self.received = 0

    def send(self, value):
        self.process.stdin.write(json.dumps(value).encode() + b"\n")
        self.process.stdin.flush()

    def response(self, request_id):
        deadline = time.monotonic() + 15
        while True:
            while b"\n" in self.buffer:
                line, _, remainder = self.buffer.partition(b"\n")
                self.buffer = bytearray(remainder)
                value = json.loads(line)
                if value.get("id") == request_id:
                    return value
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.selector.select(remaining):
                raise RuntimeError("pinned App Server response timed out")
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("pinned App Server closed before response")
            self.received += len(chunk)
            if self.received > MAX_WIRE_BYTES:
                raise RuntimeError("pinned App Server exceeded diagnostic byte limit")
            self.buffer.extend(chunk)

    def close(self):
        self.selector.close()
        if self.process.poll() is None:
            # Retire only the exact fresh diagnostic process group we own.
            os.killpg(self.process.pid, signal.SIGKILL)
        self.process.communicate(timeout=5)


def registration(executable, root, experimental):
    server = Server(executable, root)
    try:
        server.send({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "ryeos_offline_protocol_probe", "version": "1"},
            "capabilities": {"experimentalApi": experimental},
        }})
        initialized = server.response(1)
        if "error" in initialized:
            raise RuntimeError(f"initialization refused: {observed_error(initialized['error'])}")
        server.send({"method": "initialized", "params": {}})
        server.send({"id": 2, "method": "thread/start", "params": {
            "cwd": str(root), "ephemeral": True,
            "approvalPolicy": "untrusted", "sandbox": "read-only",
            "dynamicTools": [{
                "name": "ryeos_probe_execute", "description": "Offline registration probe only",
                "inputSchema": {"type": "object", "properties": {},
                                "additionalProperties": False},
            }],
        }})
        response = server.response(2)
        # No model request follows thread/start. Do not retain paths, machine
        # identity, generated thread metadata or future provider account data.
        return {
            "experimental_api": experimental,
            "registration_accepted": "result" in response,
            "error": observed_error(response.get("error")),
        }
    finally:
        server.close()


def schema_surface(executable, root, experimental):
    output = root / ("experimental-schema" if experimental else "stable-schema")
    arguments = [str(executable), "app-server", "generate-json-schema", "--out", str(output)]
    if experimental:
        arguments.append("--experimental")
    subprocess.run(arguments, cwd=root, env={"HOME": str(root), "CODEX_HOME": str(root),
                   "PATH": "/usr/bin:/bin"}, check=True, timeout=30,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    schema = json.loads((output / "v2" / "ThreadStartParams.json").read_text())
    return {
        "experimental_fields_included": experimental,
        "thread_start_has_dynamic_tools": "dynamicTools" in schema["properties"],
        "thread_start_schema_sha256": hashlib.sha256(
            (output / "v2" / "ThreadStartParams.json").read_bytes()).hexdigest(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", type=Path, required=True)
    args = parser.parse_args()
    executable = args.codex.resolve(strict=True)
    with executable.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    if digest != pinned_digest():
        parser.error("executable differs from the signed bundle's Codex pin")
    with tempfile.TemporaryDirectory(prefix="ryeos-codex-tools-probe.") as directory:
        root = Path(directory)
        result = {
            "codex_sha256": digest, "model_turns": 0, "node_mutations": 0,
            "actual_tool_emission_qualified": False,
            "schemas": [schema_surface(executable, root, flag) for flag in (False, True)],
            "registrations": [registration(executable, root / str(flag), flag)
                              for flag in (False, True)],
        }
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
