# ryeos:signed:2026-09-07T07:41:15Z:2bd857a457d85918294e40e80ad8fd1097d67c2ed83c4833d5f780aee2fb8ddd:skxk0OJ9WjZHuWX4TKT5WZ+6+3w5v0Q3x6iUDR/1XKyO+iNwbBf2Lw7Lfqlg+NAjujN6McEwZe0oVIZOMvK+Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Canonical locked-registry selection and assembly, shared with first bootstrap.

This module never invokes a transport, Cargo, package installer or node API.
The caller supplies exact input bytes; the bootstrap entry owns public network
acquisition and the admitted Tool instead reads an exact bound input tree.
Historical acquisition testimony is not copied into a new production claim.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import stat
import tempfile
import tomllib
import urllib.parse

SCHEMA = "ryeos.development.registry-acquisition.v1"
RECEIPT_SCHEMA = "ryeos.development.registry-inputs.v1"
NAME = re.compile(r"[A-Za-z0-9_-]+\Z")
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.+-]+)?\Z")
DIGEST = re.compile(r"[0-9a-f]{64}\Z")


def bounded_file(path, maximum):
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f"input is not a regular file: {path}")
    with path.open("rb") as source:
        data = source.read(maximum + 1)
    if len(data) > maximum:
        raise ValueError(f"input exceeds its bound: {path}")
    return data


def index_member(name):
    name = name.lower()
    if len(name) < 3:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


def validate_config(config):
    keys = {"category", "name", "version", "schema", "registry_source", "index_base",
            "archive_template", "allowed_https_hosts", "limits"}
    if (not isinstance(config, dict) or set(config) != keys or config["schema"] != SCHEMA or
            config["category"] != "development/ryeos" or config["name"] != "registry-acquisition" or
            config["version"] != "1.0.0"):
        raise ValueError("unsupported or incomplete acquisition declaration")
    limits = config["limits"]
    required = {"max_packages", "max_input_file_bytes", "max_index_bytes", "max_archive_bytes",
                "max_total_download_bytes", "request_timeout_seconds", "total_timeout_seconds"}
    if not isinstance(limits, dict) or set(limits) != required:
        raise ValueError("incomplete acquisition limits")
    if any(type(value) is not int or value <= 0 for value in limits.values()):
        raise ValueError("acquisition limits must be positive integers")
    hosts = config["allowed_https_hosts"]
    if not isinstance(hosts, list) or not hosts or any(
        not isinstance(host, str) or not re.fullmatch(r"[a-z0-9.-]+", host) for host in hosts
    ):
        raise ValueError("invalid HTTPS host selection")
    for url in (config["index_base"], config["archive_template"].format(name="crate", version="1.0.0")):
        validate_url(url, hosts)


def validate_url(url, hosts):
    parsed = urllib.parse.urlsplit(url)
    if (parsed.scheme != "https" or parsed.hostname not in hosts or parsed.port not in (None, 443)
            or parsed.username or parsed.password or parsed.fragment):
        raise ValueError("acquisition URL is outside the explicit HTTPS selection")


def selected_packages(lock, config):
    packages = []
    seen = set()
    for package in lock["package"]:
        if "source" not in package:
            continue  # Workspace/path sources remain in the exact project snapshot.
        name, version = package["name"], package["version"]
        checksum = package.get("checksum", "")
        if (package["source"] != config["registry_source"] or not NAME.fullmatch(name)
                or not VERSION.fullmatch(version) or not DIGEST.fullmatch(checksum)):
            raise ValueError("lock contains an unsupported or unpinned registry coordinate")
        if (name, version) in seen:
            raise ValueError("duplicate locked registry coordinate")
        seen.add((name, version))
        packages.append((name, version, checksum))
    if not packages or len(packages) > config["limits"]["max_packages"]:
        raise ValueError("locked registry inventory is empty or exceeds its bound")
    return sorted(packages)


def put(root, relative, data):
    path = PurePosixPath(relative)
    if path.is_absolute() or path.as_posix() != relative or any(p in (".", "..") for p in path.parts):
        raise ValueError("noncanonical output member")
    target = root.joinpath(*path.parts)
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open("xb") as output:
        output.write(data)
    target.chmod(0o644)


def assemble(lock_bytes, config, output, fetch, *, source_kind):
    validate_config(config)
    if source_kind not in {"public_https_acquisition", "admitted_retained_inputs"}:
        raise ValueError("unknown registry input provenance")
    if len(lock_bytes) > config["limits"]["max_input_file_bytes"]:
        raise ValueError("lock input exceeds its bound")
    packages = selected_packages(tomllib.loads(lock_bytes.decode()), config)
    if output.exists() or output.is_symlink():
        raise ValueError("output already exists; refusing overwrite")
    records, sources = {}, {}
    consumed = 0

    def selected_bytes(url, maximum):
        nonlocal consumed
        data = fetch(url, maximum)
        if not isinstance(data, bytes) or len(data) > maximum:
            raise ValueError("registry input exceeds its bound")
        consumed += len(data)
        if consumed > config["limits"]["max_total_download_bytes"]:
            raise ValueError("aggregate registry input exceeds its bound")
        return data
    current_name, selected_lines = None, []
    # Stage beside the final output. A failure leaves no partially published
    # registry; there is no cross-filesystem directory rename or node mutation.
    with tempfile.TemporaryDirectory(prefix=".registry-input-", dir=output.parent) as scratch:
        stage = Path(scratch) / "registry"
        stage.mkdir()
        for name, version, checksum in packages:
            if name != current_name:
                if current_name is not None:
                    put(stage, "index/" + index_member(current_name), b"\n".join(selected_lines) + b"\n")
                current_name, selected_lines = name, []
                url = config["index_base"].rstrip("/") + "/" + index_member(name)
                raw = selected_bytes(url, config["limits"]["max_index_bytes"])
                entries = {}
                for line in raw.splitlines():
                    entry = json.loads(line)
                    if (not isinstance(entry, dict) or entry.get("name") != name or
                            not isinstance(entry.get("vers"), str) or entry["vers"] in entries):
                        raise ValueError("registry index has contradictory package coordinates")
                    entries[entry["vers"]] = (entry, line)
                records = entries
                sources[url] = hashlib.sha256(raw).hexdigest()
            entry, line = records.get(version, ({}, b""))
            if entry.get("cksum") != checksum:
                raise ValueError("registry index does not match the exact locked checksum")
            selected_lines.append(line)
            url = config["archive_template"].format(name=name, version=version)
            archive = selected_bytes(url, config["limits"]["max_archive_bytes"])
            if hashlib.sha256(archive).hexdigest() != checksum:
                raise ValueError("crate archive does not match Cargo.lock")
            put(stage, f"{name}-{version}.crate", archive)
            sources[url] = checksum
        put(stage, "index/" + index_member(current_name), b"\n".join(selected_lines) + b"\n")
        put(stage, "registry-inputs.json", (json.dumps({
            "schema": RECEIPT_SCHEMA,
            "source_kind": source_kind,
            "lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
            "declaration_sha256": hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest(),
            "source_sha256": sources,
            "packages": [{"name": n, "version": v, "sha256": c} for n, v, c in packages],
        }, sort_keys=True, indent=2) + "\n").encode())
        # This operator-owned destination must be exclusively held during
        # acquisition; never invoke it concurrently for one output coordinate.
        if output.exists() or output.is_symlink():
            raise ValueError("output appeared during acquisition")
        stage.rename(output)


def ordinary_member(root, relative, *, directory=False):
    """Select regular bytes inside the already-admitted immutable input root.

    No mutable node/CAS path is reopened. These paths are private Tool inputs;
    RyeOS/Lillux retain import, mount and process authority, not this helper.
    """
    path = PurePosixPath(relative)
    if (not relative or not path.parts or path.is_absolute() or path.as_posix() != relative or
            any(part in (".", "..") for part in path.parts)):
        raise ValueError("noncanonical input member")
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("input root is not an ordinary directory")
    selected = root
    for index, part in enumerate(path.parts):
        selected = selected / part
        mode = selected.lstat().st_mode
        expected_directory = directory or index < len(path.parts) - 1
        if not (stat.S_ISDIR(mode) if expected_directory else stat.S_ISREG(mode)):
            raise ValueError("input member is a link or special file")
    return selected


class RetainedInputs:
    """Finite read map over an exact bound public-registry input tree; no I/O transport."""

    def __init__(self, root, lock_bytes, config):
        validate_config(config)
        self.root = root
        self.members = {}
        for name, version, _ in selected_packages(tomllib.loads(lock_bytes.decode()), config):
            self.members[config["index_base"].rstrip("/") + "/" + index_member(name)] = "index/" + index_member(name)
            self.members[config["archive_template"].format(name=name, version=version)] = f"{name}-{version}.crate"

    def __call__(self, url, maximum):
        if url not in self.members:
            raise ValueError("unselected retained registry input")
        return bounded_file(ordinary_member(self.root, self.members[url]), maximum)
