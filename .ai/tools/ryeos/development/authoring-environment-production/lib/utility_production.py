# ryeos:signed:2026-09-07T08:05:59Z:9c2b51c98e5ace0b2584d8d5a608087d76cd2658510d54d66d1c7abf9ec0636f:QC62Y286PnWqKQ/iuy44uCmyskNLZrMjnzdvE82Mv4VfYnjgs4fhx3b73uTWdF6zj8hGhPdFya/TzpqHeHnFDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Admitted utility production entry, shared by the Tool and its E2E probe.

utilities.py owns the sole compiler recipe; preparation.py owns source selection.
This module supplies the existing request/retained-output boundary. No transport,
signing, import, binding, worker permission or process-lifecycle owner is added.
"""
import json
from pathlib import Path
import shutil
import sys

from archives import read_members
from preparation import INPUT_CONFIG, RAW_ROOT, SOURCE_CONFIG, selection
from production import ordinary_member, sha256
from utilities import BUILD_ORDER, MAX_LOG_BYTES, build_utilities

SUPPORT_CONFIG = "development/ryeos/authoring-build-support.yaml"
WORK = Path("/tmp/utility-build")
OUTPUT = Path("products/authoring-built-utilities")


def parse_request():
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute() or project.is_symlink() or not project.is_dir():
        raise ValueError("invalid admitted project context")
    raw = sys.stdin.buffer.read(262145)
    if len(raw) > 262144:
        raise ValueError("utility production parameters exceed bound")
    resolved = json.loads(raw)["resolved_config"]
    if set(resolved) != {INPUT_CONFIG, SOURCE_CONFIG, SUPPORT_CONFIG}:
        raise ValueError("missing exact utility production Configs")
    return project, resolved


def produce(project, resolved):
    inputs, sources = resolved[INPUT_CONFIG], resolved[SOURCE_CONFIG]
    expected, _, _, archive_name = selection(inputs, sources)
    member = "bootstrap/" + archive_name
    archive = ordinary_member(RAW_ROOT, member)
    if archive.stat().st_size != expected[member]["bytes"] or sha256(archive) != expected[member]["sha256"]:
        raise ValueError("source container does not match admitted selection")
    selected = Path("/tmp/utility-source-archives")
    selected.mkdir(mode=0o700)
    # Use the existing bounded archive selector for the admitted source set.
    # utilities.py remains the sole verifier/extractor/compiler/output recipe.
    archives = read_members(archive, {source["archive"] for source in sources["sources"]})
    for source in sources["sources"]:
        path = selected / source["archive"]
        with path.open("xb") as output:
            output.write(archives.pop(source["archive"]))
        path.chmod(0o644)
    products = project / "products"
    if not products.exists():
        products.mkdir(mode=0o700)
    ordinary_member(project, "products", directory=True)
    work = WORK
    destination = project / OUTPUT
    logs = products / "authoring-build-logs"
    if logs.exists() or logs.is_symlink():
        raise ValueError("utility build log destination already exists")
    try:
        result = build_utilities(sources, resolved[SUPPORT_CONFIG]["support"],
                                 selected, work, destination)
    finally:
        # Preserve bounded diagnostics even when a real upstream descendant
        # fails. Do not retain the whole compiler cache or source build tree.
        logs.mkdir(mode=0o700)
        for name in BUILD_ORDER:
            candidate = work / "logs" / (name + ".log")
            if not candidate.exists() and not candidate.is_symlink():
                continue
            log = ordinary_member(work, "logs/" + name + ".log")
            size = log.stat().st_size
            if size > MAX_LOG_BYTES:
                raise ValueError("retained build log exceeds production bound")
            shutil.copyfile(log, logs / log.name)
            (logs / log.name).chmod(0o644)
            with log.open("rb") as stream:
                stream.seek(max(0, size - 8192))
                sys.stderr.write(stream.read(8192).decode(errors="replace"))
    return {"ok": True, "output_path": OUTPUT.as_posix(), **result}


def main():
    project, resolved = parse_request()
    print(json.dumps(produce(project, resolved), sort_keys=True))
