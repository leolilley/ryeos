# ryeos:signed:2026-09-08T15:36:50Z:7456daa992ae8a0b98551f2db8d65afbf6799139553df434064db25e1e28e08b:4dwjiqeDN9HO3yKXcOEAC6eVtgE8SStYTpNf/H29mYxKYO8BoLCWr8XhaZOU6g4hN+j65rbsuTZ6mjT9u/SuCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Finite, offline authoring-environment assembly; no acquisition or publication.

RyeOS owns capture, namespaces, result snapshots and import/binding. This code
only consumes the admitted input tree and writes its private project output.
The existing Python payload supplies the interpreter; selected upstream tools
inspect and relocate ELF files. There is no host PATH or custom ELF rewriter.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys


SCHEMA = "ryeos.development.authoring-environment-inputs.v2"
INPUT_ROOT = Path("/ryeos/realizations/authoring-inputs")
BUILT_UTILITIES_ROOT = Path("/ryeos/realizations/authoring-built-utilities")
INPUT_CONFIG = "development/ryeos/authoring-environment-inputs.yaml"
SOURCE_CONFIG = "development/ryeos/authoring-utility-sources.yaml"
SUPPORT_CONFIG = "development/ryeos/authoring-build-support.yaml"
RUNTIME_ROOT = "/ryeos/realizations/authoring-tools"
OUTPUT = PurePosixPath("products/authoring-environment")
MAX_FILE_BYTES = 128 * 1024 * 1024
MAX_TOTAL_BYTES = 256 * 1024 * 1024
MAX_ENTRIES = 1024
MAX_DIAGNOSTIC_BYTES = 8 * 1024 * 1024
REQUIRED_COMMANDS = frozenset("""
awk basename cat chmod cmp cp cut date diff dirname env find git grep head ln ls
mkdir mktemp mv patch printf pwd readlink realpath rg rm rmdir sed sha256sum sleep
sort stat tail tee test timeout touch tr uniq wc xargs zsh
""".split())
BUILT_UTILITY_COMMANDS = REQUIRED_COMMANDS - {"rg", "zsh"}
ELF_TOOLS = {"loader": "elf/lib/ld-linux-x86-64.so.2",
             "readelf": "elf/bin/readelf", "patchelf": "elf/bin/patchelf"}
RUNTIME_LOADER = "environment/lib/ld-linux-x86-64.so.2"


def canonical_json(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def sha256(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def relative(value: str) -> PurePosixPath:
    if not isinstance(value, str) or len(value) > 1024:
        raise ValueError("invalid member path")
    parts = value.split("/")
    if not parts or len(parts) > 32 or any(
        part in ("", ".", "..") or not re.fullmatch(r"[A-Za-z0-9_.+-]+", part)
        for part in parts
    ):
        raise ValueError(f"noncanonical member path: {value}")
    return PurePosixPath(value)


def ordinary_member(root: Path, member: str, *, directory: bool = False) -> Path:
    path = root
    if not stat.S_ISDIR(path.lstat().st_mode):
        raise ValueError("input/output root must be an ordinary directory")
    parts = relative(member).parts
    for index, part in enumerate(parts):
        path = path / part
        mode = path.lstat().st_mode
        wanted = stat.S_ISDIR if directory or index < len(parts) - 1 else stat.S_ISREG
        if not wanted(mode):
            raise ValueError(f"member is not an ordinary {'directory' if directory else 'file'}: {member}")
    return path


def validate_config(config: dict) -> None:
    keys = {"category", "name", "version", "schema", "source_date_epoch", "inputs",
            "files", "built_utility_files", "relocate", "provenance"}
    if not isinstance(config, dict) or set(config) != keys or config["schema"] != SCHEMA:
        raise ValueError("unsupported or incomplete authoring input contract")
    if (config["category"] != "development/ryeos" or
            config["name"] != "authoring-environment-inputs" or config["version"] != "1.0.0"):
        raise ValueError("unexpected authoring input identity")
    if type(config["source_date_epoch"]) is not int or not 0 <= config["source_date_epoch"] <= 4_102_444_800:
        raise ValueError("invalid source normalization epoch")
    inputs, files = config["inputs"], config["files"]
    built_files = config["built_utility_files"]
    if (not isinstance(inputs, dict) or not isinstance(files, dict) or
            not isinstance(built_files, dict) or not 1 <= len(inputs) <= MAX_ENTRIES):
        raise ValueError("invalid finite input inventory")
    total = 0
    for name, identity in inputs.items():
        relative(name)
        if not isinstance(identity, dict) or set(identity) != {"bytes", "sha256", "mode"}:
            raise ValueError("incomplete file identity")
        if (type(identity["bytes"]) is not int or not 0 <= identity["bytes"] <= MAX_FILE_BYTES or
                type(identity["mode"]) is not int or identity["mode"] not in (0o644, 0o755) or
                not isinstance(identity["sha256"], str) or
                not re.fullmatch(r"[0-9a-f]{64}", identity["sha256"])):
            raise ValueError("invalid file identity or bounds")
        total += identity["bytes"]
    if total > MAX_TOTAL_BYTES or not 1 <= len(files) <= MAX_ENTRIES:
        raise ValueError("authoring input inventory exceeds its bound")
    for target, source in files.items():
        parts = relative(target).parts
        if ((parts[0] == "environment" and len(parts) >= 3 and parts[1] in ("bin", "lib", "licenses")) or
                (parts[0] == "corresponding-sources" and len(parts) >= 2)):
            if source not in inputs:
                raise ValueError("output selects an undeclared input")
        else:
            raise ValueError("output is outside the finite artifact layout")
    if set(files) & set(built_files):
        raise ValueError("fixed and built utility selections overlap")
    if (list(built_files) != sorted(built_files) or
            set(built_files) != {f"environment/bin/{name}" for name in BUILT_UTILITY_COMMANDS} or
            any(source != "bin/" + target.removeprefix("environment/bin/")
                for target, source in built_files.items())):
        raise ValueError("built utility selection is not the exact finite command map")
    if sum(inputs[source]["bytes"] for source in files.values()) > MAX_TOTAL_BYTES:
        raise ValueError("selected outputs exceed the artifact byte bound")
    selected_files = set(files) | set(built_files)
    commands = {str(PurePosixPath(path).relative_to("environment/bin"))
                for path in selected_files if path.startswith("environment/bin/")}
    if commands != REQUIRED_COMMANDS:
        raise ValueError("authoring command inventory is not the supported exact set")
    if RUNTIME_LOADER not in files:
        raise ValueError("the runtime interpreter must be included in the artifact")
    executable_outputs = {"environment/bin/rg", "environment/bin/zsh", RUNTIME_LOADER}
    if any(inputs[files[path]]["mode"] != 0o755 for path in executable_outputs):
        raise ValueError("commands and runtime interpreter must have executable mode")
    if not any(path.startswith("corresponding-sources/") for path in files):
        raise ValueError("corresponding source delivery is required")
    if not all(value in inputs for value in ELF_TOOLS.values()):
        raise ValueError("selected ELF authoring tools are missing")
    if any(inputs[path]["mode"] != 0o755 for path in ELF_TOOLS.values()):
        raise ValueError("selected ELF authoring tools must have executable mode")
    for path in selected_files:
        if any(parent.as_posix() in selected_files for parent in PurePosixPath(path).parents):
            raise ValueError("output file/directory collision")
    if (not isinstance(config["relocate"], list) or
            config["relocate"] != sorted(set(config["relocate"])) or
            "environment/bin/zsh" not in config["relocate"]):
        raise ValueError("relocation inventory must be exact, sorted and include the selected shell")
    for member in config["relocate"]:
        if member not in files or not member.startswith(("environment/bin/", "environment/lib/")):
            raise ValueError("relocation target is not an admitted ELF input")
        if member == "environment/lib/ld-linux-x86-64.so.2":
            raise ValueError("the loader itself must retain upstream bytes")
    if not isinstance(config["provenance"], dict) or not config["provenance"]:
        raise ValueError("bootstrap and upstream source provenance is required")


def portable_regular_mode(mode: int) -> int:
    """Match Lillux's normalized_portable_regular_mode for admitted inputs.

    Content manifests commit regular-file bytes and executable class, not
    mutable storage permissions. An immutable large-object hardlink may be
    0444 while its portable manifest mode is 0644. Never chmod that shared
    input to satisfy its manifest, or use this projection for output receipts.
    """
    if not stat.S_ISREG(mode):
        raise ValueError("portable input mode requires an ordinary regular file")
    return 0o755 if mode & 0o111 else 0o644


def inventory(root: Path) -> dict:
    """Physical output inventory: retain exact permission bits in receipts."""
    return _inventory(root, portable_inputs=False)


def input_inventory(root: Path) -> dict:
    """Admitted content identity, using the same portable mode as its manifest."""
    return _inventory(root, portable_inputs=True)


def _inventory(root: Path, *, portable_inputs: bool) -> dict:
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("artifact is not an ordinary directory")
    result, total = {}, 0
    pending = [(root, 0)]
    visited = 0
    while pending:
        directory, depth = pending.pop()
        if depth > 32:
            raise ValueError("artifact depth limit exceeded")
        for path in sorted(directory.iterdir()):
            visited += 1
            if visited > MAX_ENTRIES:
                raise ValueError("artifact entry limit exceeded")
            info = path.lstat()
            name = path.relative_to(root).as_posix()
            relative(name)
            if stat.S_ISDIR(info.st_mode):
                pending.append((path, depth + 1))
            elif stat.S_ISREG(info.st_mode):
                total += info.st_size
                if info.st_size > MAX_FILE_BYTES or total > MAX_TOTAL_BYTES:
                    raise ValueError("artifact byte limit exceeded")
                mode = portable_regular_mode(info.st_mode) if portable_inputs else stat.S_IMODE(info.st_mode)
                result[name] = {"sha256": sha256(path), "bytes": info.st_size, "mode": mode}
            else:
                raise ValueError(f"unselected link or special artifact member: {name}")
    return result


def checked_inputs(root: Path, config: dict) -> None:
    validate_config(config)
    if input_inventory(root) != config["inputs"]:
        raise ValueError("admitted source bytes or modes differ from the authored input inventory")


def _bounded_json_member(root: Path, member: str) -> dict:
    path = ordinary_member(root, member)
    if path.stat().st_size > 1024 * 1024:
        raise ValueError("built utility metadata exceeds its bound")
    with path.open("rb") as source:
        value = json.load(source)
    if not isinstance(value, dict):
        raise ValueError("built utility metadata must be an object")
    return value


def validate_built_utilities(root: Path, config: dict,
                             source_config: dict, support_config: dict) -> dict:
    """Validate the selected product's finite semantic surface before copying.

    RyeOS authenticates the complete product manifest and bytes. This owner
    additionally proves that the only executable members admitted to the final
    environment are the exact source-built command set and that their retained
    build contracts agree with the current signed recipe inputs.
    """
    validate_config(config)
    if not isinstance(source_config, dict) or not isinstance(support_config, dict):
        raise ValueError("missing built utility source or support contract")
    observed = input_inventory(root)
    allowed_roots = ("bin/", "licenses/", "corresponding-sources/")
    if any(name not in {"source-contract.json", "build-evidence.json"}
           and not name.startswith(allowed_roots) for name in observed):
        raise ValueError("unexpected built utility product member")
    bins = {name.removeprefix("bin/") for name in observed
            if name.startswith("bin/") and len(relative(name).parts) == 2}
    if bins != BUILT_UTILITY_COMMANDS:
        raise ValueError("built utility product has an incomplete command inventory")
    for source in config["built_utility_files"].values():
        identity = observed.get(source)
        if identity is None or identity["mode"] != 0o755:
            raise ValueError("built utility command is absent or not executable")
    if (not any(name.startswith("licenses/") for name in observed) or
            not any(name.startswith("corresponding-sources/") for name in observed)):
        raise ValueError("built utility licenses or corresponding sources are absent")
    if _bounded_json_member(root, "source-contract.json") != source_config:
        raise ValueError("built utility source contract differs from current admission")
    evidence = _bounded_json_member(root, "build-evidence.json")
    from utilities import build_evidence
    support = support_config.get("support")
    if not isinstance(support, dict):
        raise ValueError("missing exact nested build support contract")
    expected = build_evidence(source_config, support)
    if evidence != expected:
        raise ValueError("built utility evidence differs from current recipe contracts")
    return observed


class ElfTools:
    """The selected upstream programs, invoked through their exact loader."""

    def __init__(self, inputs: Path):
        self.inputs = inputs

    def run(self, name: str, *args: str) -> str:
        if name not in ("readelf", "patchelf"):
            raise ValueError("not a production ELF operation")
        command = [str(ordinary_member(self.inputs, ELF_TOOLS["loader"])), "--inhibit-cache",
                   "--library-path", str(ordinary_member(self.inputs, "elf/lib", directory=True)),
                   str(ordinary_member(self.inputs, ELF_TOOLS[name])), *map(str, args)]
        # The enclosing RyeOS execution owns the process-group/time/resource
        # ceiling. This reader additionally bounds one upstream diagnostic.
        with subprocess.Popen(command, env={"LANG": "C", "LC_ALL": "C", "PATH": ""},
                              stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT) as process:
            try:
                output = process.stdout.read(MAX_DIAGNOSTIC_BYTES + 1)
                if len(output) > MAX_DIAGNOSTIC_BYTES:
                    raise ValueError("upstream ELF diagnostic exceeds its bound")
                status = process.wait(timeout=30)
                if status:
                    raise ValueError(f"selected {name} refused ({status}): {output[-2048:].decode(errors='replace')}")
                return output.decode("utf-8", errors="strict")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()

    def facts(self, path: Path) -> dict:
        header = self.run("readelf", "-W", "-h", str(path))
        if "ELF64" not in header or "Advanced Micro Devices X86-64" not in header:
            raise ValueError("artifact executable is not ELF64 x86_64")
        dynamic = self.run("readelf", "-W", "-d", str(path))
        program = self.run("readelf", "-W", "-l", str(path))
        interpreter = re.findall(r"Requesting program interpreter: ([^\]]+)\]", program)
        return {"dynamic": "Dynamic section" in dynamic,
                "interpreter": interpreter,
                "needed": re.findall(r"Shared library: \[([^\]]+)\]", dynamic),
                "soname": re.findall(r"Library soname: \[([^\]]+)\]", dynamic),
                "runpath": re.findall(r"Library runpath: \[([^\]]*)\]", dynamic),
                "rpath": "(RPATH)" in dynamic, "nodeflib": "NODEFLIB" in dynamic}

    def symbols(self, path: Path) -> tuple:
        # Same upstream readelf projections as the existing Stage-0 producer:
        # compare section ownership and FUNC coordinates, not ELF byte offsets
        # invented by a second binary rewriter.
        sections = dict(re.findall(r"^\s*\[\s*(\d+)\]\s+(\S+)\s", self.run(
            "readelf", "-W", "--section-headers", str(path)), re.MULTILINE))
        owners, functions = [], []
        for line in self.run("readelf", "-W", "--symbols", str(path)).splitlines():
            row = line.split()
            if not row or not re.fullmatch(r"\d+:", row[0]):
                continue
            if len(row) < 7:
                raise ValueError("malformed upstream symbol row")
            index = row[6]
            if index.isdecimal():
                if index not in sections:
                    raise ValueError("upstream symbol names an absent section")
                index = sections[index]
            name = row[7] if len(row) > 7 else ""
            owners.append((*row[3:6], index, name))
            if row[3] == "FUNC":
                functions.append((*row[1:6], name))
        return sorted(owners), sorted(functions)


def relocate_elf(path: Path, tools: ElfTools, runtime_root: str) -> dict:
    """One relocation owner shared by authoring and compiler-support production.

    The calling signed recipe supplies its exact realization mount. Do not add
    host-library discovery, ELF rewriting, or a second symbol verifier here.
    """
    before, symbols = sha256(path), tools.symbols(path)
    facts = tools.facts(path)
    if not facts["dynamic"] or not (facts["interpreter"] or facts["needed"]):
        raise ValueError("only declared dynamic ELF inputs may be relocated")
    # patchelf 0.19.1 can relocate the dynamic string table while setting a
    # longer RUNPATH. On the admitted GNU CPython executable, doing that after
    # --set-interpreter overwrites PT_INTERP. Commit search policy first, then
    # write the exact interpreter into its final location.
    tools.run("patchelf", "--no-sort", "--set-rpath", runtime_root + "/lib",
              "--no-default-lib", str(path))
    if facts["interpreter"]:
        tools.run("patchelf", "--no-sort", "--set-interpreter",
                  runtime_root + "/lib/ld-linux-x86-64.so.2", str(path))
    if tools.symbols(path) != symbols:
        raise ValueError(f"ELF symbol ownership or function coordinates changed: {path.name}")
    return {"before": before, "after": sha256(path)}


def copy_selected_files(inputs: Path, destination: Path, files: dict, identities: dict) -> None:
    """Copy an already verified finite selection into fresh private output."""
    for target, source in sorted(files.items()):
        selected = ordinary_member(inputs, source)
        output = destination.joinpath(*relative(target).parts)
        output.parent.mkdir(parents=True, exist_ok=True)
        with selected.open("rb") as reader, output.open("xb") as writer:
            shutil.copyfileobj(reader, writer, length=1024 * 1024)
        output.chmod(identities[source]["mode"])


def check_closure(environment: Path, files: dict, tools: ElfTools, relocated: set[str],
                  *, runtime_root: str) -> None:
    interpreter = ordinary_member(environment, RUNTIME_LOADER.removeprefix("environment/"))
    if stat.S_IMODE(interpreter.stat().st_mode) != 0o755:
        raise ValueError("runtime interpreter is not executable")
    loader_facts = tools.facts(interpreter)
    if loader_facts["interpreter"] or loader_facts["needed"]:
        raise ValueError("runtime interpreter must be independently loadable")
    for member in sorted(files):
        if not member.startswith(("environment/bin/", "environment/lib/")):
            continue
        local = member.removeprefix("environment/")
        path = ordinary_member(environment, local)
        if local.startswith("bin/") and stat.S_IMODE(path.stat().st_mode) != 0o755:
            raise ValueError(f"command is not executable: {local}")
        facts = tools.facts(path)
        loader = local == "lib/ld-linux-x86-64.so.2"
        if facts["interpreter"] not in ([], [runtime_root + "/lib/ld-linux-x86-64.so.2"]):
            raise ValueError(f"unclosed interpreter: {local}")
        # Static PIE executables also have a dynamic section for their own
        # relocations. Only interpreter/library edges require our runtime;
        # do not rewrite an otherwise self-contained upstream executable.
        needs_runtime = bool(facts["interpreter"] or facts["needed"])
        if needs_runtime and not loader:
            if (member not in relocated or facts["runpath"] != [runtime_root + "/lib"] or
                    facts["rpath"] or not facts["nodeflib"]):
                raise ValueError(f"unclosed library search: {local}")
        elif member in relocated:
            raise ValueError(f"unexpected static relocation target: {local}")
        elif facts["runpath"] or facts["rpath"]:
            raise ValueError(f"unexpected search path on a self-contained ELF: {local}")
        for needed in facts["needed"]:
            if "/" in needed or not re.fullmatch(r"[A-Za-z0-9_+.-]+", needed):
                raise ValueError("unsafe library dependency")
            library = ordinary_member(environment, "lib/" + needed)
            with library.open("rb") as content:
                if content.read(4) != b"\x7fELF":
                    raise ValueError("library dependency names non-ELF data")


def assemble(inputs: Path, built_utilities: Path, destination: Path, config: dict,
             source_config: dict, support_config: dict, *, tools=None) -> dict:
    checked_inputs(inputs, config)
    built_inventory = validate_built_utilities(
        built_utilities, config, source_config, support_config)
    if destination.exists() or destination.is_symlink():
        raise ValueError("assembly destination already exists")
    destination.mkdir(mode=0o700)
    tools = tools or ElfTools(inputs)
    transformations = {}
    copy_selected_files(inputs, destination, config["files"], config["inputs"])
    copy_selected_files(built_utilities, destination, config["built_utility_files"], built_inventory)
    for member in config["relocate"]:
        path = ordinary_member(destination, member)
        transformations[member] = relocate_elf(path, tools, RUNTIME_ROOT)
    selected_files = {**config["files"], **config["built_utility_files"]}
    check_closure(destination / "environment", selected_files, tools, set(transformations),
                  runtime_root=RUNTIME_ROOT)
    provenance = {"schema": 1, "input_contract_sha256": hashlib.sha256(canonical_json(config)).hexdigest(),
                  "built_utility_product_sha256": hashlib.sha256(
                      canonical_json(built_inventory)).hexdigest(),
                  "runtime_mount": RUNTIME_ROOT, "transformations": transformations,
                  "sources": config["provenance"]}
    provenance_path = destination / "provenance.json"
    provenance_path.write_bytes(canonical_json(provenance))
    provenance_path.chmod(0o644)
    for path in sorted(destination.rglob("*")):
        if path.is_dir():
            path.chmod(0o755)
        os.utime(path, (config["source_date_epoch"], config["source_date_epoch"]))
    files = inventory(destination)
    index_path = destination / "inventory.json"
    index_path.write_bytes(canonical_json(files))
    index_path.chmod(0o644)
    os.utime(index_path, (config["source_date_epoch"], config["source_date_epoch"]))
    return receipt(destination)


def receipt(destination: Path) -> dict:
    files = inventory(destination)
    return {"inventory_sha256": hashlib.sha256(canonical_json(files)).hexdigest(),
            "files": len(files), "bytes": sum(value["bytes"] for value in files.values())}


def run_operation(operation: str) -> None:
    if operation not in ("assemble", "verify"):
        raise ValueError("unsupported production operation")
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute() or not stat.S_ISDIR(project.lstat().st_mode):
        raise ValueError("invalid admitted project context")
    raw = sys.stdin.buffer.read(262145)
    if len(raw) > 262144:
        raise ValueError("production parameters exceed their bound")
    request = json.loads(raw)
    resolved = request.get("resolved_config") if isinstance(request, dict) else None
    if not isinstance(resolved, dict) or set(resolved) != {INPUT_CONFIG, SOURCE_CONFIG, SUPPORT_CONFIG}:
        raise ValueError("missing exact production Configs")
    config = resolved[INPUT_CONFIG]
    source_config = resolved[SOURCE_CONFIG]
    support_config = resolved[SUPPORT_CONFIG]
    validate_config(config)
    parent = project / "products"
    if not parent.exists():
        parent.mkdir(mode=0o700)
    ordinary_member(project, "products", directory=True)
    destination = project.joinpath(*OUTPUT.parts)
    if operation == "assemble" and (destination.exists() or destination.is_symlink()):
        raise ValueError("authoring output already exists; use a fresh production workspace")
    staging = parent / ("authoring-assembly" if operation == "assemble" else "authoring-verification")
    result = assemble(INPUT_ROOT, BUILT_UTILITIES_ROOT, staging, config,
                      source_config, support_config)
    if operation == "assemble":
        # Fail closed on reuse. No existing successful output is overwritten.
        if destination.exists() or destination.is_symlink():
            raise ValueError("authoring output already exists; use a fresh production workspace")
        staging.rename(destination)
    elif operation == "verify":
        actual = receipt(ordinary_member(project, OUTPUT.as_posix(), directory=True))
        if actual != result:
            raise ValueError("authoring output differs from independent reproduction")
    else:
        raise ValueError("unsupported production operation")
    print(json.dumps({"ok": True, "operation": operation, "output_path": OUTPUT.as_posix(),
                      **result, "binding_published": False}, sort_keys=True))
