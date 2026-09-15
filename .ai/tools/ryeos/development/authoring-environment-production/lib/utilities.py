# ryeos:signed:2026-09-08T16:57:42Z:281872d7aebc678f39a5f960508067fc1674c57f142e1733c32abd9eca9f8cd8:G9ebcMLYAThEGbrxAHes1Tc56NLNTVYBXGaT7i1S2btaWB+EJmCVpnpzLAy5uH2V4eBsqm38A3iFOC/SDELBBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Finite utility production for an admitted, private RyeOS Tool execution.

This library owns the recipe used by build-utilities and its separate E2E probe.
Stage0 owns the compiler; the separate admitted
support tree owns shell, make and build helpers. Nothing discovers host tools,
acquires packages, signs definitions, imports results, or publishes bindings.

The enclosing Tool must bound execution time, process groups and scratch storage.
This module additionally bounds archive extraction, diagnostics and final output.
"""

from __future__ import annotations

import hashlib
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess

from archives import open_archive

from production import (ELF_TOOLS, MAX_FILE_BYTES, MAX_TOTAL_BYTES, REQUIRED_COMMANDS, RUNTIME_ROOT,
                        ElfTools, canonical_json, input_inventory, ordinary_member,
                        portable_regular_mode, receipt, relative, sha256)


SCHEMA = "ryeos.development.authoring-utility-sources.v1"
PLATFORM = Path("/ryeos/realizations/platform")
SUPPORT = Path("/ryeos/realizations/authoring-build-support")
BUILD_ORDER = ("coreutils", "sed", "diffutils", "findutils", "gawk", "patch",
               "grep", "zlib", "git")
MAX_SOURCE_BYTES = 128 * 1024 * 1024
MAX_EXTRACTED_BYTES = 1024 * 1024 * 1024
MAX_EXTRACTED_ENTRIES = 100_000
MAX_LOG_BYTES = 8 * 1024 * 1024
MAX_FAILURE_CONFIG_LOG_BYTES = 64 * 1024
RECIPE_FILES = ("utilities.py", "utility_production.py", "production.py", "archives.py")
# This is a minimum, not a claim that upstream's whole subprocess closure has
# already been qualified. Every additional helper must be in the exact inventory.
REQUIRED_SUPPORT_COMMANDS = frozenset("""
sh make strip false awk basename cat chmod cmp cp cut dirname expr find grep
head install ln ls mkdir mv printf pwd rm rmdir sed sort tail test touch tr
uname uniq wc xargs tee
""".split())


def validate_sources(config: dict) -> dict:
    keys = {"category", "name", "version", "schema", "input_image",
            "source_date_epoch", "publisher_notices", "sources"}
    if (not isinstance(config, dict) or set(config) != keys or
            config["schema"] != SCHEMA or config["category"] != "development/ryeos" or
            config["name"] != "authoring-utility-sources" or config["version"] != "1.0.0"):
        raise ValueError("unsupported utility source contract")
    if type(config["source_date_epoch"]) is not int or not 0 <= config["source_date_epoch"] <= 4_102_444_800:
        raise ValueError("invalid source normalization epoch")
    if (not isinstance(config["input_image"], str) or
            not re.fullmatch(r"[A-Za-z0-9./_-]+@sha256:[0-9a-f]{64}", config["input_image"])):
        raise ValueError("input image provenance must be an exact immutable coordinate")
    if not isinstance(config["sources"], list) or len(config["sources"]) != len(BUILD_ORDER) + 1:
        raise ValueError("utility source inventory is not the finite supported set")
    selected, outputs, archives = {}, set(), set()
    for source in config["sources"]:
        if not isinstance(source, dict):
            raise ValueError("incomplete utility source selection")
        common = {"name", "version", "directory", "archive", "url", "sha256", "bytes", "licenses"}
        required = common if source.get("name") == "zig" else common | {"programs"}
        if source.get("name") == "git":
            required |= {"runtime_shell"}
        if not required <= set(source) or set(source) - required - {"configure"}:
            raise ValueError("incomplete utility source selection")
        name = source["name"]
        if name == "git" and source["runtime_shell"] != "bin/zsh":
            raise ValueError("Git runtime shell must select the declared authoring shell")
        if name not in (*BUILD_ORDER, "zig") or name in selected:
            raise ValueError("duplicate or unsupported utility source")
        for field in ("directory", "archive"):
            if len(relative(source[field]).parts) != 1:
                raise ValueError("source archive and directory must be single names")
        if source["archive"] in archives:
            raise ValueError("duplicate source archive")
        archives.add(source["archive"])
        if (type(source["bytes"]) is not int or not 0 < source["bytes"] <= MAX_SOURCE_BYTES or
                not isinstance(source["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", source["sha256"])):
            raise ValueError("invalid exact source archive identity")
        if not isinstance(source["licenses"], list) or not 1 <= len(source["licenses"]) <= 16:
            raise ValueError("missing finite source license selection")
        for notice in source["licenses"]:
            relative(notice)
        flags = source.get("configure", [])
        if (not isinstance(flags, list) or len(flags) > 32 or any(
                not isinstance(flag, str) or not re.fullmatch(r"--[A-Za-z0-9_=-]+", flag)
                for flag in flags)):
            raise ValueError("invalid finite configure options")
        programs = source.get("programs", {})
        if not isinstance(programs, dict) or len(programs) > 64:
            raise ValueError("invalid finite program selection")
        for command, built in programs.items():
            if len(relative(command).parts) != 1 or command in outputs:
                raise ValueError("duplicate or noncanonical utility command")
            relative(built)
            outputs.add(command)
        selected[name] = source
    if set(selected) != set(BUILD_ORDER) | {"zig"}:
        raise ValueError("missing utility source")
    if outputs != REQUIRED_COMMANDS - {"rg", "zsh"}:
        raise ValueError("utility outputs are not the exact supported commands")
    if not isinstance(config["publisher_notices"], list) or not 1 <= len(config["publisher_notices"]) <= 16:
        raise ValueError("missing publisher notice identities")
    for notice in config["publisher_notices"]:
        if not isinstance(notice, dict) or set(notice) != {"path", "target", "sha256"}:
            raise ValueError("invalid publisher notice identity")
        if (relative(notice["target"]).parts[0] != "licenses" or
                not isinstance(notice["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", notice["sha256"])):
            raise ValueError("invalid publisher notice target")
    return selected


def checked_support(root: Path, config: dict) -> dict[str, Path]:
    if not isinstance(config, dict) or set(config) != {"inputs", "commands", "notices"}:
        raise ValueError("exact build-support inventory is required")
    expected = config["inputs"]
    if not isinstance(expected, dict) or not expected:
        raise ValueError("exact build-support inventory is required")
    observed = input_inventory(root)
    missing = sorted(set(expected) - set(observed))
    if missing:
        raise ValueError(f"build-support inventory is missing exact member {missing[0]!r}")
    unexpected = sorted(set(observed) - set(expected))
    if unexpected:
        raise ValueError(f"build-support inventory has unexpected member {unexpected[0]!r}")
    for member in sorted(expected):
        identity = expected[member]
        if (not isinstance(identity, dict) or
                set(identity) != {"bytes", "mode", "sha256"}):
            raise ValueError(f"build-support expected identity is invalid for member {member!r}")
        differences = [
            f"{field} expected {identity[field]!r}, observed {observed[member][field]!r}"
            for field in ("bytes", "mode", "sha256")
            if identity[field] != observed[member][field]
        ]
        if differences:
            raise ValueError(
                f"build-support member {member!r} differs: " + "; ".join(differences))
    if any(member not in config["inputs"] or config["inputs"][member]["mode"] != 0o755
           for member in ELF_TOOLS.values()):
        raise ValueError("build support lacks its exact ELF inspection tools")
    commands = config["commands"]
    if not isinstance(commands, dict) or not REQUIRED_SUPPORT_COMMANDS <= commands.keys():
        raise ValueError("build-support command inventory is incomplete")
    for command, member in commands.items():
        if len(relative(command).parts) != 1 or member != f"bin/{command}":
            raise ValueError("build-support commands must select their exact bin member")
        path = ordinary_member(root, member)
        if portable_regular_mode(path.lstat().st_mode) != 0o755:
            raise ValueError("build-support command is not executable")
        with path.open("rb") as stream:
            if stream.read(4) != b"\x7fELF":
                raise ValueError("build-support command must be ELF, never an ambient shebang")
    if {name for name in config["inputs"] if name.startswith("bin/")} != set(commands.values()):
        raise ValueError("unselected build-support PATH member")
    if not isinstance(config["notices"], dict):
        raise ValueError("invalid build-support notice selection")
    for target, member in config["notices"].items():
        if relative(target).parts[0] != "licenses" or member not in config["inputs"]:
            raise ValueError("build-support notice must select an admitted license")
    return {name: root / member for name, member in commands.items()}


def extract_source(archive: Path, destination: Path, directory: str) -> Path:
    """Extract authenticated upstream sources into a fresh private directory."""
    destination.mkdir(mode=0o700, exist_ok=False)
    entries, expanded = 0, 0
    with open_archive(archive, maximum_expanded_bytes=MAX_EXTRACTED_BYTES) as source:
        for member in source:
            entries += 1
            expanded += member.size
            parts = PurePosixPath(member.name).parts
            if (entries > MAX_EXTRACTED_ENTRIES or expanded > MAX_EXTRACTED_BYTES or
                    member.size < 0 or member.size > MAX_SOURCE_BYTES or len(parts) > 32 or
                    not parts or parts[0] != directory or ".." in parts or
                    not (member.isfile() or member.isdir() or member.issym() or member.islnk())):
                raise ValueError("source archive exceeds its extraction contract")
            # Python's data filter rejects out-of-root links and strips unsafe
            # ownership/modes. Internal upstream links are not silently discarded.
            source.extract(member, destination, filter="data")
    return ordinary_member(destination, directory, directory=True)


def _append_failure_config_log(cwd: Path, log: Path, used: int) -> int:
    """Append bounded config.log head/tail evidence without following a link."""
    header = b"\n--- config.log bounded evidence ---\n"
    omitted = b"\n--- config.log omitted middle ---\n"
    available = MAX_LOG_BYTES - used
    if available <= len(header):
        return used
    descriptor = None
    try:
        candidate = ordinary_member(cwd, "config.log")
        descriptor = os.open(candidate, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        facts = os.fstat(descriptor)
        if not stat.S_ISREG(facts.st_mode):
            return used
        payload_room = min(MAX_FAILURE_CONFIG_LOG_BYTES, available - len(header))
        if facts.st_size <= payload_room:
            detail = os.read(descriptor, facts.st_size)
        elif payload_room > len(omitted):
            retained_room = payload_room - len(omitted)
            head_bytes = retained_room // 2
            tail_bytes = retained_room - head_bytes
            head = os.read(descriptor, head_bytes)
            os.lseek(descriptor, facts.st_size - tail_bytes, os.SEEK_SET)
            tail = os.read(descriptor, tail_bytes)
            detail = head + omitted + tail
        else:
            detail = b""
        with log.open("ab") as stream:
            stream.write(header)
            stream.write(detail)
        return used + len(header) + len(detail)
    except (OSError, ValueError):
        # The upstream failure remains authoritative when no safe diagnostic is
        # available. Never replace it with a diagnostic-retention error.
        return used
    finally:
        if descriptor is not None:
            os.close(descriptor)


def run(argv: list[str], cwd: Path, env: dict[str, str], log: Path) -> None:
    """Bound diagnostics without creating a second process-group/time owner."""
    if not Path(argv[0]).is_absolute():
        raise ValueError("build operation requires an exact executable path")
    used = log.stat().st_size if log.exists() else 0
    header = ("\n$ " + " ".join(argv) + "\n").encode()
    if used + len(header) > MAX_LOG_BYTES:
        raise ValueError("utility build diagnostic exceeds its bound")
    with log.open("ab") as stream:
        stream.write(header)
        used += len(header)
        with subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as process:
            try:
                while block := process.stdout.read1(65536):
                    if used + len(block) > MAX_LOG_BYTES:
                        raise ValueError("utility build diagnostic exceeds its bound")
                    stream.write(block)
                    used += len(block)
                if process.wait():
                    stream.flush()
                    used = _append_failure_config_log(cwd, log, used)
                    raise ValueError(f"utility build command failed; inspect {log.name}")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()


def build_environment(work: Path, commands: dict[str, Path], platform: Path) -> dict[str, str]:
    zig = ordinary_member(platform, "zig/zig")
    # Stage0 already owns this native linker alias and its runtime closure.
    # Autoconf queries LD independently of CC; do not discover a host linker,
    # construct a second compiler wrapper, or change the admitted PATH.
    linker = ordinary_member(platform, "native/bin/ld.lld")
    for executable in (zig, linker):
        if portable_regular_mode(executable.lstat().st_mode) != 0o755:
            raise ValueError("admitted Stage0 compiler/linker is not executable")
    shell = str(commands["sh"])
    return {
        "PATH": str(commands["sh"].parent), "HOME": str(work / "home"),
        "LC_ALL": "C", "LANG": "C", "TZ": "UTC", "ZERO_AR_DATE": "1",
        "CONFIG_SHELL": shell, "SHELL": shell, "MAKE": str(commands["make"]),
        "CC": f"{zig} cc -target x86_64-linux-musl -static",
        "LD": str(linker),
        "AR": f"{zig} ar", "RANLIB": f"{zig} ranlib",
        "CFLAGS": f"-Os -g0 -ffile-prefix-map={work}=/usr/src/authoring -fno-ident",
        "LDFLAGS": "-static -Wl,--build-id=none", "PKG_CONFIG": str(commands["false"]),
        "ZIG_GLOBAL_CACHE_DIR": str(work / "zig-cache"),
        "ZIG_LOCAL_CACHE_DIR": str(work / "zig-local-cache"),
        "TMPDIR": str(work / "tmp"), "FORCE_UNSAFE_CONFIGURE": "1",
    }


def build_commands(name: str, source: dict, source_dir: Path, zlib: Path,
                   env: dict[str, str]) -> list[list[str]]:
    shell, make = env["CONFIG_SHELL"], env["MAKE"]
    prefix = [make, "-j2", f"SHELL={shell}"]
    if name == "git":
        # Upstream uses SHELL_PATH for build generators AND compiled runtime
        # behavior. Its separate C-quoted setting keeps the build helper path
        # out of the delivered executable. Select the final authoring shell
        # from Config; do not bake a scratch/build-support path into Git.
        return [[*prefix, f"CC={env['CC']}", f"AR={env['AR']}",
                 f"SHELL_PATH={shell}",
                 f'SHELL_PATH_CQ="{RUNTIME_ROOT}/{source["runtime_shell"]}"',
                 *[f"NO_{feature}=YesPlease" for feature in (
                     "CURL", "EXPAT", "OPENSSL", "GETTEXT", "TCLTK", "PERL",
                     "PYTHON", "ICONV", "REGEX", "RUST")],
                 "prefix=/nonexistent/authoring-git", "RUNTIME_PREFIX=YesPlease",
                 f"CFLAGS={env['CFLAGS']} -I{zlib}", f"LDFLAGS={env['LDFLAGS']} -L{zlib}", "git"]]
    flags = ["--static"] if name == "zlib" else [
        "--prefix=/nonexistent/authoring", "--host=x86_64-linux-musl", *source.get("configure", [])]
    # Never execute a configure shebang. Upstream nested script/recipe behavior
    # still needs empty-namespace qualification with this exact support artifact.
    return [[shell, str(source_dir / "configure"), *flags],
            [*prefix, "libz.a" if name == "zlib" else "all"]]


def require_static(path: Path, tools: ElfTools) -> None:
    facts = tools.facts(path)
    if facts["interpreter"] or facts["needed"] or facts["runpath"] or facts["rpath"]:
        raise ValueError("utility output requires a loader or library search")


def build_evidence(source_config: dict, support_config: dict, *,
                   platform: Path = PLATFORM, support: Path = SUPPORT) -> dict:
    """Canonical evidence emitted by the sole utility recipe owner."""
    return {
        "source_contract_sha256": hashlib.sha256(canonical_json(source_config)).hexdigest(),
        "support_contract_sha256": hashlib.sha256(canonical_json(support_config)).hexdigest(),
        "platform_root": str(platform),
        "support_root": str(support),
        "recipe_source_sha256": sha256(Path(__file__)),
        "effects": "live",
    }


def build_utilities(source_config: dict, support_config: dict, source_archives: Path,
                    work: Path, destination: Path, *, platform: Path = PLATFORM,
                    support: Path = SUPPORT) -> dict:
    """Produce a retained tree; caller supplies admitted roots and private paths.

    The Tool entry and E2E probe call this same recipe. Support and final-runtime
    qualification do not authorize publication or make arbitrary support inputs
    interchangeable with the exact selected realization.
    """
    from archives import read_members

    selected = validate_sources(source_config)
    if platform != PLATFORM or support != SUPPORT:
        raise ValueError("utility production requires the exact admitted runtime roots")
    commands = checked_support(support, support_config)
    env = build_environment(work, commands, platform)
    archives = {}
    for name, source in selected.items():
        path = ordinary_member(source_archives, source["archive"])
        if path.stat().st_size != source["bytes"] or sha256(path) != source["sha256"]:
            raise ValueError(f"source archive identity mismatch: {name}")
        archives[name] = path
    for notice in source_config["publisher_notices"]:
        member = support_config["notices"].get(notice["target"])
        if member is None or sha256(ordinary_member(support, member)) != notice["sha256"]:
            raise ValueError("required publisher notice is absent from build support")
    # No host image path is read; input_image and publisher notice paths are
    # upstream provenance only. All filesystem inputs above are admitted roots.
    for path in (work, destination):
        if not path.is_absolute() or path.exists() or path.is_symlink():
            raise ValueError("utility production requires fresh absolute private output paths")
        if path.parent.resolve(strict=True) != path.parent:
            raise ValueError("utility output parent must not contain symlinks")
        if any(root == path or root in path.parents for root in (platform, support, source_archives)):
            raise ValueError("utility output must not be inside an admitted input")
    if work in destination.parents or destination in work.parents or work == destination:
        raise ValueError("utility work and retained output must be disjoint")
    work.mkdir(mode=0o700)
    destination.mkdir(mode=0o700)
    for member in ("home", "tmp", "sources", "logs"):
        (work / member).mkdir(mode=0o700)
    for member in ("bin", "licenses", "corresponding-sources"):
        (destination / member).mkdir(mode=0o755)
    env["SOURCE_DATE_EPOCH"] = str(source_config["source_date_epoch"])
    source_dirs = {}
    for name in BUILD_ORDER:
        source_dirs[name] = extract_source(archives[name], work / "sources" / name,
                                           selected[name]["directory"])
    tools = ElfTools(support)
    delivered = 0

    def deliver(output: Path, *, source: Path | None = None, data: bytes | None = None,
                mode: int = 0o644) -> None:
        nonlocal delivered
        size = source.stat().st_size if source is not None else len(data)
        if size > MAX_FILE_BYTES or delivered + size > MAX_TOTAL_BYTES:
            raise ValueError("utility output exceeds its retained byte bound")
        output.parent.mkdir(parents=True, exist_ok=True)
        with output.open("xb") as writer:
            if source is None:
                writer.write(data)
            else:
                with source.open("rb") as reader:
                    remaining = size
                    while block := reader.read(min(1024 * 1024, remaining + 1)):
                        if len(block) > remaining:
                            raise ValueError("utility output changed while being copied")
                        writer.write(block)
                        remaining -= len(block)
                    if remaining:
                        raise ValueError("utility output changed while being copied")
        output.chmod(mode)
        delivered += size

    for name in BUILD_ORDER:
        source, source_dir = selected[name], source_dirs[name]
        log = work / "logs" / f"{name}.log"
        for command in build_commands(name, source, source_dir, source_dirs["zlib"], env):
            run(command, source_dir, env, log)
        for command, member in source["programs"].items():
            binary = ordinary_member(source_dir, member)
            require_static(binary, tools)
            output = destination / "bin" / command
            deliver(output, source=binary, mode=0o755)
            run([str(commands["strip"]), "--strip-all", str(output)], work, env, log)
            if output.stat().st_size > binary.stat().st_size:
                raise ValueError("stripped utility unexpectedly grew")
            output.chmod(0o755)
            require_static(output, tools)
        for notice in source["licenses"]:
            output = destination / "licenses" / name / relative(notice)
            deliver(output, source=ordinary_member(source_dir, notice))
    zig = selected["zig"]
    members = {f"{zig['directory']}/{notice}" for notice in zig["licenses"]}
    for member, data in read_members(archives["zig"], members).items():
        output = destination / "licenses" / "zig" / Path(member).relative_to(zig["directory"])
        deliver(output, data=data)
    for notice in source_config["publisher_notices"]:
        output = destination / relative(notice["target"])
        deliver(output, source=ordinary_member(support, support_config["notices"][notice["target"]]))
    for name, archive in archives.items():
        output = destination / "corresponding-sources" / selected[name]["archive"]
        deliver(output, source=archive)
    # Corresponding-source/provenance copies are not executable authority. The
    # admitted source closure and retained capsule remain RyeOS-owned facts.
    for name in RECIPE_FILES:
        deliver(destination / "corresponding-sources" / "ryeos-production" / name,
                source=ordinary_member(Path(__file__).parent, name))
    deliver(destination / "source-contract.json", data=canonical_json(source_config))
    deliver(destination / "build-evidence.json", data=canonical_json(
        build_evidence(source_config, support_config, platform=platform, support=support)))
    for path in sorted(destination.rglob("*")):
        path.chmod(0o755 if path.is_dir() or path.parent == destination / "bin" else 0o644)
        os.utime(path, (source_config["source_date_epoch"], source_config["source_date_epoch"]))
    return {"operation": "build_utilities", **receipt(destination), "binding_published": False}
