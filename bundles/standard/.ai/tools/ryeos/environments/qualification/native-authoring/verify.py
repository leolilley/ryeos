# ryeos:signed:2026-09-08T08:44:58Z:684f709a6c0002d683cc3b2ba56595bed47a892a45af122b9a80c1d78bf991bd:F60sdn47w1nLQc3JyR7G6JuKfeV1bqS0qarkSIdRDA/CXYfFADktIkjw7vbO3pivi+zVDmmYiSTJgUObzF92Dw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/environments/qualification/native-authoring
#   version: "1.0.0"
#   description: Independently inspect and exercise the exact selected native authoring runtime
#   executor_id: tool:ryeos/environments/qualification/native-authoring/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#   external_product_slots:
#     - id: authoring-runtime
#       relationship_ref: config:development/ryeos/authoring-environment-products
#       relationship: runtime_to_native_authoring_qualification
#       kind: tree
#       mount_root: execution_runtime
#       mount: authoring-tools
#     - id: prepared-inputs
#       relationship_ref: config:development/ryeos/authoring-prepared-input-products
#       relationship: prepared_inputs_to_native_authoring_qualification
#       kind: tree
#       mount_root: execution_runtime
#       mount: authoring-inputs
"""Independent finite qualification probe for the selected authoring runtime."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import threading


SUBJECT_ID = "authoring-runtime"
SUPPORT_ID = "prepared-inputs"
PYTHON_ID = "producer-python"
SUBJECT_ROOT = Path("/ryeos/realizations/authoring-tools")
SUPPORT_ROOT = Path("/ryeos/realizations/authoring-inputs")
PYTHON_MANIFEST = "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf"
RUNTIME_ROOT = SUBJECT_ROOT.as_posix()
MAX_REALIZATION_WIRE_BYTES = 64 * 1024
MAX_ENTRIES = 1024
MAX_DEPTH = 32
MAX_FILE_BYTES = 128 * 1024 * 1024
MAX_TOTAL_BYTES = 256 * 1024 * 1024
MAX_DIAGNOSTIC_BYTES = 2 * 1024 * 1024
MAX_SUBPROCESS_INPUT_BYTES = 64 * 1024
MAX_ELF_OBJECTS = 128
HASH = re.compile(r"[0-9a-f]{64}")
NEEDED = re.compile(r"\(NEEDED\).*Shared library: \[([^]]+)\]")
SEARCH_PATH = re.compile(r"\((RPATH|RUNPATH)\).*Library (?:rpath|runpath): \[([^]]*)\]")
INTERPRETER = re.compile(r"Requesting program interpreter: ([^]]+)\]")
REQUIRED_COMMANDS = frozenset("""
awk basename cat chmod cmp cp cut date diff dirname env find git grep head ln ls
mkdir mktemp mv patch printf pwd readlink realpath rg rm rmdir sed sha256sum sleep
sort stat tail tee test timeout touch tr uniq wc xargs zsh
""".split())
INSPECTOR_FILES = {
    "elf/bin/readelf": (777864, 0o755, "4a387d80a70e6e97ef940a666fab59d5c38e7cf19ff907192fa983cb8505a7de"),
    "elf/lib/ld-linux-x86-64.so.2": (225672, 0o755, "438c546d8e8cc48496bf3a95f753051afd9db66a629a74e31a9ded71586b56e0"),
    "elf/lib/libc.so.6": (1995216, 0o644, "fa430b8f298f817a266046af84a77533185ad6fc4406c7d3787b5a0a0c207826"),
    "elf/lib/libctf-nobfd.so.0": (215992, 0o644, "69e10bcecd3c354805887f5d848c5dc8e15f36fc6e36b91de62063c12db3bf7b"),
    "elf/lib/libdl.so.2": (14408, 0o644, "295fa521a03cd2faa99974f378c9e23dd622021ec7d32bcad4f8ea61aec8a872"),
    "elf/lib/libgcc_s.so.1": (182856, 0o644, "30c61ab012a4241bed033725a09b61f5fdd3bb7df95ee852d0b096520524c7af"),
    "elf/lib/libm.so.6": (977112, 0o644, "6d567d53e895273ca14a1f9dc164fc6c8d39aed2f60aa46a733c2784228915f3"),
    "elf/lib/libpthread.so.0": (14408, 0o644, "85e21f7dba0394411d00959176fd18b470e575b0b05f1f4f41e5636802ce0500"),
    "elf/lib/librt.so.1": (14552, 0o644, "7b7b84d1aedda0e0b2bdfc68844362782132180b8f02be1300e21e77572e514b"),
    "elf/lib/libsframe.so.1": (30976, 0o644, "adb0b9a33909e7264ad724e138eb97995832e535b22b192c1a898c349c918a9f"),
    "elf/lib/libstdc++.so.6": (2497768, 0o644, "972bb2a18b71140dab0240f8a1f68ab3fb1d56bcd4c4f824a91b70888faf5a00"),
    "elf/lib/libtinfo.so.6": (220464, 0o644, "4013e7651ac68f547d8333517def25b2366c97eec7de7f06e105dbf3444ffebf"),
    "elf/lib/libutil.so.1": (14408, 0o644, "e3981d10efd152a53f083e38f5f9ddde7a049c85d0ab388a5eec91b52fc98a11"),
    "elf/lib/libz.so.1": (125376, 0o644, "85590dd58edf5445e18bc7193e5ebc01ac5841f1ae187e97705a662e90c6421e"),
    "elf/lib/libzstd.so.1": (825336, 0o644, "27f07c9a49c2c956bcfb64cd4712976586a66facbf15fc7f09bc37413b5f2b21"),
}


def canonical(value: object) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode()


def digest(value: object) -> str:
    return hashlib.sha256(canonical(value)).hexdigest()


def portable_mode(mode: int) -> int:
    if not stat.S_ISREG(mode):
        raise ValueError("portable mode applies only to regular files")
    return 0o755 if mode & 0o111 else 0o644


def load_realizations(raw: str) -> tuple[dict, dict]:
    if not isinstance(raw, str) or not raw or len(raw.encode()) > MAX_REALIZATION_WIRE_BYTES:
        raise ValueError("missing or oversized sealed realization set")
    value = json.loads(raw)
    if not isinstance(value, list) or not 1 <= len(value) <= 16:
        raise ValueError("sealed realization set must be a bounded array")
    required_keys = {
        "id", "kind", "mode", "manifest_hash", "entry_count", "total_bytes",
        "mount_root", "mount",
    }
    by_id = {}
    for item in value:
        if not isinstance(item, dict) or set(item) != required_keys:
            raise ValueError("sealed realization has an incomplete shape")
        name = item["id"]
        if not isinstance(name, str) or name in by_id or not HASH.fullmatch(item["manifest_hash"]):
            raise ValueError("sealed realization has an invalid identity")
        if (item["kind"] != "tree" or item["mode"] != "pinned" or
                item["mount_root"] != "execution_runtime" or
                type(item["entry_count"]) is not int or
                type(item["total_bytes"]) is not int):
            raise ValueError("sealed realization is not one exact pinned runtime tree")
        by_id[name] = item
    if set(by_id) != {SUBJECT_ID, SUPPORT_ID, PYTHON_ID}:
        raise ValueError("qualification admitted an unexpected realization set")
    subject, support, python = by_id[SUBJECT_ID], by_id[SUPPORT_ID], by_id[PYTHON_ID]
    if subject["mount"] != "authoring-tools" or support["mount"] != "authoring-inputs":
        raise ValueError("qualification realization mount changed")
    if (python["mount"] != "producer-python" or
            python["manifest_hash"] != PYTHON_MANIFEST):
        raise ValueError("qualification bootstrap Python changed")
    if (not 1 <= subject["entry_count"] <= MAX_ENTRIES or
            not 1 <= subject["total_bytes"] <= MAX_TOTAL_BYTES or
            not 1 <= support["entry_count"] <= MAX_ENTRIES or
            not 1 <= support["total_bytes"] <= MAX_TOTAL_BYTES):
        raise ValueError("qualification realization exceeds its finite allowance")
    return subject, support


def scan_runtime(root: Path) -> tuple[list[dict], dict[str, Path]]:
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("selected runtime root is not an ordinary directory")
    pending = [(root, 0)]
    entries = []
    files = {}
    total = 0
    while pending:
        directory, depth = pending.pop()
        if depth > MAX_DEPTH:
            raise ValueError("selected runtime exceeds its depth bound")
        for child in sorted(os.scandir(directory), key=lambda item: item.name, reverse=True):
            path = Path(child.path)
            relative = path.relative_to(root).as_posix()
            metadata = child.stat(follow_symlinks=False)
            if stat.S_ISDIR(metadata.st_mode):
                entries.append({"kind": "dir", "path": relative})
                pending.append((path, depth + 1))
            elif stat.S_ISREG(metadata.st_mode):
                if metadata.st_size > MAX_FILE_BYTES:
                    raise ValueError(f"runtime member exceeds its file bound: {relative}")
                total += metadata.st_size
                if total > MAX_TOTAL_BYTES:
                    raise ValueError("selected runtime exceeds its total-byte bound")
                before = (metadata.st_dev, metadata.st_ino, metadata.st_size,
                          metadata.st_mtime_ns)
                with path.open("rb") as source:
                    file_hash = hashlib.file_digest(source, "sha256").hexdigest()
                after = path.stat(follow_symlinks=False)
                if before != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
                    raise ValueError(f"runtime member changed while inspected: {relative}")
                entries.append({
                    "bytes": metadata.st_size,
                    "kind": "file",
                    "mode": portable_mode(metadata.st_mode),
                    "path": relative,
                    "sha256": file_hash,
                })
                files[relative] = path
            else:
                raise ValueError(f"runtime contains a link or special member: {relative}")
            if len(entries) > MAX_ENTRIES:
                raise ValueError("selected runtime exceeds its entry bound")
    entries.sort(key=lambda item: item["path"].encode())
    return entries, files


def _bounded_run(command: list[str], *, input_data: bytes | None = None,
                 environment: dict[str, str] | None = None) -> bytes:
    if not command or any(not isinstance(item, str) or not item.startswith("/") and index == 0
                          for index, item in enumerate(command)):
        raise ValueError("qualification subprocess requires an exact executable")
    if input_data is not None and len(input_data) > MAX_SUBPROCESS_INPUT_BYTES:
        raise ValueError("qualification subprocess input exceeds its byte bound")
    with tempfile.TemporaryFile() as stdin:
        if input_data is not None:
            stdin.write(input_data)
            stdin.seek(0)
        with subprocess.Popen(
            command, stdin=stdin if input_data is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            env=environment or {"LANG": "C", "LC_ALL": "C", "PATH": ""},
        ) as process:
            observed = []

            def read_output() -> None:
                observed.append(process.stdout.read(MAX_DIAGNOSTIC_BYTES + 1))

            reader = threading.Thread(target=read_output, daemon=True)
            reader.start()
            try:
                reader.join(30)
                if reader.is_alive():
                    raise ValueError("qualification subprocess exceeded its time bound")
                output = observed[0]
                if len(output) > MAX_DIAGNOSTIC_BYTES:
                    raise ValueError("qualification diagnostic exceeds its byte bound")
                status = process.wait(timeout=1)
                if status:
                    raise ValueError(f"qualification subprocess refused: {command[0]}")
                return output
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                reader.join(1)


def _readelf(path: Path, support: Path) -> str:
    loader = support / "elf/lib/ld-linux-x86-64.so.2"
    readelf = support / "elf/bin/readelf"
    for executable in (loader, readelf):
        mode = executable.lstat().st_mode
        if not stat.S_ISREG(mode) or not mode & 0o111:
            raise ValueError("prepared input ELF inspector is absent or not executable")
    output = _bounded_run([
        str(loader), "--inhibit-cache", "--glibc-hwcaps-mask", "", "--library-path",
        str(support / "elf/lib"),
        str(readelf), "-hW", "-lW", "-dW", str(path),
    ])
    return output.decode("utf-8", errors="strict")


def verify_inspector(support: Path) -> str:
    observed = {}
    for relative, (expected_bytes, expected_mode, expected_hash) in INSPECTOR_FILES.items():
        path = support / relative
        metadata = path.lstat()
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_size != expected_bytes or
                portable_mode(metadata.st_mode) != expected_mode):
            raise ValueError(f"prepared inspector member has the wrong shape: {relative}")
        before = (metadata.st_dev, metadata.st_ino, metadata.st_size, metadata.st_mtime_ns)
        with path.open("rb") as source:
            actual_hash = hashlib.file_digest(source, "sha256").hexdigest()
        after = path.stat(follow_symlinks=False)
        if before != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
            raise ValueError(f"prepared inspector member changed while checked: {relative}")
        if actual_hash != expected_hash:
            raise ValueError(f"prepared inspector member has the wrong identity: {relative}")
        observed[relative] = {
            "bytes": expected_bytes, "mode": expected_mode, "sha256": actual_hash,
        }
    return digest(observed)


def inspect_elf_closure(root: Path, support: Path, files: dict[str, Path]) -> list[dict]:
    libraries = {Path(name).name for name in files if name.startswith("lib/")}
    facts = []
    for name, path in sorted(files.items()):
        with path.open("rb") as source:
            elf = source.read(4) == b"\x7fELF"
        if name.startswith(("bin/", "lib/")) and not elf:
            raise ValueError(f"runtime executable/library is not ELF: {name}")
        if not elf:
            continue
        if len(facts) >= MAX_ELF_OBJECTS:
            raise ValueError("runtime ELF object count exceeds its bound")
        output = _readelf(path, support)
        if "Machine:                           Advanced Micro Devices X86-64" not in output:
            raise ValueError(f"runtime ELF is not x86_64: {name}")
        stack_lines = [line.split() for line in output.splitlines() if "GNU_STACK" in line]
        if len(stack_lines) != 1:
            raise ValueError(f"runtime ELF has no unique GNU_STACK contract: {name}")
        stack_flags = [token for token in stack_lines[0] if re.fullmatch(r"[RWE]+", token)]
        if len(stack_flags) != 1 or "E" in stack_flags[0]:
            raise ValueError(f"runtime ELF requests an executable stack: {name}")
        interpreters = INTERPRETER.findall(output)
        if len(interpreters) > 1 or (interpreters and
                interpreters[0] != RUNTIME_ROOT + "/lib/ld-linux-x86-64.so.2"):
            raise ValueError(f"runtime ELF has an unclosed interpreter: {name}")
        needed = sorted(NEEDED.findall(output))
        if any(value not in libraries for value in needed):
            raise ValueError(f"runtime ELF has an unavailable dependency: {name}")
        search = SEARCH_PATH.findall(output)
        if any(kind == "RPATH" for kind, _ in search) or len(search) > 1:
            raise ValueError(f"runtime ELF has an unsupported search path: {name}")
        if needed or interpreters:
            if search != [("RUNPATH", RUNTIME_ROOT + "/lib")]:
                raise ValueError(f"runtime ELF lacks the exact admitted RUNPATH: {name}")
            if "NODEFLIB" not in output:
                raise ValueError(f"runtime ELF permits host default libraries: {name}")
        facts.append({
            "interpreter": interpreters,
            "needed": needed,
            "path": name,
            "runpath": [value for _, value in search],
            "stack": stack_flags[0],
        })
    return facts


def exercise_runtime(root: Path) -> dict:
    binary = root / "bin"
    environment = {
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_NOSYSTEM": "1",
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": str(binary),
        "TZ": "UTC",
    }
    with tempfile.TemporaryDirectory(prefix="ryeos-native-authoring-probe-") as directory:
        scratch = Path(directory)
        source = scratch / "input"
        source.write_bytes(b"alpha\nbeta\n")
        second = _bounded_run([str(binary / "sed"), "-n", "2p", str(source)],
                              environment=environment)
        if _bounded_run([str(binary / "grep"), "-x", "beta"], input_data=second,
                        environment=environment) != b"beta\n":
            raise ValueError("selected sed/grep pipeline changed")
        copied = scratch / "copied"
        _bounded_run([str(binary / "cp"), str(source), str(copied)], environment=environment)
        _bounded_run([str(binary / "cmp"), str(source), str(copied)], environment=environment)
        checksum = _bounded_run([str(binary / "sha256sum"), str(copied)],
                                environment=environment).split()[0].decode()
        if checksum != hashlib.sha256(source.read_bytes()).hexdigest():
            raise ValueError("selected sha256sum changed")
        found = _bounded_run(
            [str(binary / "find"), str(scratch), "-type", "f", "-printf", "%f\n"],
            environment=environment,
        )
        sorted_names = _bounded_run([str(binary / "sort")], input_data=found,
                                    environment=environment)
        if sorted_names != b"copied\ninput\n":
            raise ValueError("selected find/sort behavior changed")
        _bounded_run([str(binary / "git"), "init", "--quiet", str(scratch / "repo")],
                     environment={**environment, "HOME": str(scratch)})
        shell = _bounded_run([
            str(root / "lib/ld-linux-x86-64.so.2"), "--inhibit-cache", "--library-path",
            str(root / "lib"), str(binary / "zsh"), "-f", "-c",
            '[[ "$("$1/bin/printf" native)" = native ]] && "$1/bin/test" -x "$1/bin/git"',
            "qualification", str(root),
        ], environment=environment)
        if shell:
            raise ValueError("selected shell probe returned unexpected output")
        return {
            "file_sha256": checksum,
            "names_sha256": hashlib.sha256(sorted_names).hexdigest(),
            "shell_exit": 0,
        }


def qualify(realizations: str, subject_root: Path = SUBJECT_ROOT,
            support_root: Path = SUPPORT_ROOT) -> dict:
    subject, _support = load_realizations(realizations)
    inspector_digest = verify_inspector(support_root)
    entries, files = scan_runtime(subject_root)
    if len(entries) != subject["entry_count"] or sum(
            entry.get("bytes", 0) for entry in entries) != subject["total_bytes"]:
        raise ValueError("materialized runtime metrics contradict the sealed manifest")
    top_level = {entry["path"].split("/", 1)[0] for entry in entries}
    if top_level != {"bin", "lib", "licenses"}:
        raise ValueError("runtime has an unexpected top-level layout")
    commands = {Path(name).name for name in files if name.startswith("bin/") and "/" not in name[4:]}
    if commands != REQUIRED_COMMANDS:
        raise ValueError("runtime command contract is incomplete or contains an extra command")
    if any(portable_mode(files["bin/" + name].lstat().st_mode) != 0o755
           for name in REQUIRED_COMMANDS):
        raise ValueError("runtime command is not executable")
    elf_facts = inspect_elf_closure(subject_root, support_root, files)
    probe = exercise_runtime(subject_root)
    return {
        "schema": "ryeos.product_qualification_result.v1",
        "subject_manifest_hash": subject["manifest_hash"],
        "claims": ["authoring_runtime_closed"],
        "probe_evidence": {
            "schema": "ryeos.native_authoring_probe.v1",
            "command_contract_digest": digest(sorted(REQUIRED_COMMANDS)),
            "command_count": len(REQUIRED_COMMANDS),
            "elf_closure_digest": digest(elf_facts),
            "elf_count": len(elf_facts),
            "inspector_identity_digest": inspector_digest,
            "network_contacted": False,
            "runtime_inventory_digest": digest(entries),
            "runtime_probe_digest": digest(probe),
        },
    }


def main() -> None:
    result = qualify(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", ""))
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
