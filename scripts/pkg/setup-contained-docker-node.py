#!/usr/bin/env python3
"""Prepare and start one administrator-owned contained Docker node.

Requires the installed ryeos-contained runtime and a locally available immutable
image. Does not pull/build images, migrate nodes, or replace existing containers.
"""
import argparse
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

SPEC = importlib.util.spec_from_file_location("host_install", Path(__file__).with_name("install-contained-docker-runtime.py"))
HOST = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HOST)

ENTRY = ["/usr/bin/tini", "--", "/usr/local/bin/contained-workflow-entrypoint"]
LABEL = "io.ryeos.contained-setup"


def publisher_trust_args(enabled):
    # The root source publisher signs this build's node profiles and bundles.
    # This is an explicit operator decision, never inferred from image labels.
    return ["--trust-file", "/opt/ryeos/.ai/PUBLISHER_TRUST.toml"] if enabled else []


def validate_image(image):
    config = image.get("Config") or {}
    labels = config.get("Labels") or {}
    expected = {"io.ryeos.image": "contained-workflow", "io.ryeos.required-node-profile": "contained-workflow",
                "io.ryeos.controller-uid": "10001", "io.ryeos.controller-gid": "10001"}
    if any(labels.get(key) != value for key, value in expected.items()):
        raise ValueError("image does not declare the contained-workflow contract")
    if config.get("Entrypoint") != ENTRY or config.get("Cmd"):
        raise ValueError("image does not use the fixed contained bootstrap")
    if image.get("Os") != "linux" or image.get("Architecture") != "amd64":
        raise ValueError("contained setup currently supports linux/amd64 only")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image.get("Id", "")):
        raise ValueError("image inspection returned no immutable image ID")
    if not re.fullmatch(r"[0-9a-f]{40}", labels.get("org.opencontainers.image.revision", "")):
        raise ValueError("image is missing its exact source revision")


def container_matches(container, record, state):
    host = container.get("HostConfig") or {}
    mounts = {entry.get("Destination"): entry for entry in container.get("Mounts", [])}
    config = container.get("Config") or {}
    return (container.get("Image") == record["image"]
            and host.get("Runtime") == "ryeos-contained"
            and host.get("Privileged") is False
            and host.get("PidMode", "") == ""
            and host.get("NetworkMode") in ("default", "bridge")
            and config.get("User") == "0:0"
            and config.get("Entrypoint") == ENTRY
            and not config.get("Cmd")
            and (config.get("Labels") or {}).get(LABEL) == str(state)
            and host.get("PortBindings") == {"8000/tcp": [{"HostIp": "127.0.0.1", "HostPort": str(record["port"])}]}
            and all(mounts.get(destination, {}).get("Type") == "bind"
                    and mounts[destination].get("Source") == str(state / name)
                    and mounts[destination].get("RW") is True
                    for destination, name in [("/data/app", "app"), ("/projects", "projects")])
            and len(mounts) == 2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", required=True)
    parser.add_argument("--image", required=True, help="local sha256 image ID or repository@sha256 digest")
    parser.add_argument("--port", required=True, type=int, help="host loopback port")
    parser.add_argument("--trust-source-publisher", action="store_true",
                        help="explicitly trust the pinned image's source publisher (local development only)")
    parser.add_argument("--confirm", action="store_true", required=True)
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise ValueError("setup requires administrator authority")
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", args.name) or not 1024 <= args.port <= 65535:
        raise ValueError("use a simple lowercase node name and an unprivileged host port")
    if not re.fullmatch(r"(?:[a-zA-Z0-9./:_-]+@)?sha256:[0-9a-f]{64}", args.image):
        raise ValueError("--image must be immutable; mutable tags are not accepted")
    docker = Path("/usr/bin/docker")
    HOST.require_executable(docker)

    def run(*arguments, capture=True):
        # Bind this host installer to the local daemon rather than ambient
        # DOCKER_HOST/context settings which could refer to another machine.
        try:
            result = subprocess.run([str(docker), "--host", "unix:///var/run/docker.sock", *arguments],
                                    check=True, text=True, capture_output=capture)
        except subprocess.CalledProcessError as error:
            if error.stderr:
                print(error.stderr.rstrip(), file=sys.stderr)
            raise
        return result.stdout if capture else None

    info = json.loads(run("info", "--format", "{{json .}}"))
    if "ryeos-contained" not in info.get("Runtimes", {}) or info.get("CgroupVersion") != "2":
        raise ValueError("local Docker needs cgroup v2 and the activated ryeos-contained runtime")
    image = json.loads(run("image", "inspect", args.image))[0]
    validate_image(image)
    state = Path("/var/lib/ryeos/contained-nodes") / args.name
    parent = HOST.protected_directory(state, create=True)
    try:
        os.fchmod(parent, 0o700)
        fcntl.flock(parent, fcntl.LOCK_EX)
        contract = {"schema": 1, "name": args.name, "image": image["Id"], "port": args.port}
        if args.trust_source_publisher:
            contract["trust_source_publisher"] = True
        try:
            record = json.loads(HOST.read_regular("setup.json", parent, True, 16384), object_pairs_hook=HOST.unique_object)
        except FileNotFoundError:
            record = None
        if record is None:
            if os.listdir(parent):
                raise ValueError("setup directory is not empty and has no retained setup record")
            record = dict(contract, directories={})
            for name in ["app", "projects"]:
                os.mkdir(name, 0o700, dir_fd=parent)
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
                try:
                    os.fchown(child, 10001, 10001)
                    metadata = os.fstat(child)
                    record["directories"][name] = [metadata.st_dev, metadata.st_ino]
                    os.fsync(child)
                finally:
                    os.close(child)
            fd = os.open("setup.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=parent)
            with os.fdopen(fd, "w") as stream:
                json.dump(record, stream, sort_keys=True)
                stream.flush()
                os.fsync(stream.fileno())
            os.fsync(parent)
        if (any(record.get(key) != value for key, value in contract.items())
                or bool(record.get("trust_source_publisher", False)) != args.trust_source_publisher):
            raise ValueError("existing node setup differs; image/port changes require explicit stopped-node migration")
        for name in ["app", "projects"]:
            child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
            try:
                metadata = os.fstat(child)
                if ([metadata.st_dev, metadata.st_ino] != record["directories"][name]
                        or (metadata.st_uid, metadata.st_gid) != (10001, 10001)
                        or metadata.st_mode & 0o077):
                    raise ValueError(f"retained {name} directory identity/ownership changed")
            finally:
                os.close(child)

        ids = run("ps", "-aq", "--filter", f"name=^/{args.name}$").split()
        if ids:
            container = json.loads(run("inspect", ids[0]))[0]
            if len(ids) != 1 or not container_matches(container, record, state):
                raise ValueError("existing container does not match this exact setup")
            if not container["State"]["Running"]:
                run("start", args.name, capture=False)
        else:
            mount = f"type=bind,src={state / 'app'},dst=/data/app"
            profile_args = [] if os.path.lexists(state / "app/.ai/node/policies") else ["--node-profile", "contained-workflow"]
            # Offline initialization uses the exact image, fixed non-root account
            # and signed contained profile. Validators need nested namespaces;
            # process-scope admission belongs to the subsequent hooked daemon.
            run("run", "--rm", "--runtime", "runc", "--network", "none", "--user", "10001:10001",
                "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined",
                "--mount", mount, "--entrypoint", "/usr/local/bin/ryeos", image["Id"],
                "init", "--non-interactive", "--app-root", "/data/app", "--source", "/opt/ryeos",
                "--bind", "[::]:8000", *profile_args,
                *publisher_trust_args(args.trust_source_publisher), capture=False)
            run("create", "--name", args.name, "--runtime", "ryeos-contained", "--user", "0:0",
                "--label", f"{LABEL}={state}", "--publish", f"127.0.0.1:{args.port}:8000",
                "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined",
                "--mount", mount, "--mount", f"type=bind,src={state / 'projects'},dst=/projects",
                image["Id"], capture=False)
            run("start", args.name, capture=False)
    finally:
        os.close(parent)
    deadline = time.monotonic() + 60
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    while True:
        try:
            with opener.open(f"http://127.0.0.1:{args.port}/_ryeos/ready", timeout=2) as response:
                if response.status == 200:
                    break
        except (OSError, urllib.error.URLError):
            pass
        status = json.loads(run("inspect", args.name))[0]["State"]
        if not status["Running"] or time.monotonic() >= deadline:
            raise ValueError(f"contained node did not become ready; retain state and inspect Docker logs for {args.name}")
        time.sleep(1)
    print(f"Container ready: {args.name}; endpoint http://127.0.0.1:{args.port}")
    print("Readiness is not full installed containment qualification; retain lifecycle and worker evidence separately.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        sys.exit(f"contained Docker node setup failed: {error}")
