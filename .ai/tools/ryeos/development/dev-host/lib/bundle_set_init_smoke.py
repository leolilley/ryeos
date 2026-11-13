#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:6db6fc5be0df4fda91259e84ab57186506e248ee95f6878ff65fb3ea8e57483d:KhAjdh/tx7kOM8FnIheKVgB+H/sZnRTlTLbXTAyGzz5vs0qrlaLwqyfImtnDJwiQ1rkyx7Ey4aqapZZqpYKfDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Initialise a throwaway node from a worktree's bundles and check it admits them.

External development host tool; it grants no RyeOS authority and spends
nothing. `ryeos init` admits every bundle's definitions together (launch
preparers, handlers, kind schemas), so a refusal a real adopt or reinstall
would hit shows here first, against a temporary app root that is removed
afterwards. `ryeosd init-policy-preflight` checks only the policy, and CI
boots images, not worktrees.

  bundle_set_init_smoke.py [--ryeos PATH | --installed] [--node-profile P]
                           [--source DIR] [--trust-file F] [--start] [--keep]
                           [--worktree DIR] [--config-file F]

Defaults come from `config:development/ryeos/dev-node`: the worktree's built
CLI, its `bundles/`, the dev publisher trust file and the node profile. The
check passes when init exits 0 and installs exactly the profile's bundle set.
With --start it also starts the node, waits for /_ryeos/ready and stops it;
a local-class host may first need `ryeos node host setup` for that.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

_spec = importlib.util.spec_from_file_location(
    "dev_host", Path(__file__).resolve().parent / "dev_host.py")
dev_host = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dev_host)
ToolError = dev_host.ToolError

# A development node is not running under a measured substrate image.
UNBOUND_SUBSTRATE = "sha256:" + "0" * 64


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def init_command(ryeos: Path, app_root: Path, source: Path, profile: str, host_class: str,
                 trust_file: Path, port: int, socket_path: Path) -> list[str]:
    return [str(ryeos), "init", "--json", "--app-root", str(app_root), "--source", str(source),
            "--node-profile", profile, "--host-class", host_class, "--trust-file", str(trust_file),
            "--bind", f"127.0.0.1:{port}", "--uds-path", str(socket_path),
            "--substrate-image-digest", UNBOUND_SUBSTRATE, "--substrate-protocol", "1"]


def ready(port: int) -> bool:
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/_ryeos/ready", timeout=5) as response:
            return json.load(response).get("ready") is True
    except (OSError, ValueError):
        return False


def start_and_stop(ryeos: Path, app_root: Path, port: int, timeout: int, logs: Path) -> tuple[bool, str]:
    with open(logs / "start.log", "w") as log:
        started = subprocess.run([str(ryeos), "start", "--app-root", str(app_root)],
                                 stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                                 timeout=timeout)
    came_up = False
    if started.returncode == 0:
        deadline = time.monotonic() + timeout
        while not (came_up := ready(port)) and time.monotonic() < deadline:
            time.sleep(2)
    with open(logs / "stop.log", "w") as log:
        subprocess.run([str(ryeos), "stop", "--app-root", str(app_root)], stdout=log,
                       stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, timeout=timeout)
    if came_up:
        return True, "node came up and stopped"
    tail = (logs / "start.log").read_text(errors="replace")[-600:]
    return False, f"start exited {started.returncode}, ready={came_up}: {tail}"


def smoke(worktree: Path, ryeos: Path, source: Path, profile: str, host_class: str, trust_file: Path,
          scratch_root: Path, init_timeout: int, ready_timeout: int, start: bool, keep: bool) -> dict:
    for path, what in ((ryeos, "ryeos CLI"), (trust_file, "trust file")):
        if not path.is_file():
            raise ToolError(f"{what} {path} does not exist")
    if not source.is_dir():
        raise ToolError(f"bundle source {source} is not a directory")
    expected = dev_host.bundle_set_names(worktree, dev_host.profile_bundle_set(worktree, profile))
    scratch_root.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="bundle-set-init-smoke-", dir=scratch_root))
    app_root, port = root / "app", free_port()
    result = {"profile": profile, "app_root": str(app_root), "expected_bundles": expected}
    try:
        outcome = _init_and_check(ryeos, root, app_root, port, source, profile, host_class, trust_file,
                                  init_timeout, ready_timeout, start, expected, result)
    finally:
        if not keep:
            shutil.rmtree(root, ignore_errors=True)
    if keep:
        outcome["kept"] = str(root)
    return outcome


def _init_and_check(ryeos, root, app_root, port, source, profile, host_class, trust_file,
                    init_timeout, ready_timeout, start, expected, result) -> dict:
    argv = init_command(ryeos, app_root, source, profile, host_class, trust_file, port, root / "node.sock")
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=init_timeout,
                              stdin=subprocess.DEVNULL, env={**os.environ, "TMPDIR": str(root)})
    except subprocess.TimeoutExpired:
        return {**result, "ok": False, "reason": f"init did not finish in {init_timeout}s"}
    (root / "init.err").write_text(done.stderr)
    if done.returncode != 0:
        return {**result, "ok": False, "init_exit": done.returncode,
                "reason": "init refused: " + done.stderr.strip()[-1200:]}
    try:
        installed = json.loads(done.stdout)["bundles_installed"]
    except (ValueError, KeyError, TypeError):
        return {**result, "ok": False, "reason": "init printed no bundles_installed JSON"}
    missing = sorted(set(expected) - set(installed))
    extra = sorted(set(installed) - set(expected))
    result.update(installed_bundles=installed, missing=missing, unexpected=extra)
    if missing or extra:
        return {**result, "ok": False, "reason": "installed bundles differ from the profile's set"}
    if start:
        came_up, detail = start_and_stop(ryeos, app_root, port, ready_timeout, root)
        result["start"] = detail
        if not came_up:
            return {**result, "ok": False, "reason": "node did not come up"}
    return {**result, "ok": True}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    cli = parser.add_mutually_exclusive_group()
    cli.add_argument("--ryeos", type=Path, help="the ryeos CLI to init with")
    cli.add_argument("--installed", action="store_true", help="use the dev node's installed CLI")
    parser.add_argument("--node-profile")
    parser.add_argument("--source", type=Path, help="bundle source dir (default: <worktree>/bundles)")
    parser.add_argument("--trust-file", type=Path)
    parser.add_argument("--start", action="store_true", help="also start the node and wait for ready")
    parser.add_argument("--keep", action="store_true", help="keep the temporary app root and logs")
    parser.add_argument("--worktree")
    parser.add_argument("--config-file", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    try:
        worktree = dev_host.worktree_argument(args.worktree)
        node = dev_host.load_dev_node(args.config_file, worktree)
        ryeos = args.ryeos or (node.installed("ryeos") if args.installed else node.built("ryeos"))
        result = smoke(worktree, ryeos, args.source or worktree / "bundles",
                       args.node_profile or node.node_profile, node.host_class,
                       args.trust_file or node.trust_file, node.scratch_root,
                       node.init_timeout, node.ready_timeout, args.start, args.keep)
    except (ToolError, subprocess.TimeoutExpired) as error:
        print(f"bundle-set-init-smoke: {error}", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(result, indent=2))
    elif result["ok"]:
        print(f"ok: {len(result['installed_bundles'])} bundles of profile {result['profile']} admit"
              + (f"; {result['start']}" if "start" in result else ""))
    else:
        print(f"FAIL {result['reason']}")
        for key in ("missing", "unexpected"):
            if result.get(key):
                print(f"  {key}: {', '.join(result[key])}")
    if "kept" in result:
        print(f"kept {result['kept']}", file=sys.stderr)
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
