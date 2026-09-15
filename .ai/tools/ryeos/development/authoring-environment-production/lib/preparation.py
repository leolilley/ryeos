# ryeos:signed:2026-09-06T07:00:11Z:51406f3257b7a995b224bb8ad344c55b85a825d4274ece7b11f43d12f692a327:a/zU+WhE57J3zMHHDSgGO3SqHstkxUM8TeXnlxsbZsqb5SRuk1DfbzhmaUshm0B+p1XkdpHutnjk2/B92sdrBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Select admitted archives into the existing, independently authored input contract.

No downloads, container access, host tool discovery, signing or publication.
Historical utility archives remain exact inputs, not evidence of a new build.
"""

from __future__ import annotations

import hashlib
import io
import json
from pathlib import Path
import shutil
import stat
import sys

from archives import read_members
from production import (canonical_json, input_inventory, inventory,
                        ordinary_member, receipt, relative, validate_config)
from utilities import validate_sources

RAW_ROOT = Path("/ryeos/realizations/authoring-source-inputs")
INPUT_CONFIG = "development/ryeos/authoring-environment-inputs.yaml"
SOURCE_CONFIG = "development/ryeos/authoring-utility-sources.yaml"
OUTPUT = "products/authoring-prepared-inputs"


def selection(config: dict, source_config: dict) -> tuple:
    validate_config(config)
    validate_sources(source_config)
    provenance = config["provenance"]
    if (source_config["input_image"] != provenance["bootstrap_image"] or
            source_config["source_date_epoch"] != config["source_date_epoch"]):
        raise ValueError("source and assembly contracts disagree")
    archives = provenance["utility_bootstrap_archives"]
    sources = [name for name in archives if name.endswith("-sources.tar.gz")]
    binaries = [name for name in archives if name.endswith(".tar.gz") and name not in sources]
    if len(archives) != 2 or len(sources) != 1 or len(binaries) != 1:
        raise ValueError("exact utility binary/source pair is required")
    raw, copies = {}, {}

    def select(name: str, identity: dict, target: str | None = None) -> None:
        relative(name)
        if name in raw:
            raise ValueError("duplicate raw input selection")
        raw[name] = identity
        if target is not None:
            relative(target)
            if target not in config["inputs"] or target in copies.values():
                raise ValueError("raw member selects an absent or duplicate output")
            copies[name] = target

    for name, identity in archives.items():
        if len(relative(name).parts) != 1 or set(identity) != {"bytes", "sha256"}:
            raise ValueError("invalid utility archive coordinate")
        select("bootstrap/" + name, {**identity, "mode": 0o644},
               "sources/" + name if name in sources else None)
    # The selected package's digest fixes its bytes; its bounded size is checked
    # before hashing. Preserve the existing assembly contract unchanged.
    select("workload/selected-package.tar.gz", {
        "bytes": None, "sha256": provenance["workload_package_sha256"], "mode": 0o644})
    upstream_targets = set()
    for source in provenance["source_and_notice_inputs"]:
        if len(relative(source["archive"]).parts) != 1:
            raise ValueError("upstream archive must have a single name")
        select("upstreams/" + source["archive"], {
            "bytes": source["bytes"], "sha256": source["sha256"], "mode": 0o644}, source["target"])
        upstream_targets.add(source["target"])
    for name, identity in config["inputs"].items():
        if name.startswith(("elf/", "notices/")) and name not in upstream_targets:
            select(name, identity, name)
    return raw, copies, binaries[0], sources[0]


def checked_raw(root: Path, expected: dict) -> None:
    # Reuse production's regular-file, symlink, entry, depth and total bounds.
    # Raw materializations carry portable manifest modes, not mutable cache
    # write bits. Output receipts continue to retain physical modes.
    observed = input_inventory(root)
    if observed.keys() != expected.keys():
        raise ValueError("raw input inventory differs from its exact selection")
    for name, identity in expected.items():
        actual = observed[name]
        if (actual["sha256"] != identity["sha256"] or actual["mode"] != identity["mode"] or
                (identity["bytes"] is not None and actual["bytes"] != identity["bytes"])):
            raise ValueError(f"raw input identity mismatch: {name}")


def prepare(root: Path, destination: Path, config: dict, source_config: dict) -> dict:
    expected, copies, binary_name, source_name = selection(config, source_config)
    checked_raw(root, expected)
    # Partial work is kept in a private staging directory, never under the
    # advertised completed path. Reuse refuses rather than replacing evidence.
    destination.mkdir(mode=0o700, exist_ok=False)
    tree = destination / "tree"
    tree.mkdir(mode=0o700)

    def put(name: str, data: bytes | Path) -> None:
        identity = config["inputs"].get(name)
        if identity is None:
            raise ValueError("archive selects bytes outside the authored input contract")
        target = tree.joinpath(*relative(name).parts)
        target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
        with target.open("xb") as output:
            if isinstance(data, Path):
                with data.open("rb") as stream:
                    shutil.copyfileobj(stream, output, length=1024 * 1024)
            else:
                if (len(data) != identity["bytes"] or
                        hashlib.sha256(data).hexdigest() != identity["sha256"]):
                    raise ValueError(f"selected archive member identity mismatch: {name}")
                output.write(data)
        target.chmod(identity["mode"])

    for name, target in copies.items():
        put(target, ordinary_member(root, name))
    binary_members = {"bin/" + name for source in source_config["sources"]
                      for name in source.get("programs", {})}
    for name, data in read_members(root / "bootstrap" / binary_name, binary_members).items():
        put("utilities/" + name, data)
    workload_members = config["provenance"]["workload_members"]
    for name, data in read_members(root / "workload/selected-package.tar.gz", set(workload_members)).items():
        if hashlib.sha256(data).hexdigest() != workload_members[name]:
            raise ValueError("selected workload resource identity mismatch")
        put("utilities/bin/" + relative(name).name, data)
    sources = source_config["sources"]
    nested = read_members(root / "bootstrap" / source_name, {source["archive"] for source in sources})
    for source in sources:
        data = nested.pop(source["archive"])
        if len(data) != source["bytes"] or hashlib.sha256(data).hexdigest() != source["sha256"]:
            raise ValueError("corresponding source archive identity mismatch")
        names = {source["directory"] + "/" + notice for notice in source["licenses"]}
        for name, content in read_members(io.BytesIO(data), names).items():
            notice = name.removeprefix(source["directory"] + "/")
            put("utilities/licenses/" + source["name"] + "/" + notice, content)
    # These are newly produced files, not immutable input materializations.
    # Keep their physical permission bits exact before reporting completion.
    if inventory(tree) != config["inputs"]:
        raise ValueError("prepared output bytes or modes differ from the authored inventory")
    # This is a copy of the admitted contract, not an output-generated signature.
    contract = destination / "input-contract.json"
    with contract.open("xb") as output:
        output.write(canonical_json(config))
    contract.chmod(0o644)
    return {**receipt(tree),
            "input_contract_sha256": hashlib.sha256(canonical_json(config)).hexdigest()}


def run_preparation() -> None:
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute() or not stat.S_ISDIR(project.lstat().st_mode):
        raise ValueError("invalid admitted project context")
    raw = sys.stdin.buffer.read(262145)
    if len(raw) > 262144:
        raise ValueError("preparation parameters exceed their bound")
    request = json.loads(raw)
    resolved = request.get("resolved_config") if isinstance(request, dict) else None
    if not isinstance(resolved, dict) or set(resolved) != {INPUT_CONFIG, SOURCE_CONFIG}:
        raise ValueError("missing exact resolved preparation contracts")
    parent = project / "products"
    if not parent.exists():
        parent.mkdir(mode=0o700)
    ordinary_member(project, "products", directory=True)
    destination = project / OUTPUT
    if destination.exists() or destination.is_symlink():
        raise ValueError("prepared output already exists; use a fresh production workspace")
    staging = parent / "authoring-preparation"
    result = prepare(RAW_ROOT, staging, resolved[INPUT_CONFIG], resolved[SOURCE_CONFIG])
    if destination.exists() or destination.is_symlink():
        raise ValueError("prepared output already exists; use a fresh production workspace")
    staging.rename(destination)
    print(json.dumps({"ok": True, "operation": "prepare", "output_path": OUTPUT + "/tree",
                      **result, "binding_published": False}, sort_keys=True))
