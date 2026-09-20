# ryeos:signed:2026-09-20T11:15:48Z:3f738c26fc7198096880ca906cd1d2233a7f4f89f0568e622d46e492572d641c:8DLPUe0WXqLjNWwZD6aeD8MXqFjV/Xo/DeI+gSB15IuoS0uKSKcL8lJnECFj8Km22UZx1NBa3egrm3U9RNacAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/python3
"""Qualify one exact admitted signed substrate Core seed."""
import json
import os
import pathlib
import re
import stat
import sys

HASH = re.compile(r"[0-9a-f]{64}")
request = json.load(sys.stdin)
if request != {}:
    raise SystemExit("Core seed qualification accepts no caller-authored parameters")
sealed = json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "null"))
if (not isinstance(sealed, list) or len(sealed) != 1
        or sealed[0].get("id") != "subject"):
    raise SystemExit("exact admitted signed Core seed required")
manifest_hash = sealed[0].get("manifest_hash")
if not isinstance(manifest_hash, str) or not HASH.fullmatch(manifest_hash):
    raise SystemExit("admitted Core seed has no exact manifest identity")
root = pathlib.Path("/ryeos/realizations/core-seed")
manifest = root / ".ai/manifest.yaml"
if not root.is_dir() or root.is_symlink() or not manifest.is_file() or manifest.is_symlink():
    raise SystemExit("Core seed manifest is absent or unsafe")
try:
    lines = manifest.read_text().splitlines()
except UnicodeDecodeError:
    raise SystemExit("Core seed manifest is not UTF-8")
signature_lines = [line for line in lines if line.startswith("# ryeos:signed:")]
if len(signature_lines) != 1:
    raise SystemExit("Core seed manifest has no unique RyeOS signature")
body = "\n".join(line for line in lines if not line.startswith("# ryeos:signed:"))
try:
    manifest_value = json.loads(body)
except json.JSONDecodeError:
    raise SystemExit("Core seed manifest body is not canonical JSON")
if (not isinstance(manifest_value, dict) or manifest_value.get("name") != "core"):
    raise SystemExit("signed seed does not identify Core")
if body != json.dumps(manifest_value, sort_keys=True, separators=(",", ":")):
    raise SystemExit("Core seed manifest body is not canonical JSON")

seen = set()
binary_count = 0
for path in root.rglob("*"):
    info = path.lstat()
    if path.is_symlink():
        raise SystemExit("Core seed contains a symbolic link")
    if stat.S_ISDIR(info.st_mode):
        continue
    if not stat.S_ISREG(info.st_mode):
        raise SystemExit("Core seed contains a special filesystem object")
    inode = (info.st_dev, info.st_ino)
    if inode in seen or info.st_nlink != 1:
        raise SystemExit("Core seed contains a hard link")
    seen.add(inode)
    if path.is_relative_to(root / ".ai/bin"):
        binary_count += 1
        if info.st_mode & 0o111 == 0:
            raise SystemExit("Core seed native payload is non-executable")
if binary_count == 0:
    raise SystemExit("Core seed contains no native payload")
thread_id = os.environ.get("RYE_THREAD_ID")
if not thread_id:
    raise SystemExit("Core seed qualification has no admitted thread identity")
json.dump({
    "schema": "ryeos.product_qualification_result.v1",
    "subject_manifest_hash": manifest_hash,
    "claims": ["substrate_core_seed_checks_v1"],
    "probe_evidence": {
        "schema": "ryeos.core_seed_qualification.v1",
        "bundle_name": "core",
        "binary_count": binary_count,
        "checks": [
            "captured-product-binding", "publisher-signed-manifest",
            "canonical-core-manifest", "core-bundle-identity",
            "no-symbolic-links", "no-hard-links", "native-payloads-executable",
        ],
        "verifier_thread_id": thread_id,
    },
}, sys.stdout, sort_keys=True, separators=(",", ":"))
print()
