# ryeos:signed:2026-09-29T03:29:28Z:e0042fb83a61634130924a45a0b867919575dce1b17a6559f1304d16e6a3bb98:+gj0J+mLMWvz9f0akHspR9DYkf8gfl+tPS7esETytggnmP3Yv7o938QAWkEciO18jjwEE8CNGZtxJIkJLNeWAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/environments/qualification
#   version: "2.0.0"
#   description: Qualify a retained x86-64 GNU platform against its admitted version profile
#   executor_id: "@subprocess"
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     additionalProperties: false
#     required: [schema, bootstrap, tool_versions]
#     properties:
#       schema: {const: ryeos.development_platform_profile.v2}
#       bootstrap:
#         type: object
#         additionalProperties: false
#         required: [schema, artifact_class, execution_gate, target, rust_version, rust_host, zig_version, source_date_epoch, publisher_image, input_contract_body_sha256, producer_sha256, runtime_helper_sha256]
#         properties:
#           schema: {const: ryeos.development-toolchain-bootstrap.v2}
#           artifact_class: {const: runtime_closed_platform_candidate}
#           execution_gate: {const: target_local_binding_and_isolated_acceptance_required}
#           target: {const: x86_64-unknown-linux-gnu}
#           rust_version: {type: string, minLength: 5, maxLength: 14, pattern: '^(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})$'}
#           rust_host: {const: x86_64-unknown-linux-gnu}
#           zig_version: {type: string, minLength: 5, maxLength: 14, pattern: '^(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})$'}
#           source_date_epoch: {type: string, minLength: 1, maxLength: 256, pattern: '^[0-9]+$'}
#           publisher_image: {type: string, pattern: '^docker\.io/library/rust@sha256:[0-9a-f]{64}$'}
#           input_contract_body_sha256: {type: string, pattern: '^[0-9a-f]{64}$'}
#           producer_sha256: {type: string, pattern: '^[0-9a-f]{64}$'}
#           runtime_helper_sha256: {type: string, pattern: '^[0-9a-f]{64}$'}
#       tool_versions:
#         type: object
#         required: [cargo, gcc, collect2, linker, archiver]
#         additionalProperties: false
#         properties:
#           cargo: {type: string, minLength: 1, maxLength: 512, pattern: '^[ -~]+$'}
#           gcc: {type: string, minLength: 1, maxLength: 512, pattern: '^[ -~]+$'}
#           collect2: {type: string, minLength: 1, maxLength: 512, pattern: '^[ -~]+$'}
#           linker: {type: string, minLength: 1, maxLength: 512, pattern: '^[ -~]+$'}
#           archiver: {type: string, minLength: 1, maxLength: 512, pattern: '^[ -~]+$'}
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#   external_product_slots:
#     - id: subject
#       relationship_ref: config:development/ryeos/platform-products
#       relationship: platform_to_qualification_verifier
#       kind: tree
#       mount_root: execution_runtime
#       mount: platform
#   env_config:
#     # The bootstrap Python is dynamically linked against its retained musl
#     # loader. Enter it through that loader; isolated execution deliberately
#     # has no ambient /lib/ld-musl-x86_64.so.1.
#     interpreter: {type: realization_member, realization_id: producer-python, relative_path: lib/ld-musl-x86_64.so.1}
#   config:
#     command: "${interpreter}"
#     args: ["--library-path", "/ryeos/realizations/producer-python/lib", "/ryeos/realizations/producer-python/python/bin/python3.14", "-I", "-B", "${tool_path}"]
#     input_data: "${params_json}"
#     timeout_secs: 120
"""Finite, independent qualification of the selected development platform."""

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import struct
import subprocess
import tempfile


ROOT = Path("/ryeos/realizations/platform")
HASH = re.compile(r"[0-9a-f]{64}")
RELEASE = re.compile(r"(?:0|[1-9][0-9]{0,3})\.(?:0|[1-9][0-9]{0,3})\.(?:0|[1-9][0-9]{0,3})")
MAX_OUTPUT = 64 * 1024
PROBE_ENVIRONMENT = {"LANG": "C", "LC_ALL": "C", "PATH": ""}
QUALIFICATION_CLAIMS = (
    "development_platform_profile_v2",
    "development_platform_target_x86_64_linux_gnu_v1",
)
REQUIRED_EXECUTABLES = (
    "rust/bin/cargo", "rust/bin/rustc", "rust/bin/rustdoc",
    "native/bin/ar", "native/bin/collect2", "native/bin/gcc", "native/bin/ld.lld",
    "zig/zig",
)
REQUIRED_ABI_MEMBERS = (
    "lib/ld-linux-x86-64.so.2", "lib/libc.so.6", "lib/libc_nonshared.a",
    "lib/crtbeginS.o", "lib/crtendS.o", "lib/Scrt1.o", "lib/crti.o", "lib/crtn.o",
)
EXPECTED_TOP_LEVEL = {
    "RYEOS-AUTHORING-PROGRAMS", "RYEOS-BOOTSTRAP", "RYEOS-ELF-TRANSFORMS",
    "RYEOS-RUNTIME-DEPENDENCIES", "RYEOS-RUNTIME-SOURCES", "RYEOS-TREE-SHA256",
    "UPSTREAM-RUST-MANIFEST.toml", "lib", "native", "rust", "share", "sysroot", "zig",
}
BOOTSTRAP_FIELDS = frozenset({
    "schema", "artifact_class", "execution_gate", "target", "rust_version",
    "rust_host", "zig_version", "source_date_epoch", "publisher_image",
    "input_contract_body_sha256", "producer_sha256", "runtime_helper_sha256",
})
TOOL_VERSION_FIELDS = frozenset({"cargo", "gcc", "collect2", "linker", "archiver"})


def expected_bootstrap(params):
    if not isinstance(params, dict) or set(params) != {"schema", "bootstrap", "tool_versions"} or \
            params["schema"] != "ryeos.development_platform_profile.v2":
        raise ValueError("unsupported development platform profile")
    bootstrap = params["bootstrap"]
    if not isinstance(bootstrap, dict) or set(bootstrap) != BOOTSTRAP_FIELDS or \
            any(not isinstance(value, str) or not value or len(value) > 256
                for value in bootstrap.values()):
        raise ValueError("development platform bootstrap expectation is malformed")
    if (bootstrap["schema"] != "ryeos.development-toolchain-bootstrap.v2" or
            bootstrap["artifact_class"] != "runtime_closed_platform_candidate" or
            bootstrap["execution_gate"] != "target_local_binding_and_isolated_acceptance_required" or
            bootstrap["target"] != "x86_64-unknown-linux-gnu" or
            bootstrap["rust_host"] != bootstrap["target"] or
            not RELEASE.fullmatch(bootstrap["rust_version"]) or
            not RELEASE.fullmatch(bootstrap["zig_version"]) or
            not bootstrap["source_date_epoch"].isdigit() or
            not bootstrap["publisher_image"].startswith("docker.io/library/rust@sha256:") or
            not HASH.fullmatch(bootstrap["publisher_image"].removeprefix("docker.io/library/rust@sha256:")) or
            any(not HASH.fullmatch(bootstrap[name]) for name in (
                "input_contract_body_sha256", "producer_sha256", "runtime_helper_sha256"))):
        raise ValueError("development platform bootstrap expectation is unsupported")
    versions = params["tool_versions"]
    if not isinstance(versions, dict) or set(versions) != TOOL_VERSION_FIELDS or any(
            not isinstance(value, str) or not 1 <= len(value) <= 512 or
            value != value.strip() or any(ord(character) < 32 or ord(character) > 126
                                          for character in value)
            for value in versions.values()):
        raise ValueError("development platform tool version expectations are malformed")
    # Cargo is shipped with the selected Rust distribution. Keep the release
    # coordinate consistent, while the full build identity remains signed data.
    cargo = versions["cargo"].split(" ")
    if len(cargo) < 2 or cargo[0] != "cargo" or cargo[1] != bootstrap["rust_version"]:
        raise ValueError("development platform Cargo release contradicts its Rust profile")
    return bootstrap


def realizations():
    value = json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "null"))
    if not isinstance(value, list) or len(value) != 2:
        raise ValueError("platform qualification requires verifier and subject")
    by_id = {item.get("id"): item for item in value if isinstance(item, dict)}
    if set(by_id) != {"producer-python", "subject"}:
        raise ValueError("platform qualification admitted unexpected realizations")
    python, subject = by_id["producer-python"], by_id["subject"]
    keys = {"id", "kind", "mode", "manifest_hash", "entry_count", "total_bytes", "mount_root", "mount"}
    if any(set(item) != keys or not isinstance(item["entry_count"], int) or
           not isinstance(item["total_bytes"], int) for item in value):
        raise ValueError("platform qualification realization identity is not canonical")
    # Signed Tool external_content pins the exact bootstrap runtime. The
    # daemon-authored sealed set supplies its selected identity here.
    if (python.get("kind") != "tree" or python.get("mode") != "pinned" or
            python.get("mount_root") != "execution_runtime" or
            python.get("mount") != "producer-python" or
            not isinstance(python.get("manifest_hash"), str) or
            not HASH.fullmatch(python["manifest_hash"])):
        raise ValueError("platform verifier runtime changed")
    if (subject.get("kind") != "tree" or subject.get("mode") != "pinned" or
            subject.get("mount_root") != "execution_runtime" or
            subject.get("mount") != "platform" or
            not HASH.fullmatch(subject.get("manifest_hash", ""))):
        raise ValueError("platform subject identity changed")
    if subject["manifest_hash"] == python["manifest_hash"]:
        raise ValueError("platform may not qualify itself")
    return subject


def regular(path, maximum):
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size > maximum:
            raise ValueError(f"invalid platform member: {path}")
        data = bytearray()
        while len(data) <= maximum:
            chunk = os.read(descriptor, min(65536, maximum + 1 - len(data)))
            if not chunk:
                break
            data.extend(chunk)
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    if len(data) > maximum or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
        raise ValueError(f"unstable platform member: {path}")
    return data


def stable_digest(path, maximum):
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size > maximum:
            raise ValueError(f"invalid platform member: {path}")
        digest = hashlib.sha256()
        length = 0
        while True:
            chunk = os.read(descriptor, 65536)
            if not chunk:
                break
            length += len(chunk)
            if length > maximum:
                raise ValueError(f"oversized platform member: {path}")
            digest.update(chunk)
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    if length != before.st_size or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
        raise ValueError(f"unstable platform member: {path}")
    return digest.hexdigest()


def elf_identity(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            raise ValueError(f"platform member is not regular: {path}")
        header = os.read(descriptor, 64)
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
        raise ValueError(f"platform member changed during inspection: {path}")
    if header[:7] != b"\x7fELF\x02\x01\x01":
        raise ValueError(f"platform member is not ELF64 little-endian: {path}")
    machine = struct.unpack_from("<H", header, 18)[0]
    if machine != 62:
        raise ValueError(f"platform member is not x86_64: {path}")
    return {"class": "ELF64", "endian": "little", "machine": "x86_64", "machine_id": machine}


def run_program(program, *arguments, environment=None, cwd=None):
    loader = str(ROOT / "lib/ld-linux-x86-64.so.2")
    command = [loader, "--library-path", str(ROOT / "lib"), str(program), *arguments]
    completed = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT,
                               env=PROBE_ENVIRONMENT if environment is None else environment,
                               cwd=cwd,
                               timeout=10, check=False)
    if len(completed.stdout) > MAX_OUTPUT:
        raise ValueError(f"platform tool identity probe exceeded its output bound: {program}")
    if completed.returncode:
        output = completed.stdout.decode("utf-8", "replace").strip()
        raise ValueError(
            f"platform tool identity probe failed ({completed.returncode}): {program}: {output}")
    return completed.stdout.decode("utf-8", "strict")


def cargo_identity():
    # RyeOS supplies the sandbox's private scratch through TMPDIR. Cargo gets
    # fresh state and cwd for this probe only; no project/user config is inherited.
    with tempfile.TemporaryDirectory(prefix="ryeos-platform-qualification-") as directory:
        scratch = Path(directory)
        probe_home, cargo_home = scratch / "home", scratch / "cargo"
        probe_home.mkdir()
        cargo_home.mkdir()
        environment = {**PROBE_ENVIRONMENT, "HOME": str(probe_home),
                       "CARGO_HOME": str(cargo_home), "CARGO_NET_OFFLINE": "true"}
        return run_program(ROOT / "rust/bin/cargo", "--version",
                           environment=environment, cwd=scratch).strip()


def gcc_identity(expected):
    # GCC reports its invoked basename. The retained executable is native/bin/gcc,
    # not the publisher image's x86_64-linux-gnu-gcc-14 path.
    version = run_program(ROOT / "native/bin/gcc", "--version").splitlines()[0]
    if version != expected:
        raise ValueError("platform native compiler identity changed")
    return version


def collect2_identity():
    # collect2 forwards --version to its linker too. Select the retained LLD
    # through GCC's supported selector/search path, without an ambient ld alias.
    environment = {**PROBE_ENVIRONMENT,
                   "COMPILER_PATH": str(ROOT / "native/bin")}
    return run_program(ROOT / "native/bin/collect2", "-fuse-ld=lld", "--version",
                       environment=environment).splitlines()[0]


def run_static_program(program, *arguments):
    completed = subprocess.run([str(program), *arguments], stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               env=PROBE_ENVIRONMENT,
                               timeout=10, check=False)
    if len(completed.stdout) > MAX_OUTPUT:
        raise ValueError(f"platform static tool identity probe exceeded its output bound: {program}")
    if completed.returncode:
        output = completed.stdout.decode("utf-8", "replace").strip()
        raise ValueError(
            f"platform static tool identity probe failed ({completed.returncode}): {program}: {output}")
    return completed.stdout.decode("utf-8", "strict")


def inventory(root, subject):
    entries, total = [], 0
    for directory, directories, files in os.walk(root, followlinks=False):
        directories.sort()
        files.sort()
        for name in directories + files:
            path = Path(directory, name)
            if path.is_symlink():
                raise ValueError(f"platform contains a symbolic link: {path}")
        for name in directories:
            path = Path(directory, name)
            metadata = path.stat(follow_symlinks=False)
            if not stat.S_ISDIR(metadata.st_mode):
                raise ValueError(f"platform contains a non-directory member: {path}")
            entries.append({"path": path.relative_to(root).as_posix(), "kind": "dir",
                            "bytes": 0, "mode": 0o755})
        for name in files:
            path = Path(directory, name)
            metadata = path.stat(follow_symlinks=False)
            if not stat.S_ISREG(metadata.st_mode):
                raise ValueError(f"platform contains a non-regular member: {path}")
            relative = path.relative_to(root).as_posix()
            digest = stable_digest(path, 268435456)
            normalized_mode = 0o755 if metadata.st_mode & 0o111 else 0o644
            entries.append({"path": relative, "kind": "file", "bytes": metadata.st_size,
                            "mode": normalized_mode, "sha256": digest})
            total += metadata.st_size
    entries.sort(key=lambda item: item["path"])
    if len(entries) != subject.get("entry_count") or total != subject.get("total_bytes"):
        raise ValueError("platform metrics contradict the sealed product manifest")
    if {entry["path"].split("/", 1)[0] for entry in entries} != EXPECTED_TOP_LEVEL:
        raise ValueError("platform top-level closure changed")
    return entries


def validate_bootstrap_testimony(expected):
    lines = regular(ROOT / "RYEOS-BOOTSTRAP", 16384).decode("utf-8", "strict").splitlines()
    if any(line.count("=") != 1 for line in lines):
        raise ValueError("platform bootstrap testimony is malformed")
    testimony = dict(line.split("=", 1) for line in lines)
    if testimony != expected:
        raise ValueError("platform bootstrap testimony changed")


def validate_tree_testimony(entries):
    lines = regular(ROOT / "RYEOS-TREE-SHA256", 4 * 1024 * 1024).decode(
        "utf-8", "strict").splitlines()
    testimony = {}
    for line in lines:
        fields = line.split("\t")
        if len(fields) != 4 or fields[0] not in {"d", "f"} or fields[3] in testimony:
            raise ValueError("platform retained tree testimony is malformed")
        testimony[fields[3]] = fields[:3]
    observed = {entry["path"]: entry for entry in entries if entry["path"] != "RYEOS-TREE-SHA256"}
    if set(testimony) != set(observed):
        raise ValueError("platform retained tree testimony paths changed")
    for path, entry in observed.items():
        kind, mode, digest = testimony[path]
        if entry["kind"] == "dir":
            # Large-content manifests preserve directory identity but
            # materialization deliberately normalizes directory modes.
            if (kind, mode, digest) != ("d", "755", "-"):
                raise ValueError("platform retained directory testimony changed")
        elif (kind, mode, digest) != ("f", f'{entry["mode"]:o}', entry["sha256"]):
            raise ValueError("platform retained file testimony contradicts its contents")


def validate_runtime_testimony(entries, contracts):
    by_contract = {item["path"]: item for item in contracts}
    rows = []
    for entry in entries:
        relative = entry["path"]
        if entry["kind"] != "file" or relative.split("/", 1)[0] not in {"rust", "zig", "lib", "native"}:
            continue
        contract = by_contract.get(relative)
        if contract is not None:
            interpreter = contract["interpreter"] or "-"
            needed = ",".join(contract["needed"]) or "-"
            rows.append(f'{entry["sha256"]}\t{relative}\telf\t{interpreter}\t{needed}\n')
        elif entry["mode"] & 0o111:
            rows.append(f'{entry["sha256"]}\t{relative}\tnon_elf\t-\t-\n')
    if regular(ROOT / "RYEOS-RUNTIME-DEPENDENCIES", 4 * 1024 * 1024) != "".join(rows).encode():
        raise ValueError("platform runtime-dependency testimony contradicts its contents")


def elf_dynamic_contract(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        header = os.read(descriptor, 7)
    finally:
        os.close(descriptor)
    if header[:4] != b"\x7fELF":
        return None
    data = regular(path, 268435456)
    if (len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01" or
            struct.unpack_from("<H", data, 18)[0] != 62):
        raise ValueError(f"platform contains a non-x86-64 ELF member: {path}")
    phoff = struct.unpack_from("<Q", data, 32)[0]
    phentsize, phnum = struct.unpack_from("<HH", data, 54)
    interpreter = None
    if phnum:
        if phentsize < 56 or phoff > len(data) or phnum > (len(data) - phoff) // phentsize:
            raise ValueError(f"platform ELF has an invalid program-header table: {path}")
        for offset in range(phoff, phoff + phentsize * phnum, phentsize):
            kind, _, file_offset, _, _, file_size = struct.unpack_from("<IIQQQQ", data, offset)
            if kind == 3:
                if file_offset > len(data) or file_size > len(data) - file_offset:
                    raise ValueError(f"platform ELF has an invalid interpreter range: {path}")
                interpreter = data[file_offset:file_offset + file_size].rstrip(b"\0").decode(
                    "utf-8", "strict")
    shoff = struct.unpack_from("<Q", data, 40)[0]
    shentsize, shnum = struct.unpack_from("<HH", data, 58)
    if shnum and (shentsize < 64 or shoff > len(data) or shnum > (len(data) - shoff) // shentsize):
        raise ValueError(f"platform ELF has an invalid section-header table: {path}")
    sections = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + index * shentsize)
                for index in range(shnum)]
    needed, search = [], []
    for section in sections:
        if section[1] != 6:
            continue
        if section[6] >= len(sections):
            raise ValueError(f"platform ELF dynamic string table is absent: {path}")
        strings_section = sections[section[6]]
        if (strings_section[4] > len(data) or
                strings_section[5] > len(data) - strings_section[4] or
                section[4] > len(data) or section[5] > len(data) - section[4]):
            raise ValueError(f"platform ELF dynamic data is out of bounds: {path}")
        strings = data[strings_section[4]:strings_section[4] + strings_section[5]]
        dynamic = data[section[4]:section[4] + section[5]]
        entry_size = section[9] or 16
        if entry_size < 16:
            raise ValueError(f"platform ELF dynamic entry is invalid: {path}")
        for offset in range(0, len(dynamic) - 15, entry_size):
            tag, value = struct.unpack_from("<QQ", dynamic, offset)
            if tag == 0:
                break
            if tag in (1, 15, 29):
                if value >= len(strings):
                    raise ValueError(f"platform ELF dynamic string is out of bounds: {path}")
                end = strings.find(b"\0", value)
                if end < 0:
                    raise ValueError(f"platform ELF dynamic string is unterminated: {path}")
                text = strings[value:end].decode("utf-8", "strict")
                if tag == 1:
                    needed.append(text)
                else:
                    search.append(text)
    return {"path": path.relative_to(ROOT).as_posix(), "interpreter": interpreter,
            "needed": sorted(needed), "search": search}


def validate_dynamic_closure(root, relative, contract, runtime_directories):
    expected_loader = str(root / "lib/ld-linux-x86-64.so.2")
    if contract["interpreter"] not in (None, expected_loader):
        raise ValueError(f"platform ELF has an unclosed interpreter: {relative}")
    for value in contract["search"]:
        for component in value.split(":"):
            if not component.startswith(str(root) + "/") or "$" in component:
                raise ValueError(f"platform ELF has an unclosed search path: {relative}")
            resolved = Path(component).resolve(strict=True)
            if root not in resolved.parents:
                raise ValueError(f"platform ELF search path escapes its realization: {relative}")
    for member in contract["needed"]:
        if "/" in member or member in (".", ".."):
            raise ValueError(f"platform ELF dependency is not a basename: {relative}")
        candidates = []
        for directory in runtime_directories:
            candidate = directory / member
            if candidate.is_file() and not candidate.is_symlink():
                resolved = candidate.resolve(strict=True)
                if root not in resolved.parents:
                    raise ValueError(f"platform ELF dependency escapes its realization: {relative}")
                candidates.append(resolved)
        if len(set(candidates)) != 1:
            raise ValueError(f"platform ELF dependency is absent or ambiguous: {relative}: {member}")


def probe_compilers(bootstrap, expected_versions):
    """Measure every supported tool identity and compare with signed expectations."""
    rustc = ROOT / "rust/bin/rustc"
    version = run_program(rustc, "-vV")
    fields = dict(line.split(": ", 1) for line in version.splitlines() if ": " in line)
    if fields.get("release") != bootstrap["rust_version"] or fields.get("host") != bootstrap["rust_host"]:
        raise ValueError("platform compiler target or release changed")
    cargo_version = cargo_identity()
    gcc_version = gcc_identity(expected_versions["gcc"])
    collect2_version = collect2_identity()
    linker_version = run_program(ROOT / "native/bin/ld.lld", "--version").strip()
    archiver_version = run_program(ROOT / "native/bin/ar", "--version").splitlines()[0]
    zig_version = run_static_program(ROOT / "zig/zig", "version").strip()
    if cargo_version != expected_versions["cargo"]:
        raise ValueError("platform Cargo identity changed")
    if collect2_version != expected_versions["collect2"]:
        raise ValueError("platform native compiler driver identity changed")
    if linker_version != expected_versions["linker"]:
        raise ValueError("platform linker identity changed")
    if archiver_version != expected_versions["archiver"]:
        raise ValueError("platform archiver identity changed")
    if zig_version != bootstrap["zig_version"]:
        raise ValueError("platform Zig identity changed")
    return fields, {
        "implementation": "rustc", "release": fields["release"],
        "commit_hash": fields.get("commit-hash"), "cargo": cargo_version,
        "gcc": gcc_version, "collect2": collect2_version,
        "linker": linker_version, "archiver": archiver_version, "zig": zig_version,
    }


def execute(params):
    bootstrap = expected_bootstrap(params)
    subject = realizations()
    entries = inventory(ROOT, subject)
    validate_bootstrap_testimony(bootstrap)
    validate_tree_testimony(entries)
    by_path = {entry["path"]: entry for entry in entries if entry["kind"] == "file"}
    for relative in REQUIRED_EXECUTABLES:
        if relative not in by_path or by_path[relative]["mode"] & 0o111 == 0:
            raise ValueError(f"required platform executable is absent: {relative}")
    for relative in REQUIRED_ABI_MEMBERS:
        if relative not in by_path:
            raise ValueError(f"required platform ABI member is absent: {relative}")
    runtime_directories = [
        ROOT / "lib",
        ROOT / "rust/lib",
        ROOT / "rust/lib/rustlib/x86_64-unknown-linux-gnu/lib",
    ]
    elf_contracts = []
    for relative in sorted(by_path):
        contract = elf_dynamic_contract(ROOT / relative)
        if contract is None:
            continue
        validate_dynamic_closure(ROOT, relative, contract, runtime_directories)
        elf_contracts.append(contract)
    validate_runtime_testimony(entries, elf_contracts)
    identity = elf_identity(ROOT / "rust/bin/rustc")
    fields, compiler = probe_compilers(bootstrap, params["tool_versions"])
    rustlib = ROOT / "rust/lib/rustlib/x86_64-unknown-linux-gnu"
    if not rustlib.is_dir():
        raise ValueError("platform target standard library is absent")
    evidence = {
        "schema": "ryeos.development_platform_evidence.v2",
        "target": {"triple": fields["host"], "architecture": "x86_64", "operating_system": "linux", "abi": "gnu"},
        "compiler": compiler,
        "executable": identity,
        "abi_members": {
            "loader_sha256": hashlib.sha256(regular(ROOT / "lib/ld-linux-x86-64.so.2", 4 * 1024 * 1024)).hexdigest(),
            "libc_sha256": hashlib.sha256(regular(ROOT / "lib/libc.so.6", 4 * 1024 * 1024)).hexdigest(),
        },
        "schema_version": 2,
        "runtime_inventory_digest": hashlib.sha256(
            json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
        "runtime_entry_count": len(entries),
        "runtime_total_bytes": sum(item["bytes"] for item in entries),
        "required_executables": list(REQUIRED_EXECUTABLES),
        "required_abi_members": list(REQUIRED_ABI_MEMBERS),
        "elf_closure_digest": hashlib.sha256(
            json.dumps(elf_contracts, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
        "elf_count": len(elf_contracts),
        "network_contacted": False,
    }
    return {"schema": "ryeos.product_qualification_result.v1",
            "subject_manifest_hash": subject["manifest_hash"],
            "claims": list(QUALIFICATION_CLAIMS),
            "probe_evidence": evidence}


if __name__ == "__main__":
    request = json.load(__import__("sys").stdin)
    print(json.dumps(execute(request), sort_keys=True, separators=(",", ":")))
