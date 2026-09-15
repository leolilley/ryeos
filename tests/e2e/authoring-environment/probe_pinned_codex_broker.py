#!/usr/bin/env python3
"""Offline vendor transport diagnostic, not hosted execution qualification.

Uses the test host's Python/runtime only to observe the pinned vendor sandbox.
It never opens a RyeOS node, authenticates to a provider, or calls a model.
No production dependency or alternate worker launch path is established here.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile

import yaml


CHILD = r'''
import json, os, socket, urllib.parse

def proxy_request(request):
    endpoint = urllib.parse.urlsplit(os.environ["HTTP_PROXY"])
    if endpoint.scheme != "http" or endpoint.hostname != "127.0.0.1":
        raise RuntimeError("diagnostic proxy is not an explicit loopback HTTP listener")
    with socket.create_connection((endpoint.hostname, endpoint.port), timeout=3) as stream:
        stream.sendall(request)
        chunks = bytearray()
        while len(chunks) < 8192:
            chunk = stream.recv(min(1024, 8192 - len(chunks)))
            if not chunk:
                break
            chunks.extend(chunk)
        return chunks.decode("ascii", errors="replace")

result = {"pid_in_command_namespace": os.getpid()}
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(3)
        stream.connect(os.environ["PROBE_ENDPOINT"])
        result["direct_unix"] = {"connected": True}
except OSError as error:
    result["direct_unix"] = {"connected": False, "errno": error.errno}

if "HTTP_PROXY" in os.environ:
    result["unix_proxy"] = proxy_request((
        "POST http://localhost:80/ HTTP/1.1\r\nHost: localhost:80\r\n"
        "x-unix-socket: " + os.environ["PROBE_ENDPOINT"] +
        "\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").encode())
    # Reserved .invalid name: the empty domain allowlist must refuse this at
    # the proxy. No real external destination or successful DNS is required.
    result["external_proxy"] = proxy_request(
        b"GET http://broker-denied.invalid/ HTTP/1.1\r\nHost: broker-denied.invalid\r\n"
        b"Connection: close\r\n\r\n")
print(json.dumps(result))
'''


def pinned_digest():
    repository = Path(__file__).resolve().parents[3]
    activation = yaml.safe_load((repository /
        "bundles/codex/.ai/config/codex/activation.yaml").read_text())
    return next(member["sha256"] for source in activation["sources"]
                for member in source["members"] if member["path"] == "bin/codex")


def run_bounded(arguments, environment):
    # This is a standalone external test, not a RyeOS process owner. On a
    # timeout retire only the fresh diagnostic process group we just created.
    process = subprocess.Popen(arguments, env=environment, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=20)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise RuntimeError("pinned sandbox diagnostic exceeded 20 seconds") from None
    if process.returncode != 0:
        raise RuntimeError(f"sandbox exited {process.returncode}: {stderr[-2048:]}")
    return json.loads(stdout), stderr[-2048:]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", type=Path, required=True,
                        help="retained exact pinned executable; never the user's running Codex")
    args = parser.parse_args()
    executable = args.codex.resolve(strict=True)
    with executable.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    if digest != pinned_digest():
        parser.error("executable differs from the signed bundle's Codex pin")
    observations = []
    with tempfile.TemporaryDirectory(prefix="ryeos-codex-broker-probe.") as temporary:
        root = Path(temporary)
        home = root / "home"
        project = root / "project"
        home.mkdir()
        project.mkdir()
        endpoint = str(root / "broker.sock")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            listener.bind(endpoint)
            listener.listen(4)
            environment = {
                "HOME": str(home), "CODEX_HOME": str(home), "PATH": "/usr/bin:/bin",
                "LANG": "C.UTF-8", "PROBE_ENDPOINT": endpoint,
            }
            for proxied in (False, True):
                # Broader test-host filesystem reads supply the diagnostic
                # interpreter. They are NOT a proposed worker permission set.
                permission = ('{ filesystem={ ":root"="read", '
                              '":workspace_roots"="write" }, network={ enabled='
                              + str(proxied).lower() + ', domains={}, unix_sockets={ '
                              + json.dumps(endpoint) + '="allow" } } }')
                command = [str(executable), "sandbox", "-C", str(project),
                           "-P", "broker-diagnostic", "-c",
                           "permissions.broker-diagnostic=" + permission,
                           "-c", "features.network_proxy=" + str(proxied).lower(),
                           sys.executable, "-I", "-c", CHILD]
                observed, warning = run_bounded(command, environment)
                observations.append({"proxy_enabled": proxied, **observed,
                                     "diagnostic_stderr": warning})
            # Neither attempted transport should have contacted this socket.
            listener.settimeout(0.1)
            try:
                unexpected, _ = listener.accept()
            except TimeoutError:
                pass
            else:
                unexpected.close()
                raise RuntimeError("pinned vendor transport behavior changed: broker was contacted")
    direct, proxied = observations
    assert direct["direct_unix"] == {"connected": False, "errno": 1}
    assert proxied["direct_unix"] == {"connected": False, "errno": 1}
    assert proxied["unix_proxy"].startswith("HTTP/1.1 501 ")
    assert "unix sockets unsupported" in proxied["unix_proxy"]
    assert proxied["external_proxy"].startswith("HTTP/1.1 403 ")
    print(json.dumps({"schema_version": 1, "codex_sha256": digest,
                      "credential_free": True, "model_calls": 0,
                      "hosted_qualification": False,
                      "observed_blocker": "pinned_linux_unix_socket_proxy_unsupported",
                      "observations": observations}, indent=2))


if __name__ == "__main__":
    main()
