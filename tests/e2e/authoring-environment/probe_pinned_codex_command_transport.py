#!/usr/bin/env python3
"""Credential-free vendor launch probe; not RyeOS admission qualification.

The harness deliberately passes one host descriptor to the pinned real provider
to observe whether its command transport can execute it. The short-lived shell
only writes bounded synthetic facts and exits; it is not a production connector.
No turn/start, credentials, node access, or model request is used.
The second probe uses the pinned vendor's own exec-server over its command
transport. It proves protocol interoperability, not the RyeOS connector route.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import tempfile

from probe_pinned_codex_broker import pinned_digest
from probe_pinned_codex_tools import Server


def probe(executable, root):
    marker = root / "observed.txt"
    # Intentionally exercise inherited-descriptor execution in the real vendor,
    # without adding descriptor adoption to a substitute provider.
    descriptor = os.open("/bin/sh", os.O_RDONLY | os.O_CLOEXEC)
    program = f"/proc/self/fd/{descriptor}"
    script = 'printf "%s\\n" "$TRANSPORT_SENTINEL" "$HOME" "$PWD" > "$MARKER"'
    config = (
        'default = "probe"\ninclude_local = false\n[[environments]]\n'
        'id = "probe"\nprogram = ' + json.dumps(program) + '\n'
        'args = ' + json.dumps(["-c", script]) + '\n'
        '[environments.env]\nTRANSPORT_SENTINEL = "configured-value"\n'
        'MARKER = ' + json.dumps(str(marker)) + '\n'
    )
    server = None
    try:
        server = Server(executable, root / "home", environments=config,
                        inherited_fds=(descriptor,))
        os.close(descriptor)
        descriptor = None
        server.send({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "ryeos_transport_probe", "version": "1"},
            "capabilities": {"experimentalApi": True}}})
        if "error" in server.response(1):
            raise RuntimeError("provider initialization refused")
        server.send({"method": "initialized", "params": {}})
        server.send({"id": 2, "method": "thread/start", "params": {
            "cwd": str(root), "ephemeral": True,
            "approvalPolicy": "never", "sandbox": "read-only"}})
        response = server.response(2)
        observed = marker.read_text().splitlines() if marker.exists() else []
        return {
            "connector_program_executed": bool(observed),
            "configured_environment_received": observed[:1] == ["configured-value"],
            "provider_home_inherited": len(observed) == 3 and observed[1] == str(server.root),
            "provider_cwd_inherited": len(observed) == 3 and observed[2] == str(server.root),
            "thread_start_returned_error": "error" in response,
            "protocol_handshake_qualified": False,
            "ryeos_admission_qualified": False,
        }
    finally:
        if descriptor is not None:
            os.close(descriptor)
        if server is not None:
            server.close()


def probe_command_handshake(executable, root, private_diagnostics=None):
    config = (
        'default = "probe"\ninclude_local = false\n[[environments]]\n'
        'id = "probe"\nprogram = ' + json.dumps(str(executable)) + '\n'
        'args = ["exec-server", "--listen", "stdio"]\n'
    )
    transport_log = (private_diagnostics.with_suffix(".stderr")
                     if private_diagnostics is not None else None)
    server = Server(executable, root / "handshake-home", environments=config,
                    private_transport_log=transport_log)
    try:
        server.send({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "ryeos_command_handshake_probe", "version": "1"},
            "capabilities": {"experimentalApi": True}}})
        if "error" in server.response(1):
            raise RuntimeError("provider initialization refused")
        server.send({"method": "initialized", "params": {}})
        server.send({"id": 2, "method": "environment/info", "params": {
            "environmentId": "probe"}})
        response = server.response(2)
        info = response.get("result", {})
        error = response.get("error", {})
        if error and private_diagnostics is not None:
            # Explicit diagnostic opt-in, create-only and owner-private. Never
            # forward unclassified vendor text into the ordinary public report.
            descriptor = os.open(private_diagnostics, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w") as output:
                json.dump(error, output)
        message = error.get("message", "").lower()
        server.send({"id": 3, "method": "environment/info", "params": {
            "environmentId": "local"}})
        local = server.response(3)
        return {
            "vendor_command_protocol_works": bool(info.get("shell", {}).get("name")),
            "command_cwd_matches_private_home": info.get("cwd") == server.root.as_uri(),
            "local_environment_refused": "error" in local,
            "handshake_error_code": error.get("code"),
            "handshake_error_category": next((category for token, category in (
                ("unknown environment", "unknown_environment"),
                ("method not found", "unsupported_method"),
                ("experimental", "experimental_capability"),
                ("operation not permitted", "host_permission"),
                ("permission denied", "host_permission"),
                ("connection", "connection_failure"),
                ("closed", "transport_closed"),
            ) if token in message), "unclassified" if error else None),
            "ryeos_connector_qualified": False,
            "native_mount_delivery_qualified": False,
        }
    finally:
        server.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", type=Path, required=True)
    parser.add_argument("--private-diagnostics", type=Path,
                        help="create an owner-private error artifact; never overwrite")
    args = parser.parse_args()
    executable = args.codex.resolve(strict=True)
    with executable.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    if digest != pinned_digest():
        parser.error("executable differs from authored Codex pin")
    with tempfile.TemporaryDirectory(prefix="ryeos-codex-transport-probe.") as directory:
        result = probe(executable, Path(directory))
        handshake = probe_command_handshake(executable, Path(directory), args.private_diagnostics)
    print(json.dumps({"codex_sha256": digest, "model_turns": 0,
                      "descriptor_launch": result, "command_handshake": handshake}, indent=2))
    if not (result["connector_program_executed"]
            and handshake["vendor_command_protocol_works"]
            and handshake["command_cwd_matches_private_home"]
            and handshake["local_environment_refused"]):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
