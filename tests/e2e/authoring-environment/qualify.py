#!/usr/bin/env python3
"""Empty-root, offline runtime-closure probe; does not operate any RyeOS node."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import subprocess
import time
HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[2]
PRODUCTION_OWNER = REPOSITORY / ".ai/tools/ryeos/development/authoring-environment-production/lib/production.py"


class ProbeRefused(RuntimeError):
    def __init__(self, reason, output):
        super().__init__(reason)
        self.output = output


def run_bounded(command, *, timeout=90, maximum_output=1024 * 1024):
    """One finite probe, including host-side diagnostics and wall-clock bounds."""
    output = bytearray()
    deadline = time.monotonic() + timeout
    with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT) as process:
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(process.stdout, selectors.EVENT_READ)
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise ProbeRefused("probe exceeded its duration bound", bytes(output))
                    if not ready.select(min(remaining, 1)):
                        continue
                    chunk = os.read(process.stdout.fileno(), min(65536, maximum_output - len(output) + 1))
                    if not chunk:
                        break
                    output.extend(chunk)
                    if len(output) > maximum_output:
                        raise ProbeRefused("probe exceeded its diagnostic bound", bytes(output[:maximum_output]))
            try:
                status = process.wait(timeout=max(0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                raise ProbeRefused("probe exceeded its duration bound", bytes(output)) from None
            return status, bytes(output)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--production", type=Path, required=True)
    parser.add_argument("--inventory-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location("production", PRODUCTION_OWNER)
    production = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(production)
    if production.receipt(args.production)["inventory_sha256"] != args.inventory_sha256:
        raise ValueError("selected production inventory does not match")
    expected = production.inventory(args.production / "environment")
    args.output.mkdir(parents=False, exist_ok=False)
    context = args.output / "context"
    context.mkdir()
    shutil.copytree(args.production / "environment", context / "environment", symlinks=True)
    if production.inventory(context / "environment") != expected:
        raise ValueError("probe copy differs from selected production")
    shutil.copyfile(HERE / "probe.sh", context / "probe.sh")
    shutil.copyfile(HERE / "Dockerfile.probe", context / "Dockerfile")
    # Use an exact image ID, not a mutable tag, for the qualifying execution.
    image_id = args.output / "image-id"
    subprocess.run(["docker", "build", "--network=none", "--iidfile", str(image_id), str(context)],
                   check=True, timeout=120)
    selected_image = image_id.read_text().strip()
    container_id = args.output / "container-id"
    command = [
        "docker", "run", "--rm", "--pull=never", "--network=none", "--read-only", "--cap-drop=ALL",
        "--cidfile", str(container_id),
        "--security-opt=no-new-privileges", "--pids-limit=64", "--memory=256m",
        "--user=65534:65534", "--tmpfs",
        "/project/probe:rw,nosuid,nodev,size=16m,uid=65534,gid=65534,mode=0700",
        selected_image,
    ]
    try:
        try:
            status, output = run_bounded(command)
        except ProbeRefused as error:
            (args.output / "probe.log").write_bytes(error.output)
            raise
    finally:
        # Killing the Docker client does not stop its container. Only remove
        # the exact ID created by this invocation in its fresh private output.
        if container_id.exists():
            selected_container = container_id.read_text().strip()
            if not re.fullmatch(r"[0-9a-f]{64}", selected_container):
                raise RuntimeError("probe returned an invalid container identity")
            subprocess.run(["docker", "rm", "-f", selected_container], timeout=15,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    (args.output / "probe.log").write_bytes(output)
    diagnostic = output.decode(errors="replace")
    print(diagnostic, end="", flush=True)
    expected_edit = hashlib.sha256(b"alpha\ngamma\n").hexdigest()
    if status or f"{expected_edit}  changed.txt" not in diagnostic:
        raise RuntimeError(f"authoring probe failed ({status}); see {args.output / 'probe.log'}")
    report = {
        "inventory_sha256": args.inventory_sha256,
        "image_id": selected_image, "exit_code": status,
        "changed_file_sha256": expected_edit, "execution_user": 65534,
        "network": "none", "host_libraries": False, "descriptor_search": True,
        "ryeos_binding_qualified": False, "hosted_model_qualified": False,
    }
    (args.output / "qualification.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
