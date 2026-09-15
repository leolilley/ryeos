# ryeos:signed:2026-09-07T06:32:12Z:e2de6015e812d20097ea4a7cba118aee4ed136d0f3167025e523013f23c9828a:Rv5LOJMxsyQvWJkZYlu/jVOskNOxsItpl8DgVg7KPYIK+rGxO0CpTVsTGWAlakVD3zQgpz3GtilAhgX9unqYDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Offline assembly of the exact shell/Make support consumed by utilities.py.

Acquisition, import, binding and isolation remain with their existing owners.
This recipe consumes only an admitted finite input tree, reuses the authoring
ELF transformation/verification owner, and writes fresh private project output.
An assembled support tree is not evidence that utility compilation succeeded.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import sys

from production import (ELF_TOOLS, MAX_ENTRIES, MAX_TOTAL_BYTES, ElfTools, canonical_json, check_closure,
                        copy_selected_files, input_inventory, ordinary_member,
                        receipt, relative, relocate_elf)
from utilities import REQUIRED_SUPPORT_COMMANDS, SUPPORT, checked_support


SCHEMA = "ryeos.development.authoring-build-support-inputs.v1"
INPUT_ROOT = Path("/ryeos/realizations/build-support-inputs")
OUTPUT = "products/authoring-build-support"


def validate_config(config: dict) -> None:
    if (not isinstance(config, dict) or set(config) != {
            "category", "name", "version", "schema", "inputs", "files",
            "source_date_epoch", "provenance"} or config["schema"] != SCHEMA or
            config["category"] != "development/ryeos" or
            config["name"] != "authoring-build-support-inputs" or config["version"] != "1.0.0"):
        raise ValueError("unsupported build-support input contract")
    if type(config["source_date_epoch"]) is not int or not 0 <= config["source_date_epoch"] <= 4_102_444_800:
        raise ValueError("invalid source normalization epoch")
    if not isinstance(config["inputs"], dict) or not config["inputs"]:
        raise ValueError("missing exact build-support inputs")
    if not isinstance(config["files"], dict) or not 1 <= len(config["files"]) <= MAX_ENTRIES:
        raise ValueError("missing build-support output selection")
    for target, source in config["files"].items():
        parts = relative(target).parts
        relative(source)
        if (len(parts) < 2 or parts[0] not in {"bin", "lib", "elf", "licenses"} or
                (parts[0] in {"bin", "lib"} and len(parts) != 2) or source not in config["inputs"]):
            raise ValueError("invalid build-support output member")
        if any(parent.as_posix() in config["files"] for parent in relative(target).parents):
            raise ValueError("build-support output file/directory collision")
    if set(config["files"].values()) != set(config["inputs"]):
        raise ValueError("build support must retain the complete selected input inventory")
    commands = {name.removeprefix("bin/") for name in config["files"] if name.startswith("bin/")}
    if not REQUIRED_SUPPORT_COMMANDS <= commands:
        raise ValueError("build-support shell/Make/helper inventory is incomplete")
    required = set(ELF_TOOLS.values()) | {"lib/ld-linux-x86-64.so.2", "licenses/GPL-2",
                                       "licenses/GPL-3", "licenses/LGPL-2.1"}
    if not required <= config["files"].keys():
        raise ValueError("build-support interpreter, inspection tools or notices are missing")
    if not isinstance(config["provenance"], dict) or not config["provenance"]:
        raise ValueError("build-support input provenance is required")


def assemble_support(inputs: Path, destination: Path, config: dict, *, tools=None) -> dict:
    validate_config(config)
    # Existing inventory applies the bounded no-symlink, portable-mode rules.
    # No second manifest or digest implementation is authoritative here.
    if input_inventory(inputs) != config["inputs"]:
        raise ValueError("build-support inputs differ from their exact inventory")
    if sum(config["inputs"][source]["bytes"] for source in config["files"].values()) > MAX_TOTAL_BYTES:
        raise ValueError("selected build-support output exceeds its byte bound")
    if destination.exists() or destination.is_symlink():
        raise ValueError("build-support destination already exists")
    destination.mkdir(mode=0o700)
    copy_selected_files(inputs, destination, config["files"], config["inputs"])
    tools = tools if tools is not None else ElfTools(inputs)
    transformations = {}
    for member in sorted(config["files"]):
        if not member.startswith(("bin/", "lib/")) or member == "lib/ld-linux-x86-64.so.2":
            continue
        path = ordinary_member(destination, member)
        facts = tools.facts(path)
        if facts["interpreter"] or facts["needed"]:
            transformations[member] = relocate_elf(path, tools, str(SUPPORT))
    # Use the same closure verifier as the authoring baseline, with this
    # recipe's exact mount, not host paths or an invented runtime-discovery map.
    check_closure(destination, {"environment/" + p: p for p in config["files"]}, tools,
                  {"environment/" + p for p in transformations}, runtime_root=str(SUPPORT))
    provenance = {"schema": 1, "runtime_mount": str(SUPPORT),
                  "input_contract_sha256": hashlib.sha256(canonical_json(config)).hexdigest(),
                  "transformations": transformations, "sources": config["provenance"]}
    record = destination / "RYEOS-BUILD-SUPPORT.json"
    record.write_bytes(canonical_json(provenance))
    record.chmod(0o644)
    for path in sorted(destination.rglob("*")):
        if path.is_dir():
            path.chmod(0o755)
        os.utime(path, (config["source_date_epoch"], config["source_date_epoch"]))
    # Feed the existing utility consumer; do not invent another support format.
    contract = {"inputs": input_inventory(destination),
                "commands": {p.removeprefix("bin/"): p for p in config["files"] if p.startswith("bin/")},
                "notices": {p: p for p in config["files"] if p.startswith("licenses/")}}
    checked_support(destination, contract)
    return {"support_contract": contract, **receipt(destination)}


def run_support_assembly() -> None:
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute() or project.is_symlink() or not project.is_dir():
        raise ValueError("invalid admitted project context")
    raw = sys.stdin.buffer.read(262145)
    if len(raw) > 262144:
        raise ValueError("build-support parameters exceed their bound")
    config = json.loads(raw)["resolved_config"]
    parent = project / "products"
    if not parent.exists():
        parent.mkdir(mode=0o700)
    ordinary_member(project, "products", directory=True)
    result = assemble_support(INPUT_ROOT, project / OUTPUT, config)
    print(json.dumps({"ok": True, "output_path": OUTPUT, "binding_published": False,
                      **result}, sort_keys=True))
