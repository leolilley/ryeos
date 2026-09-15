# ryeos:signed:2026-09-08T15:46:29Z:1dc367a947028e9ffb5e01e30a535b9301c300adb651810b318977c7b99c072f:C3MXs8q7rss9/n00Eqq1OfoJT4VkrpXDd3B1UJfXraBY4U5FbHIiDV4azm0yxdxhViaxeTrBbNQ1I/AUpqYKCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Finite GNU CPython ELF relocation and closure inspection.

The caller supplies an already selected ``python/`` tree and the admitted
``ElfTools`` implementation.  This module never searches the host, executes
the selected Python, parses ELF bytes, acquires content, or publishes it.
Upstream readelf owns ELF interpretation and the existing ``relocate_elf``
owner performs every mutation.
"""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import re
import stat

from archives import resolve_contained_member
from production import (MAX_FILE_BYTES, MAX_TOTAL_BYTES, canonical_json, relative,
                        relocate_elf, sha256)


INVENTORY_SCHEMA = "ryeos.gnu-python-elf-inventory.v2"
EVIDENCE_SCHEMA = "ryeos.gnu-python-elf-inventory-evidence.v1"
DEFAULT_PYTHON_VERSION = "3.14"
DEFAULT_SOABI = "cpython-314-x86_64-linux-gnu"
LOADER_MEMBER = "lib/ld-linux-x86-64.so.2"
MAX_DEPTH = 32
MAX_RUNTIME_ENTRIES = 10000
MAX_ELF_OBJECTS = 1024
MAX_LINKS = 4096
MAX_NEEDED_PER_OBJECT = 64
MAX_VERSION_NAMES = 4096
MAX_INVENTORY_BYTES = 4 * 1024 * 1024
MAX_LINK_HOPS = 128
SONAME = re.compile(r"[A-Za-z0-9_+.-]+")
VERSION_NAME = re.compile(r"[A-Za-z0-9_+.-]+")


def _runtime_root(value: str) -> str:
    if (not isinstance(value, str) or not value.startswith("/") or
            len(value) > 1024 or "\\" in value or "\0" in value):
        raise ValueError("runtime root must be a bounded canonical absolute path")
    parts = value[1:].split("/")
    if not parts or any(part in ("", ".", "..") or
                        not re.fullmatch(r"[A-Za-z0-9_.+-]+", part)
                        for part in parts):
        raise ValueError("runtime root must be a bounded canonical absolute path")
    return value


def _python_contract(python_version: str, soabi: str) -> tuple[str, str, str]:
    if not re.fullmatch(r"[0-9]+\.[0-9]+", python_version):
        raise ValueError("invalid finite Python version")
    expected_soabi = "cpython-" + python_version.replace(".", "") + "-x86_64-linux-gnu"
    if soabi != expected_soabi:
        raise ValueError("SOABI does not match the selected GNU CPython version")
    return (f"bin/python{python_version}",
            f"lib/libpython{python_version}.so.1.0",
            f"lib/python{python_version}/lib-dynload")


def _validate_raw_link(member: str, target: str) -> None:
    if (not target or target.startswith("/") or "\\" in target or
            "\0" in target or len(target) > 1024):
        raise ValueError(f"unsafe Python symlink target: {member}")
    for part in target.split("/"):
        if part == "":
            raise ValueError(f"noncanonical Python symlink target: {member}")
        if part not in (".", ".."):
            relative(part)


def _scan(python_root: Path) -> tuple[dict[str, dict], dict[str, str], set[str]]:
    python_root = Path(python_root)
    try:
        root_mode = python_root.lstat().st_mode
    except FileNotFoundError as error:
        raise ValueError("Python runtime root is absent") from error
    if not stat.S_ISDIR(root_mode):
        raise ValueError("Python runtime root must be an ordinary directory")
    regulars: dict[str, dict] = {}
    links: dict[str, str] = {}
    directories = {""}
    pending = [(python_root, 0)]
    entries = total = 0
    while pending:
        directory, depth = pending.pop()
        if depth > MAX_DEPTH:
            raise ValueError("Python runtime depth exceeds its bound")
        try:
            children = sorted(os.scandir(directory), key=lambda item: item.name)
        except OSError as error:
            raise ValueError("cannot inspect Python runtime directory") from error
        for child in children:
            entries += 1
            if entries > MAX_RUNTIME_ENTRIES:
                raise ValueError("Python runtime entry count exceeds its bound")
            member = Path(child.path).relative_to(python_root).as_posix()
            relative(member)
            info = child.stat(follow_symlinks=False)
            if stat.S_ISDIR(info.st_mode):
                directories.add(member)
                pending.append((Path(child.path), depth + 1))
            elif stat.S_ISREG(info.st_mode):
                total += info.st_size
                if info.st_size > MAX_FILE_BYTES or total > MAX_TOTAL_BYTES:
                    raise ValueError("Python runtime bytes exceed their bound")
                with open(child.path, "rb") as stream:
                    magic = stream.read(4)
                regulars[member] = {
                    "path": Path(child.path),
                    "bytes": info.st_size,
                    "mode": stat.S_IMODE(info.st_mode),
                    "elf": magic == b"\x7fELF",
                }
            elif stat.S_ISLNK(info.st_mode):
                target = os.readlink(child.path)
                _validate_raw_link(member, target)
                links[member] = target
            else:
                raise ValueError(f"special Python runtime member: {member}")
    return regulars, links, directories


def _resolution_nodes(regulars: dict, links: dict, directories: set[str]) -> dict:
    nodes = {member: ("file", None) for member in regulars}
    nodes.update({member: ("directory", None) for member in directories if member})
    nodes.update({member: ("symlink", target) for member, target in links.items()})
    return nodes


def _resolve_member(member: str, nodes: dict) -> tuple[str, str]:
    resolved, kind = resolve_contained_member(
        nodes, member, selected_root="", max_hops=MAX_LINK_HOPS)
    return resolved, "regular" if kind == "file" else kind


def _parse_header(output: str, member: str) -> str:
    fields = {}
    for line in output.splitlines():
        match = re.match(r"^\s*(Class|Data|Type|Machine):\s*(.+?)\s*$", line)
        if match:
            if match.group(1) in fields:
                raise ValueError(f"duplicate ELF header field: {member}")
            fields[match.group(1)] = match.group(2)
    if set(fields) != {"Class", "Data", "Type", "Machine"}:
        raise ValueError(f"incomplete ELF header: {member}")
    if fields["Class"] != "ELF64" or fields["Data"] != "2's complement, little endian":
        raise ValueError(f"ELF is not little-endian ELF64: {member}")
    if fields["Machine"] != "Advanced Micro Devices X86-64":
        raise ValueError(f"ELF is not x86_64: {member}")
    kind = fields["Type"].split()[0]
    if kind not in ("EXEC", "DYN"):
        raise ValueError(f"ELF has an unsupported type: {member}")
    return kind


def _parse_gnu_stack(output: str, member: str) -> str:
    rows = [line.split() for line in output.splitlines()
            if line.lstrip().startswith("GNU_STACK")]
    if len(rows) != 1 or len(rows[0]) < 7:
        raise ValueError(f"ELF must have one well-formed GNU_STACK header: {member}")
    flags = rows[0][-2]
    if "E" in flags:
        raise ValueError(f"ELF requests an executable stack: {member}")
    if not flags or any(flag not in "RW" for flag in flags):
        raise ValueError(f"ELF GNU_STACK flags are malformed: {member}")
    return "".join(flag.lower() for flag in "RW" if flag in flags)


def _parse_version_info(output: str, member: str) -> tuple[dict[str, list[str]], list[str]]:
    section = None
    provider = None
    required: dict[str, set[str]] = {}
    provided: set[str] = set()
    count = 0
    saw_needs = saw_definitions = False
    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("Version needs section "):
            section, provider, saw_needs = "needs", None, True
            continue
        if stripped.startswith("Version definition section "):
            section, provider, saw_definitions = "definitions", None, True
            continue
        if stripped.startswith("Version symbols section "):
            section, provider = "symbols", None
            continue
        if section == "needs":
            file_match = re.search(r"\bFile:\s*(\S+)", line)
            if file_match:
                provider = file_match.group(1)
                if not SONAME.fullmatch(provider):
                    raise ValueError(f"unsafe version provider in {member}")
                required.setdefault(provider, set())
            name_match = re.search(r"\bName:\s*(\S+)", line)
            if name_match:
                if provider is None or not VERSION_NAME.fullmatch(name_match.group(1)):
                    raise ValueError(f"malformed required symbol version in {member}")
                required[provider].add(name_match.group(1))
                count += 1
        elif section == "definitions":
            name_match = re.search(r"\bName:\s*(\S+)", line)
            if name_match:
                if not VERSION_NAME.fullmatch(name_match.group(1)):
                    raise ValueError(f"malformed provided symbol version in {member}")
                provided.add(name_match.group(1))
                count += 1
        if count > MAX_VERSION_NAMES:
            raise ValueError("ELF symbol-version inventory exceeds its bound")
    # readelf prints these section headings only when the corresponding section
    # exists. Empty output is a valid absence; dangling structured rows are not.
    if ("File:" in output or "Name:" in output) and not (saw_needs or saw_definitions):
        raise ValueError(f"unscoped ELF version information in {member}")
    return ({name: sorted(versions) for name, versions in sorted(required.items())},
            sorted(provided))


def _validate_layout(regulars: dict, links: dict, directories: set[str],
                     nodes: dict, python_version: str, soabi: str) -> tuple[str, str, str]:
    if sum(identity["elf"] for identity in regulars.values()) > MAX_ELF_OBJECTS:
        raise ValueError("GNU Python ELF object count exceeds its bound")
    if len(links) > MAX_LINKS:
        raise ValueError("GNU Python symlink count exceeds its bound")
    entrypoint, libpython, extension_root = _python_contract(python_version, soabi)
    for member in (entrypoint, libpython, LOADER_MEMBER):
        if member not in regulars or not regulars[member]["elf"]:
            raise ValueError(f"required GNU Python ELF is absent: {member}")
    if extension_root not in directories:
        raise ValueError("GNU Python lib-dynload directory is absent")
    extensions = [member for member, identity in regulars.items()
                  if identity["elf"] and member.startswith(extension_root + "/")]
    if not extensions:
        raise ValueError("GNU Python has no ELF extension modules")
    suffix = "." + soabi + ".so"
    if any("/" in member.removeprefix(extension_root + "/") or
           not member.endswith(suffix) for member in extensions):
        raise ValueError("GNU Python extension does not match the admitted SOABI")
    if not regulars[entrypoint]["mode"] & 0o111 or not regulars[LOADER_MEMBER]["mode"] & 0o111:
        raise ValueError("GNU Python entrypoint or loader is not executable")
    for member in sorted(links):
        _resolve_member(member, nodes)
    return entrypoint, libpython, extension_root


def relocate_python_runtime(python_root: Path, tools, *, runtime_root: str,
                            needed_replacements: dict[str, dict[str, str]],
                            needed_additions: dict[str, list[str]],
                            python_version: str = DEFAULT_PYTHON_VERSION,
                            soabi: str = DEFAULT_SOABI) -> dict:
    """Relocate every dynamic Python ELF with an external runtime edge."""
    runtime_root = _runtime_root(runtime_root)
    regulars, links, directories = _scan(Path(python_root))
    nodes = _resolution_nodes(regulars, links, directories)
    _validate_layout(regulars, links, directories, nodes, python_version, soabi)
    if not isinstance(needed_replacements, dict):
        raise ValueError("GNU Python relocation requires an exact replacement map")
    facts_by_member = {}
    observed_slash_needed = {}
    for member, identity in sorted(regulars.items()):
        if not identity["elf"] or member == LOADER_MEMBER:
            continue
        facts = tools.facts(identity["path"])
        facts_by_member[member] = facts
        slash_needed = [name for name in facts["needed"] if "/" in name]
        if slash_needed:
            observed_slash_needed[member] = set(slash_needed)
    if set(needed_replacements) != set(observed_slash_needed):
        raise ValueError("slash-bearing DT_NEEDED replacements are incomplete or unused")
    for member, replacements in needed_replacements.items():
        if (not isinstance(replacements, dict) or not replacements or
                set(replacements) != observed_slash_needed[member]):
            raise ValueError(f"DT_NEEDED replacements differ from observed edges: {member}")
        existing = set(facts_by_member[member]["needed"])
        for old, new in replacements.items():
            if (not isinstance(old, str) or "/" not in old or "\0" in old or len(old) > 1024 or
                    not isinstance(new, str) or not SONAME.fullmatch(new) or new in existing):
                raise ValueError(f"unsafe or colliding DT_NEEDED replacement: {member}")
    if (not isinstance(needed_additions, dict) or
            list(needed_additions) != sorted(needed_additions)):
        raise ValueError("GNU Python relocation requires canonical DT_NEEDED additions")
    for member, additions in needed_additions.items():
        facts = facts_by_member.get(member)
        if facts is None or not facts["dynamic"]:
            raise ValueError(f"DT_NEEDED addition target is not a dynamic ELF: {member}")
        if (not isinstance(additions, list) or not additions or len(additions) > 16 or
                additions != sorted(set(additions)) or
                any(not isinstance(name, str) or not SONAME.fullmatch(name)
                    for name in additions)):
            raise ValueError(f"invalid finite DT_NEEDED additions: {member}")
        if set(additions) & set(facts["needed"]):
            raise ValueError(f"DT_NEEDED additions are already present: {member}")
        for name in additions:
            resolved, kind = _resolve_member("lib/" + name, nodes)
            if (kind != "regular" or not resolved.startswith("lib/") or
                    resolved not in regulars or not regulars[resolved]["elf"]):
                raise ValueError(f"DT_NEEDED addition lacks a runtime/lib provider: {name}")
            provider_facts = tools.facts(regulars[resolved]["path"])
            if provider_facts.get("soname") != [name]:
                raise ValueError(f"DT_NEEDED addition provider has the wrong SONAME: {name}")
    transformations = {}
    for member, identity in sorted(regulars.items()):
        if not identity["elf"] or member == LOADER_MEMBER:
            continue
        facts = facts_by_member[member]
        additions = needed_additions.get(member, [])
        if (facts["interpreter"] or facts["needed"] or additions) and not facts["dynamic"]:
            raise ValueError(f"ELF runtime edge lacks a dynamic section: {member}")
        if facts["dynamic"] and (facts["interpreter"] or facts["needed"] or additions):
            path = identity["path"]
            before = sha256(path)
            symbols = tools.symbols(path)
            replacements = needed_replacements.get(member, {})
            for old, new in sorted(replacements.items()):
                tools.run("patchelf", "--no-sort", "--replace-needed", old, new, str(path))
            for name in additions:
                tools.run("patchelf", "--no-sort", "--add-needed", name, str(path))
            edited_facts = tools.facts(path)
            expected_needed = sorted(
                [replacements.get(name, name) for name in facts["needed"]] + additions)
            if sorted(edited_facts["needed"]) != expected_needed:
                raise ValueError(
                    f"GNU Python dependency edits differ from their declaration: {member}")
            relocated = relocate_elf(path, tools, runtime_root)
            after_facts = tools.facts(path)
            if (sorted(after_facts["needed"]) != expected_needed or
                    tools.symbols(path) != symbols):
                raise ValueError(f"GNU Python ELF relocation changed its admitted semantics: {member}")
            transformations[member] = {
                "before": before,
                "after": relocated["after"],
                "needed_replacements": [{"before": old, "after": new}
                                        for old, new in sorted(replacements.items())],
                "needed_additions": list(additions),
            }
    return transformations


def inspect_python_runtime(python_root: Path, tools, *, runtime_root: str,
                           python_version: str = DEFAULT_PYTHON_VERSION,
                           soabi: str = DEFAULT_SOABI) -> dict:
    """Prove the finite relocated GNU CPython ELF and symbol-version closure."""
    runtime_root = _runtime_root(runtime_root)
    python_root = Path(python_root)
    regulars, links, directories = _scan(python_root)
    nodes = _resolution_nodes(regulars, links, directories)
    entrypoint, libpython, _ = _validate_layout(
        regulars, links, directories, nodes, python_version, soabi)
    loader = runtime_root + "/" + LOADER_MEMBER
    objects = []
    by_member = {}
    version_names = 0
    for member, identity in sorted(regulars.items()):
        if not identity["elf"]:
            continue
        if len(objects) >= MAX_ELF_OBJECTS:
            raise ValueError("GNU Python ELF object count exceeds its bound")
        path = identity["path"]
        facts = tools.facts(path)
        elf_type = _parse_header(tools.run("readelf", "-W", "-h", str(path)), member)
        gnu_stack = _parse_gnu_stack(tools.run("readelf", "-W", "-l", str(path)), member)
        required, provided = _parse_version_info(
            tools.run("readelf", "-W", "--version-info", str(path)), member)
        version_names += len(provided) + sum(len(names) for names in required.values())
        if version_names > MAX_VERSION_NAMES:
            raise ValueError("GNU Python symbol-version inventory exceeds its bound")
        needed = facts["needed"]
        if (not isinstance(needed, list) or len(needed) > MAX_NEEDED_PER_OBJECT or
                len(needed) != len(set(needed)) or
                any(not isinstance(name, str) or not SONAME.fullmatch(name) for name in needed)):
            raise ValueError(f"unsafe or excessive DT_NEEDED closure: {member}")
        sonames = facts.get("soname")
        if (not isinstance(sonames, list) or len(sonames) > 1 or
                any(not isinstance(name, str) or not SONAME.fullmatch(name)
                    for name in sonames)):
            raise ValueError(f"ELF has an invalid DT_SONAME: {member}")
        if facts["interpreter"] not in ([], [loader]):
            raise ValueError(f"ELF has an unclosed interpreter: {member}")
        if any(provider not in needed for provider in required):
            raise ValueError(f"symbol-version provider is not a DT_NEEDED edge: {member}")
        needs_runtime = bool(facts["interpreter"] or needed)
        if member == LOADER_MEMBER:
            if (facts["interpreter"] or needed or facts["runpath"] or
                    facts["rpath"] or facts["nodeflib"]):
                raise ValueError("GNU loader must be independently loadable")
        elif needs_runtime:
            if (not facts["dynamic"] or facts["runpath"] != [runtime_root + "/lib"] or
                    facts["rpath"] or not facts["nodeflib"]):
                raise ValueError(f"ELF library search is not closed: {member}")
        elif facts["runpath"] or facts["rpath"]:
            raise ValueError(f"self-contained ELF has an unexpected search path: {member}")
        record = {
            "path": member,
            "sha256": sha256(path),
            "bytes": identity["bytes"],
            "mode": identity["mode"],
            "elf_type": elf_type,
            "dynamic": bool(facts["dynamic"]),
            "interpreter": list(facts["interpreter"]),
            "needed": sorted(needed),
            "soname": sonames[0] if sonames else None,
            "resolved_needed": {},
            "runpath": list(facts["runpath"]),
            "rpath": bool(facts["rpath"]),
            "nodeflib": bool(facts["nodeflib"]),
            "gnu_stack": gnu_stack,
            "required_versions": required,
            "provided_versions": provided,
        }
        objects.append(record)
        by_member[member] = record
    if any(record["elf_type"] != "DYN" for record in objects
           if record["path"] != entrypoint):
        raise ValueError("GNU Python libraries and loader must be ELF DYN objects")
    if by_member[entrypoint]["interpreter"] != [loader]:
        raise ValueError("GNU Python entrypoint does not use the exact admitted loader")
    # Resolve each DT_NEEDED edge from the only admitted search directory.
    for record in objects:
        for needed in record["needed"]:
            requested = "lib/" + needed
            resolved, kind = _resolve_member(requested, nodes)
            if kind != "regular" or not resolved.startswith("lib/") or resolved not in by_member:
                raise ValueError(f"DT_NEEDED does not resolve to a runtime/lib ELF: {needed}")
            # DT_SONAME is optional. When it is absent, the exact admitted
            # runtime/lib filename is the loader identity for this edge. A
            # present SONAME must still agree with the requested name.
            if (by_member[resolved]["soname"] is not None and
                    by_member[resolved]["soname"] != needed):
                raise ValueError(f"DT_NEEDED provider SONAME differs from request: {needed}")
            record["resolved_needed"][needed] = resolved
            missing = set(record["required_versions"].get(needed, ())) - set(
                by_member[resolved]["provided_versions"])
            if missing:
                raise ValueError(f"DT_NEEDED provider lacks required symbol versions: {needed}")
    link_inventory = []
    for member, target in sorted(links.items()):
        resolved, _ = _resolve_member(member, nodes)
        link_inventory.append({"path": member, "target": target,
                               "resolved": resolved})
    inventory = {
        "schema": INVENTORY_SCHEMA,
        "runtime_root": runtime_root,
        "python": {"version": python_version, "soabi": soabi,
                   "entrypoint": entrypoint, "libpython": libpython},
        "loader": LOADER_MEMBER,
        "objects": objects,
        "links": link_inventory,
    }
    canonical_inventory(inventory)
    return inventory


def canonical_inventory(inventory: dict) -> bytes:
    if (not isinstance(inventory, dict) or
            set(inventory) != {"schema", "runtime_root", "python", "loader", "objects", "links"} or
            inventory["schema"] != INVENTORY_SCHEMA or inventory["loader"] != LOADER_MEMBER or
            not isinstance(inventory["objects"], list) or
            not isinstance(inventory["links"], list) or
            len(inventory["objects"]) > MAX_ELF_OBJECTS or len(inventory["links"]) > MAX_LINKS):
        raise ValueError("invalid GNU Python ELF inventory")
    runtime_root = _runtime_root(inventory["runtime_root"])
    python = inventory["python"]
    if not isinstance(python, dict) or set(python) != {"version", "soabi", "entrypoint", "libpython"}:
        raise ValueError("invalid GNU Python identity inventory")
    entrypoint, libpython, _ = _python_contract(python["version"], python["soabi"])
    if python["entrypoint"] != entrypoint or python["libpython"] != libpython:
        raise ValueError("GNU Python identity inventory is inconsistent")
    object_fields = {"path", "sha256", "bytes", "mode", "elf_type", "dynamic", "soname",
                     "interpreter", "needed", "resolved_needed", "runpath", "rpath",
                     "nodeflib", "gnu_stack", "required_versions", "provided_versions"}
    object_paths = []
    versions = 0
    for record in inventory["objects"]:
        if not isinstance(record, dict) or set(record) != object_fields:
            raise ValueError("invalid GNU Python ELF object inventory")
        relative(record["path"])
        object_paths.append(record["path"])
        if (not isinstance(record["sha256"], str) or
                not re.fullmatch(r"[0-9a-f]{64}", record["sha256"]) or
                type(record["bytes"]) is not int or not 0 <= record["bytes"] <= MAX_FILE_BYTES or
                type(record["mode"]) is not int or not 0 <= record["mode"] <= 0o7777 or
                record["elf_type"] not in ("EXEC", "DYN") or
                (record["soname"] is not None and
                 (not isinstance(record["soname"], str) or
                  not SONAME.fullmatch(record["soname"]))) or
                type(record["dynamic"]) is not bool or type(record["rpath"]) is not bool or
                type(record["nodeflib"]) is not bool or record["rpath"] or
                record["gnu_stack"] not in ("r", "rw")):
            raise ValueError("invalid GNU Python ELF object fact")
        for key in ("interpreter", "needed", "runpath", "provided_versions"):
            if not isinstance(record[key], list) or not all(isinstance(value, str) for value in record[key]):
                raise ValueError("invalid GNU Python ELF list fact")
        if (record["needed"] != sorted(set(record["needed"])) or
                len(record["needed"]) > MAX_NEEDED_PER_OBJECT or
                any(not SONAME.fullmatch(name) for name in record["needed"])):
            raise ValueError("invalid canonical DT_NEEDED inventory")
        resolved = record["resolved_needed"]
        required = record["required_versions"]
        if (not isinstance(resolved, dict) or set(resolved) != set(record["needed"]) or
                not isinstance(required, dict) or any(name not in record["needed"] for name in required)):
            raise ValueError("incomplete GNU Python dependency inventory")
        for needed, member in resolved.items():
            if not SONAME.fullmatch(needed) or not isinstance(member, str):
                raise ValueError("invalid resolved GNU Python dependency")
            relative(member)
            if not member.startswith("lib/"):
                raise ValueError("resolved GNU Python dependency leaves runtime/lib")
        if (record["provided_versions"] != sorted(set(record["provided_versions"])) or
                any(not VERSION_NAME.fullmatch(name) for name in record["provided_versions"])):
            raise ValueError("noncanonical provided symbol versions")
        for provider, names in required.items():
            if (not SONAME.fullmatch(provider) or not isinstance(names, list) or
                    names != sorted(set(names)) or
                    any(not isinstance(name, str) or not VERSION_NAME.fullmatch(name) for name in names)):
                raise ValueError("noncanonical required symbol versions")
        versions += len(record["provided_versions"]) + sum(len(names) for names in required.values())
    if (object_paths != sorted(set(object_paths)) or entrypoint not in object_paths or
            LOADER_MEMBER not in object_paths or versions > MAX_VERSION_NAMES):
        raise ValueError("incomplete or noncanonical GNU Python ELF objects")
    by_member = {record["path"]: record for record in inventory["objects"]}
    expected_loader = runtime_root + "/" + LOADER_MEMBER
    for record in inventory["objects"]:
        needs_runtime = bool(record["interpreter"] or record["needed"])
        if record["interpreter"] not in ([], [expected_loader]):
            raise ValueError("GNU Python inventory has an unclosed interpreter")
        if record["path"] == LOADER_MEMBER:
            if (record["elf_type"] != "DYN" or not record["dynamic"] or
                    record["interpreter"] or record["needed"] or record["runpath"] or
                    record["rpath"] or record["nodeflib"]):
                raise ValueError("GNU Python inventory has an invalid loader")
        elif record["path"] != entrypoint and record["elf_type"] != "DYN":
            raise ValueError("GNU Python inventory has a non-DYN library")
        elif needs_runtime:
            if (not record["dynamic"] or record["runpath"] != [runtime_root + "/lib"] or
                    record["rpath"] or not record["nodeflib"]):
                raise ValueError("GNU Python inventory has an unclosed library search")
        elif record["runpath"] or record["rpath"]:
            raise ValueError("GNU Python inventory has an unexpected search path")
        for needed, provider in record["resolved_needed"].items():
            target = by_member.get(provider)
            if target is None:
                raise ValueError("GNU Python inventory dependency is not an ELF object")
            if target["soname"] is not None and target["soname"] != needed:
                raise ValueError("GNU Python inventory provider SONAME differs from request")
            missing = set(record["required_versions"].get(needed, ())) - set(
                target["provided_versions"])
            if missing:
                raise ValueError("GNU Python inventory provider lacks a requested version")
    if (by_member[entrypoint]["interpreter"] != [expected_loader] or
            not by_member[entrypoint]["mode"] & 0o111 or
            not by_member[LOADER_MEMBER]["mode"] & 0o111):
        raise ValueError("GNU Python inventory lacks its exact executable loader")
    link_fields = {"path", "target", "resolved"}
    link_paths = []
    for link in inventory["links"]:
        if not isinstance(link, dict) or set(link) != link_fields:
            raise ValueError("invalid GNU Python link inventory")
        relative(link["path"])
        relative(link["resolved"])
        _validate_raw_link(link["path"], link["target"])
        link_paths.append(link["path"])
    if link_paths != sorted(set(link_paths)):
        raise ValueError("noncanonical GNU Python link inventory")
    encoded = canonical_json(inventory)
    if len(encoded) > MAX_INVENTORY_BYTES:
        raise ValueError("GNU Python ELF inventory exceeds its byte bound")
    return encoded


def inventory_evidence(inventory: dict) -> dict:
    encoded = canonical_inventory(inventory)
    required = sum(len(versions) for record in inventory["objects"]
                   for versions in record["required_versions"].values())
    provided = sum(len(record["provided_versions"]) for record in inventory["objects"])
    return {
        "schema": EVIDENCE_SCHEMA,
        "inventory_sha256": hashlib.sha256(encoded).hexdigest(),
        "runtime_root": inventory["runtime_root"],
        "loader": inventory["loader"],
        "elf_objects": len(inventory["objects"]),
        "library_links": sum(link["path"].startswith("lib/") for link in inventory["links"]),
        "required_symbol_versions": required,
        "provided_symbol_versions": provided,
    }
