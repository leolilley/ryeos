# ryeos:signed:2026-09-20T12:43:58Z:7913fc4f2dff90552d152797b8fe139070ed2b814aaaf7cdc36db2e474ad6668:a0PkN9a8r7XRJ2HYOpYe1gWz0Ip76N0fPhs8VCPRtr3W4oQaVbRfi8sbFXl6i5a0HnBkmNZ9uLIwrVBvpx3QBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/env python3
"""Qualify one canonical receipt-only captured substrate release product."""
import json
import os
import pathlib
import re
import stat
import sys

HASH = re.compile(r"[0-9a-f]{64}")
TRIPLE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}")
ROOT = pathlib.Path("/ryeos/realizations/substrate-release")
RECEIPT = ROOT / ".ai/substrate-release.json"


def refuse(message):
    raise SystemExit(message)


def validate_target(value):
    if not isinstance(value, dict):
        refuse("substrate target must be an object")
    if value == {"kind": "portable"}:
        return
    if set(value) != {"kind", "triple"} or value.get("kind") != "triple":
        refuse("substrate target is not a closed portable or triple target")
    if not isinstance(value["triple"], str) or not TRIPLE.fullmatch(value["triple"]):
        refuse("substrate target triple is invalid")


def validate_receipt(value):
    required = {
        "schema", "kind", "substrate_image_digest", "substrate_protocol",
        "target", "core_generation_hash",
    }
    if not isinstance(value, dict) or set(value) != required:
        refuse("substrate receipt has an open or incomplete schema")
    if value["schema"] != "ryeos.substrate_build_receipt.v1":
        refuse("substrate receipt schema is not current")
    if value["kind"] != "substrate_build_receipt":
        refuse("substrate receipt kind is invalid")
    digest = value["substrate_image_digest"]
    if not isinstance(digest, str) or not digest.startswith("sha256:") or not HASH.fullmatch(digest[7:]):
        refuse("substrate image digest must be exact sha256")
    protocol = value["substrate_protocol"]
    if isinstance(protocol, bool) or not isinstance(protocol, int) or protocol <= 0 or protocol > 0xFFFFFFFF:
        refuse("substrate protocol must be a nonzero u32")
    validate_target(value["target"])
    core = value["core_generation_hash"]
    if not isinstance(core, str) or not HASH.fullmatch(core):
        refuse("core generation hash must be lowercase 64-hex")


request = json.load(sys.stdin)
if request != {}:
    refuse("substrate qualification accepts no caller-authored parameters")
sealed = json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "null"))
if not isinstance(sealed, list) or len(sealed) != 1 or sealed[0].get("id") != "subject":
    refuse("exact admitted substrate subject required")
manifest_hash = sealed[0].get("manifest_hash")
if not isinstance(manifest_hash, str) or not HASH.fullmatch(manifest_hash):
    refuse("admitted substrate subject has no exact manifest identity")
if not ROOT.is_dir() or ROOT.is_symlink():
    refuse("substrate realization root is absent or unsafe")

seen = set()
entries = []
for path in ROOT.rglob("*"):
    info = path.lstat()
    relative = path.relative_to(ROOT).as_posix()
    if path.is_symlink():
        refuse("substrate receipt product contains a symbolic link")
    if stat.S_ISDIR(info.st_mode):
        entries.append((relative, "dir"))
        continue
    if not stat.S_ISREG(info.st_mode):
        refuse("substrate receipt product contains a special filesystem object")
    inode = (info.st_dev, info.st_ino)
    if inode in seen or info.st_nlink != 1:
        refuse("substrate receipt product contains a hard link")
    seen.add(inode)
    if info.st_mode & 0o111:
        refuse("substrate receipt product contains executable content")
    if info.st_size <= 0 or info.st_size > 65536:
        refuse("substrate receipt exceeds its finite size bound")
    entries.append((relative, "file"))
if sorted(entries) != [(".ai", "dir"), (".ai/substrate-release.json", "file")]:
    refuse("substrate product is not the canonical receipt-only tree")

receipt_bytes = RECEIPT.read_bytes()
try:
    receipt_value = json.loads(receipt_bytes)
except (UnicodeDecodeError, json.JSONDecodeError):
    refuse("substrate receipt is not UTF-8 JSON")
validate_receipt(receipt_value)
canonical = json.dumps(receipt_value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
if receipt_bytes != canonical:
    refuse("substrate receipt bytes are not canonical JSON")
thread_id = os.environ.get("RYE_THREAD_ID")
if not thread_id:
    refuse("qualification has no admitted thread identity")

json.dump({
    "schema": "ryeos.product_qualification_result.v1",
    "subject_manifest_hash": manifest_hash,
    "claims": ["substrate_release_checks_v1"],
    "probe_evidence": {
        "schema": "ryeos.substrate_release_qualification.v1",
        "receipt": receipt_value,
        "checks": [
            "captured-product-binding", "receipt-only-tree", "canonical-receipt-json",
            "no-links-or-special-files", "no-executable-content",
        ],
        "verifier_thread_id": thread_id,
    },
}, sys.stdout, sort_keys=True, separators=(",", ":"))
print()
