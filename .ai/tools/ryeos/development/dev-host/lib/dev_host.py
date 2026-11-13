# ryeos:signed:2026-11-12T23:59:41Z:79d5156cda1e2517a21ac8c7cec74e1b3e64e1d43a2e609d29783514837b58ee:9ZZssU5fi6aERJ6rUgIbexzrNKWEloeVriwqVYF1C7oCqstW5fnB85w9pxMRSLf3Y9K6vZrzbJv+FsR4k68qDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Shared helpers for the externally invoked development host tools.

These tools run on a developer's machine, outside RyeOS: they read host
processes, disks, installed binaries and app roots. They are not admitted
Tools and grant no RyeOS authority. Site-specific values (paths, process
patterns, floors) come from signed development Configs under
`.ai/config/development/ryeos/`; a developer may pass a local copy with
`--config-file`. A Config body is JSON; its signature envelope line is skipped,
not verified, exactly as the repository-validation CI entry does.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


REPOSITORY_ROOT = Path(__file__).resolve().parents[6]
CONFIG_DIR = Path(".ai/config/development/ryeos")
MAX_CONFIG_BYTES = 262144
_VARIABLE = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^${}]*))?\}")


class ToolError(Exception):
    """A refusal the tool reports in one line and exits 2 for."""


def config_path(root: Path, name: str) -> Path:
    return root / CONFIG_DIR / f"{name}.yaml"


def load_config(path: Path, name: str, schema: str, keys: set[str]) -> dict:
    """Read one development Config with exactly the expected top-level keys."""
    try:
        with open(path, "rb") as source:
            raw = source.read(MAX_CONFIG_BYTES + 1)
    except OSError as error:
        raise ToolError(f"cannot read config {path}: {error.strerror}") from error
    if len(raw) > MAX_CONFIG_BYTES:
        raise ToolError(f"config {path} exceeds {MAX_CONFIG_BYTES} bytes")
    if raw.startswith(b"# ryeos:signed:"):
        raw = raw.split(b"\n", 1)[1] if b"\n" in raw else b""
    try:
        config = json.loads(raw)
    except ValueError as error:
        raise ToolError(f"config {path} is not JSON: {error}") from error
    expected = {"category", "name", "version", "schema", "description"} | keys
    if not isinstance(config, dict) or set(config) != expected:
        found = set(config) if isinstance(config, dict) else set()
        raise ToolError(f"config {path} keys differ: missing {sorted(expected - found)}, "
                        f"unexpected {sorted(found - expected)}")
    if (config["category"], config["name"], config["schema"]) != ("development/ryeos", name, schema):
        raise ToolError(f"config {path} is not development/ryeos/{name} with schema {schema}")
    return config


def expand(value: str, extra: dict[str, str] | None = None) -> str:
    """Expand `${NAME}` and `${NAME:-default}` from `extra`, then the environment.

    Innermost references expand first, so a default may itself hold a
    reference. An unset name with no default is a refusal, never empty text.
    """
    if not isinstance(value, str):
        raise ToolError(f"expected a string config value, got {value!r}")
    lookup = dict(os.environ)
    lookup.update(extra or {})

    def replace(match: re.Match) -> str:
        name, default = match.group(1), match.group(2)
        if lookup.get(name):
            return lookup[name]
        if default is not None:
            return default
        raise ToolError(f"config value {value!r} needs ${{{name}}}, which is not set")

    previous = None
    while previous != value:
        previous, value = value, _VARIABLE.sub(replace, value)
    return value


def expand_path(value: str, extra: dict[str, str] | None = None) -> Path:
    path = Path(expand(value, extra)).expanduser()
    if not path.is_absolute():
        raise ToolError(f"config path {value!r} does not expand to an absolute path ({path})")
    return path


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        while chunk := source.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def _bundle_sets(root: Path, call: str) -> list[str]:
    script = root / "scripts/pkg/bundle-sets.sh"
    result = subprocess.run(["bash", "-c", 'source "$1"; ' + call, "bundle-sets", str(script)],
                            capture_output=True, text=True, cwd=root)
    if result.returncode != 0:
        raise ToolError(f"bundle-sets.sh: {call} refused: {result.stderr.strip() or 'unknown name'}")
    return result.stdout.split()


def profile_bundle_set(root: Path, profile: str) -> str:
    _safe_name(profile)
    return _bundle_sets(root, f"ryeos_node_init_profile_bundle_set {profile}")[0]


def bundle_set_names(root: Path, set_name: str) -> list[str]:
    _safe_name(set_name)
    return _bundle_sets(root, f"ryeos_bundle_set_names {set_name}")


def daemon_features_for(root: Path, *, set_name: str | None = None,
                        bundles: list[str] | None = None) -> set[str]:
    """The ryeosd features a bundle set or bundle list selects (signed ownership)."""
    selector = ["--set", set_name] if set_name else ["--bundles", ",".join(bundles or [])]
    result = subprocess.run(
        ["python3", "-I", str(root / "scripts/pkg/bundle-features.py"),
         "--repository-root", str(root), *selector, "cargo-features", "ryeosd"],
        capture_output=True, text=True)
    if result.returncode != 0:
        raise ToolError(f"bundle-features.py refused ({result.returncode}): {result.stderr.strip()}")
    features = set()
    for item in result.stdout.strip().split(","):
        if item:
            package, _, feature = item.partition("/")
            if package != "ryeosd" or not feature:
                raise ToolError(f"bundle-features.py printed an unexpected selection {item!r}")
            features.add(feature)
    return features


def build_info(ryeosd: Path, timeout: int = 30) -> dict:
    """`ryeosd build-info --json`: version, revision, profile, compiled_features."""
    if not os.access(ryeosd, os.X_OK):
        raise ToolError(f"{ryeosd} is not an executable file")
    try:
        result = subprocess.run([str(ryeosd), "build-info", "--json"], capture_output=True,
                                text=True, timeout=timeout, stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired as error:
        raise ToolError(f"{ryeosd} build-info did not answer in {timeout}s") from error
    if result.returncode != 0:
        raise ToolError(f"{ryeosd} build-info exited {result.returncode}: {result.stderr.strip()[:300]}")
    try:
        info = json.loads(result.stdout)
    except ValueError as error:
        raise ToolError(f"{ryeosd} build-info printed no JSON (older daemon?)") from error
    if not isinstance(info, dict) or not isinstance(info.get("compiled_features"), list):
        raise ToolError(f"{ryeosd} build-info has no compiled_features list")
    return info


def _safe_name(value: str) -> None:
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,63}", value or ""):
        raise ToolError(f"not a bundle set or profile name: {value!r}")


DEV_NODE_SCHEMA = "ryeos.development.dev-node.v1"
DEV_NODE_KEYS = {"installed_bin_dir", "host_binaries", "app_root", "target_dir", "build_profile",
                 "bundle_target_triple", "node_profile", "host_class", "trust_file", "scratch_root",
                 "init_timeout_seconds", "ready_timeout_seconds", "source_paths", "drift_ignore"}


class DevNode:
    """`config:development/ryeos/dev-node`, expanded for one worktree."""

    def __init__(self, config: dict, worktree: Path):
        base = {"WORKTREE": str(worktree)}
        self.worktree = worktree
        self.installed_bin_dir = expand_path(config["installed_bin_dir"], base)
        self.host_binaries = _names(config["host_binaries"], "host_binaries")
        self.app_root = expand_path(config["app_root"], base)
        self.target_dir = expand_path(config["target_dir"], base)
        self.build_profile = _names([config["build_profile"]], "build_profile")[0]
        self.triple = config["bundle_target_triple"]
        self.node_profile = config["node_profile"]
        self.host_class = config["host_class"]
        self.trust_file = expand_path(config["trust_file"], base)
        self.scratch_root = expand_path(config["scratch_root"], base)
        self.init_timeout = _positive(config["init_timeout_seconds"], "init_timeout_seconds")
        self.ready_timeout = _positive(config["ready_timeout_seconds"], "ready_timeout_seconds")
        self.source_paths = list(config["source_paths"])
        self.drift_ignore = list(config["drift_ignore"])

    def installed(self, binary: str) -> Path:
        return self.installed_bin_dir / binary

    def built(self, binary: str) -> Path:
        return self.target_dir / self.build_profile / binary


def load_dev_node(config_file: Path | None, worktree: Path) -> DevNode:
    path = config_file or config_path(REPOSITORY_ROOT, "dev-node")
    return DevNode(load_config(path, "dev-node", DEV_NODE_SCHEMA, DEV_NODE_KEYS), worktree)


def _names(values, field: str) -> list[str]:
    if (not isinstance(values, list) or not values or
            not all(isinstance(v, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", v) for v in values)):
        raise ToolError(f"{field} must be plain file names")
    return values


def _positive(value, field: str) -> int:
    if type(value) is not int or value <= 0:
        raise ToolError(f"{field} must be a positive integer")
    return value


def worktree_argument(value: str | None) -> Path:
    """The worktree a tool checks: the given directory, else this tool's repository."""
    path = Path(value).resolve() if value else REPOSITORY_ROOT
    if not (path / "Cargo.toml").is_file() or not (path / "bundles").is_dir():
        raise ToolError(f"{path} is not a RyeOS worktree (no Cargo.toml and bundles/)")
    return path
