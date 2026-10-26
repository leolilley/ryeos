#!/usr/bin/env python3
"""Rebuild the pinned `build-support-inputs` tree from its upstream sources.

The signed inventory names every member with its sha256, size and mode. Each
member has exactly one source:

  * `image/*`  copied out of the pinned Rust toolchain image (`docker cp`),
  * `bin/tee`  one member of the retained authoring-tools archive,
  * `elf/*`    members of the authoring source-inputs archive.

Everything the script needs (image digest, archive URLs and digests, the
expected tree digest) is read from repository YAML; only file paths live here.
The default mode is a dry run that reads YAML and prints the plan. `--execute`
needs docker and network and refuses on any mismatch; archive content is
untrusted, so only inventory-listed regular members are extracted, by name,
never through tarfile extraction.

The tree manifest hash is computed in pure Python with the same canonical
content-manifest rule the repository's tree author uses, so the rebuilt tree is
checked against the pinned digest directly.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path, PurePosixPath

import yaml

ROOT = Path(__file__).resolve().parents[2]
INVENTORY = ROOT / ".ai/config/development/ryeos/authoring-build-support-inputs.yaml"
ACQUISITION = ROOT / ".ai/config/development/ryeos/authoring-source-acquisition.yaml"
SOURCE_ACTIVATION = (ROOT / "bundles/environment-qualification/.ai/config/development/ryeos/"
                     "authoring-source-inputs-activation.yaml")
PIN_HOLDER = (ROOT / ".ai/tools/ryeos/development/authoring-environment-production/"
              "assemble-build-support.py")
PIN_ID = "build-support-inputs"

INVENTORY_SCHEMA = "ryeos.development.authoring-build-support-inputs.v1"
IMAGE_RE = re.compile(r"^[a-z0-9./_-]+@sha256:[0-9a-f]{64}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
CHUNK = 1024 * 1024
DOWNLOAD_TIMEOUT = 60


class Refusal(Exception):
    pass


# ---------------------------------------------------------------- repository data

def load_yaml(path: Path) -> dict:
    """Load a signed YAML/JSON document, skipping the leading signature line."""
    text = path.read_text(encoding="utf-8")
    lines = text.split("\n")
    if lines and lines[0].startswith("# ryeos:signed:"):
        text = "\n".join(lines[1:])
    value = yaml.safe_load(text)
    if not isinstance(value, dict):
        raise Refusal(f"{path}: not a mapping")
    return value


def read_pinned_digest(holder: Path, external_id: str) -> str:
    """Read the pinned tree digest of one `external_content` entry from a tool header."""
    lines = holder.read_text(encoding="utf-8").split("\n")
    for index, line in enumerate(lines):
        if line.strip() == f"#     - id: {external_id}":
            for follow in lines[index + 1:index + 8]:
                match = re.match(r"^#\s+digest:\s*([0-9a-f]{64})\s*$", follow)
                if match:
                    return match.group(1)
                if follow.startswith("#     - id:"):
                    break
    raise Refusal(f"no pinned digest for {external_id} in {holder}")


def check_identity(name: str, identity: object) -> None:
    if (not isinstance(identity, dict) or set(identity) != {"bytes", "mode", "sha256"} or
            type(identity["bytes"]) is not int or identity["bytes"] < 0 or
            identity["mode"] not in (0o644, 0o755) or
            not isinstance(identity["sha256"], str) or not HEX64.match(identity["sha256"])):
        raise Refusal(f"malformed inventory identity: {name}")


def safe_relative(name: str) -> PurePosixPath:
    path = PurePosixPath(name)
    if (not name or path.is_absolute() or "\\" in name or "\0" in name or
            any(part in ("", ".", "..") for part in name.split("/"))):
        raise Refusal(f"unsafe member path: {name!r}")
    return path


def load_pins(inventory_path=INVENTORY, acquisition_path=ACQUISITION,
              activation_path=SOURCE_ACTIVATION, pin_holder=PIN_HOLDER) -> dict:
    inventory = load_yaml(inventory_path)
    if inventory.get("schema") != INVENTORY_SCHEMA:
        raise Refusal("unsupported build-support input inventory schema")
    inputs = inventory.get("inputs")
    provenance = inventory.get("provenance")
    if not isinstance(inputs, dict) or not inputs or not isinstance(provenance, dict):
        raise Refusal("inventory has no inputs or provenance")
    for name, identity in inputs.items():
        safe_relative(name)
        check_identity(name, identity)
    image = provenance.get("image")
    if not isinstance(image, str) or not IMAGE_RE.match(image):
        raise Refusal("inventory image is not a digest-pinned reference")
    image_paths = provenance.get("input_sources")
    if not isinstance(image_paths, dict):
        raise Refusal("inventory records no image paths")
    helper = provenance.get("retained_static_helper")
    if (not isinstance(helper, dict) or helper.get("member") not in inputs or
            not HEX64.match(str(helper.get("archive_sha256")))):
        raise Refusal("inventory records no usable retained helper")

    acquisition = load_yaml(acquisition_path)
    declared = {a["name"]: a for a in acquisition.get("bootstrap_archives", [])}
    tools = declared.get(helper["archive"])
    if tools is None:
        raise Refusal("retained helper archive is not declared by the source acquisition")
    activation = load_yaml(activation_path)
    sources = activation.get("sources")
    if not isinstance(sources, list) or len(sources) != 1:
        raise Refusal("source-inputs activation must declare exactly one source")
    release = sources[0]
    prefix = None
    for component in activation.get("components", []):
        shape = component.get("shape", {})
        if shape.get("source") == release["id"]:
            prefix = shape.get("prefix")
    if not prefix:
        raise Refusal("source-inputs activation records no archive prefix")
    return {
        "inputs": inputs,
        "image": image,
        "image_paths": image_paths,
        "helper_member": helper["member"],
        "tools_archive": {
            "name": helper["archive"],
            "url": tools["url"],
            "sha256": helper["archive_sha256"],
            "acquisition_sha256": tools["sha256"],
            "max_bytes": tools["bytes"],
        },
        "source_archive": {
            "name": release["url"].rsplit("/", 1)[-1],
            "url": release["url"],
            "sha256": release["sha256"],
            "prefix": prefix,
            "max_bytes": release["maximum_compressed_bytes"],
        },
        "expected_manifest_hash": read_pinned_digest(pin_holder, PIN_ID),
    }


# ---------------------------------------------------------------- plan

def build_plan(pins: dict) -> dict:
    """Assign each inventory member to one source; unknown origins stay unknown."""
    members = {}
    for name in sorted(pins["inputs"]):
        identity = pins["inputs"][name]
        entry = {"bytes": identity["bytes"], "mode": identity["mode"], "sha256": identity["sha256"]}
        top = name.split("/", 1)[0]
        if top == "image" and name in pins["image_paths"]:
            entry.update(source="image", container_path=pins["image_paths"][name],
                         basis="inventory provenance input_sources")
        elif name == pins["helper_member"]:
            entry.update(source="tools-archive", archive_member="located by sha256 among regular `tee` members",
                         basis="inventory provenance retained_static_helper")
        elif top == "elf":
            entry.update(source="source-archive", archive_member=f"{pins['source_archive']['prefix']}/{name}",
                         basis="inferred: the source-inputs raw selection uses the same elf/ names; "
                               "verified by sha256 before acceptance")
        else:
            entry.update(source="unknown", basis="no recorded origin")
        members[name] = entry
    conflict = pins["tools_archive"]["sha256"] != pins["tools_archive"]["acquisition_sha256"]
    return {
        "sources": {
            "image": {"reference": pins["image"]},
            "tools-archive": {k: pins["tools_archive"][k] for k in ("name", "url", "sha256")}
                             | {"acquisition_declared_sha256": pins["tools_archive"]["acquisition_sha256"],
                                "pin_conflict": conflict},
            "source-archive": {k: pins["source_archive"][k] for k in ("name", "url", "sha256", "prefix")},
        },
        "members": members,
        "member_count": len(members),
        "total_bytes": sum(m["bytes"] for m in members.values()),
        "unknown_members": sorted(n for n, m in members.items() if m["source"] == "unknown"),
        "expected_manifest_hash": pins["expected_manifest_hash"],
        "inventory_manifest": inventory_manifest(pins["inputs"]),
    }


# ---------------------------------------------------------------- manifest

def canonical_json(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii")


def manifest_of(files: dict) -> dict:
    """Content manifest of {path: (mode, size, sha256)}, with implied directories."""
    names = set()
    for path in files:
        parts = path.split("/")
        for depth in range(1, len(parts)):
            names.add("/".join(parts[:depth]))
    entries = [{"path": d, "kind": "dir"} for d in names]
    total = 0
    for path, (mode, size, digest) in files.items():
        total += size
        entries.append({"path": path, "kind": "file", "mode": mode, "size": size, "blob_hash": digest})
    entries.sort(key=lambda e: e["path"].encode())
    value = {"schema": "ryeos.external_content.tree.v2", "kind": "external_content_manifest",
             "entries": entries, "entry_count": len(entries), "total_bytes": total}
    return {"hash": hashlib.sha256(canonical_json(value)).hexdigest(),
            "entry_count": len(entries), "total_bytes": total}


def inventory_manifest(inputs: dict) -> dict:
    return manifest_of({n: (i["mode"], i["bytes"], i["sha256"]) for n, i in inputs.items()})


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(CHUNK):
            digest.update(chunk)
    return digest.hexdigest()


def walk_tree(root: Path) -> dict:
    """Observed {path: (portable mode, size, sha256)}; refuses links and special files."""
    if root.is_symlink() or not root.is_dir():
        raise Refusal("tree root is not an ordinary directory")
    observed = {}
    for path in sorted(root.rglob("*")):
        info = path.lstat()
        name = path.relative_to(root).as_posix()
        if stat.S_ISDIR(info.st_mode):
            continue
        if not stat.S_ISREG(info.st_mode):
            raise Refusal(f"link or special file in tree: {name}")
        mode = 0o755 if info.st_mode & 0o111 else 0o644
        observed[name] = (mode, info.st_size, hash_file(path))
    return observed


def verify_tree(root: Path, inputs: dict) -> dict:
    """Check every member's sha256, size and mode, and that nothing extra exists."""
    observed = walk_tree(root)
    if set(observed) != set(inputs):
        missing = sorted(set(inputs) - set(observed))
        extra = sorted(set(observed) - set(inputs))
        raise Refusal(f"tree membership differs from inventory: missing={missing[:5]} extra={extra[:5]}")
    for name, identity in inputs.items():
        mode, size, digest = observed[name]
        if size != identity["bytes"]:
            raise Refusal(f"size mismatch: {name}")
        if digest != identity["sha256"]:
            raise Refusal(f"sha256 mismatch: {name}")
        if mode != identity["mode"]:
            raise Refusal(f"mode mismatch: {name}")
    return {"members_verified": len(observed), "manifest": manifest_of(observed)}


# ---------------------------------------------------------------- untrusted archives

def download(url: str, destination: Path, expected_sha256: str, max_bytes: int) -> None:
    if not url.startswith("https://"):
        raise Refusal(f"refusing non-https download: {url}")
    digest, total = hashlib.sha256(), 0
    request = urllib.request.Request(url, headers={"User-Agent": "rebuild-build-support-inputs"})
    with urllib.request.urlopen(request, timeout=DOWNLOAD_TIMEOUT) as response, \
            destination.open("xb") as output:
        while chunk := response.read(CHUNK):
            total += len(chunk)
            if total > max_bytes:
                raise Refusal(f"download exceeds its declared bound: {url}")
            digest.update(chunk)
            output.write(chunk)
    if digest.hexdigest() != expected_sha256:
        raise Refusal(f"archive sha256 mismatch for {url}: got {digest.hexdigest()}")


def _copy_member(archive: tarfile.TarFile, member: tarfile.TarInfo, destination: Path | None,
                 limit: int) -> str:
    """Stream one regular member, bounded; returns its sha256."""
    digest, total = hashlib.sha256(), 0
    source = archive.extractfile(member)
    output = destination.open("xb") if destination is not None else None
    try:
        while chunk := source.read(CHUNK):
            total += len(chunk)
            if total > limit:
                raise Refusal(f"archive member exceeds its inventory size: {member.name}")
            digest.update(chunk)
            if output:
                output.write(chunk)
    finally:
        if output:
            output.close()
    return digest.hexdigest()


def extract_listed(archive_path: Path, wanted: dict, out_dir: Path) -> None:
    """Extract only {archive member name: (output relative name, identity)}.

    Every archive entry name is checked for traversal; listed members must be
    ordinary regular files; nothing is written outside `out_dir`.
    """
    seen = set()
    with tarfile.open(archive_path, "r:gz") as archive:
        for member in archive:
            safe_relative(member.name.rstrip("/"))
            if member.name not in wanted:
                continue
            if not member.isreg():
                raise Refusal(f"listed archive member is not a regular file: {member.name}")
            if member.name in seen:
                raise Refusal(f"duplicate archive member: {member.name}")
            seen.add(member.name)
            target_name, identity = wanted[member.name]
            if member.size != identity["bytes"]:
                raise Refusal(f"archive member size differs from inventory: {member.name}")
            target = out_dir.joinpath(*safe_relative(target_name).parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            if _copy_member(archive, member, target, identity["bytes"]) != identity["sha256"]:
                raise Refusal(f"archive member sha256 differs from inventory: {member.name}")
    absent = sorted(set(wanted) - seen)
    if absent:
        raise Refusal(f"archive lacks listed members: {absent[:5]}")


def extract_by_hash(archive_path: Path, basename: str, target: Path, identity: dict) -> str:
    """Extract the one regular member named `basename` whose bytes match `identity`."""
    found = None
    with tarfile.open(archive_path, "r:gz") as archive:
        for member in archive:
            safe_relative(member.name.rstrip("/"))
            if not member.isreg() or PurePosixPath(member.name).name != basename:
                continue
            if member.size != identity["bytes"]:
                continue
            if _copy_member(archive, member, None, identity["bytes"]) == identity["sha256"]:
                if found is not None:
                    raise Refusal(f"more than one archive member matches {basename}")
                found = member.name
                target.parent.mkdir(parents=True, exist_ok=True)
                with tarfile.open(archive_path, "r:gz") as second:
                    _copy_member(second, second.getmember(found), target, identity["bytes"])
    if found is None:
        raise Refusal(f"no archive member named {basename} matches the inventory")
    return found


# ---------------------------------------------------------------- execution

def docker(*args: str) -> str:
    result = subprocess.run(["docker", *args], check=False, capture_output=True, text=True)
    if result.returncode != 0:
        raise Refusal(f"docker {args[0]} failed: {result.stderr.strip()}")
    return result.stdout.strip()


def copy_from_image(plan: dict, output: Path) -> None:
    cid = docker("create", plan["sources"]["image"]["reference"])
    try:
        for name, member in plan["members"].items():
            if member["source"] != "image":
                continue
            target = output.joinpath(*safe_relative(name).parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            docker("cp", "-L", f"{cid}:{member['container_path']}", str(target))
    finally:
        subprocess.run(["docker", "rm", "-f", cid], check=False, capture_output=True)


def apply_modes(output: Path, inputs: dict) -> None:
    for name, identity in inputs.items():
        path = output.joinpath(*safe_relative(name).parts)
        if path.is_symlink() or not path.is_file():
            raise Refusal(f"not an ordinary file after assembly: {name}")
        path.chmod(identity["mode"])


def execute(pins: dict, plan: dict, output: Path, scratch_root: Path, keep_scratch: bool) -> dict:
    if plan["unknown_members"]:
        raise Refusal(f"members with unknown origin: {plan['unknown_members']}")
    if plan["sources"]["tools-archive"]["pin_conflict"]:
        raise Refusal("the retained helper archive digest in the inventory differs from the "
                      "source acquisition's declared digest; resolve the pin before rebuilding")
    if output.exists() or output.is_symlink():
        raise Refusal("output directory already exists")
    scratch_root.mkdir(parents=True, exist_ok=True)
    tools_dir = Path(tempfile.mkdtemp(prefix="tools-archive-", dir=scratch_root))
    source_dir = Path(tempfile.mkdtemp(prefix="source-archive-", dir=scratch_root))
    output.mkdir(parents=True)
    try:
        copy_from_image(plan, output)
        tools, source = pins["tools_archive"], pins["source_archive"]
        tools_path = tools_dir / tools["name"]
        download(tools["url"], tools_path, tools["sha256"], tools["max_bytes"])
        helper = pins["helper_member"]
        extract_by_hash(tools_path, PurePosixPath(helper).name, output.joinpath(*helper.split("/")),
                        pins["inputs"][helper])
        source_path = source_dir / source["name"]
        download(source["url"], source_path, source["sha256"], source["max_bytes"])
        wanted = {f"{source['prefix']}/{n}": (n, pins["inputs"][n])
                  for n, m in plan["members"].items() if m["source"] == "source-archive"}
        extract_listed(source_path, wanted, output)
        apply_modes(output, pins["inputs"])
        result = verify_tree(output, pins["inputs"])
    except BaseException:
        shutil.rmtree(output, ignore_errors=True)
        raise
    finally:
        if not keep_scratch:
            shutil.rmtree(tools_dir, ignore_errors=True)
            shutil.rmtree(source_dir, ignore_errors=True)
    expected = pins["expected_manifest_hash"]
    return {"members_verified": result["members_verified"],
            "expected_manifest_hash": expected,
            "computed_manifest_hash": result["manifest"]["hash"],
            "manifest_matches_pin": result["manifest"]["hash"] == expected,
            "entry_count": result["manifest"]["entry_count"],
            "total_bytes": result["manifest"]["total_bytes"],
            "output": str(output)}


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--dry-run", action="store_true", help="print the plan (default)")
    mode.add_argument("--execute", action="store_true", help="rebuild the tree (docker + network)")
    parser.add_argument("--output", type=Path, help="new directory for the rebuilt tree")
    parser.add_argument("--scratch", type=Path, default=Path(tempfile.gettempdir()),
                        help="parent for the two new archive download directories")
    parser.add_argument("--keep-scratch", action="store_true")
    args = parser.parse_args(argv)
    try:
        pins = load_pins()
        plan = build_plan(pins)
        if not args.execute:
            print(json.dumps(plan, indent=2, sort_keys=True))
            return 0
        if args.output is None:
            raise Refusal("--execute requires --output")
        report = execute(pins, plan, args.output.resolve(), args.scratch.resolve(), args.keep_scratch)
        print(json.dumps(report, indent=2, sort_keys=True))
        return 0 if report["manifest_matches_pin"] else 2
    except Refusal as error:
        print(f"refused: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
