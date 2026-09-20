# ryeos:signed:2026-09-20T12:43:58Z:8becf54c5bce396ab88a9ce56e02370f3fcf1996423942dc7b48d6f903856fd9:j++109BmUxcIapIZeg2BBtNyfL8K13buzhbegYzhwgR2CRz7ZRoXhlFjpQ3ItMAWLpIGqQsqbbOdObJXeP7eDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/env python3
"""Apply the exact publisher manifest to one admitted unsigned Core seed."""
import hashlib
import json
import os
import pathlib
import re
import shutil
import stat
import sys

HASH = re.compile(r"[0-9a-f]{64}")
request = json.load(sys.stdin)
required = {"release_input", "materialization_result_hash",
            "signed_tree_manifest_hash", "manifest_item_hash", "signed_manifest"}
if not isinstance(request, dict) or set(request) != required:
    raise SystemExit("closed Core seed capture request required")
release_input = request["release_input"]
if (not isinstance(release_input, dict) or release_input.get("bundle_name") != "core"
        or release_input.get("predecessor_generation_hash") is not None
        or not isinstance(release_input.get("authored_manifest"), dict)
        or release_input["authored_manifest"].get("name") != "core"):
    raise SystemExit("capture input is not an initial Core seed")
if any(not isinstance(request[name], str) or not HASH.fullmatch(request[name])
       for name in ("materialization_result_hash", "signed_tree_manifest_hash",
                    "manifest_item_hash")):
    raise SystemExit("invalid Core seed capture coordinate")
signed = request["signed_manifest"]
if not isinstance(signed, str) or len(signed.encode()) > 131072:
    raise SystemExit("signed Core manifest exceeds its finite bound")
if hashlib.sha256(signed.encode()).hexdigest() != request["manifest_item_hash"]:
    raise SystemExit("signed Core manifest bytes changed")
sealed = json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "null"))
if (not isinstance(sealed, list) or len(sealed) != 1
        or sealed[0].get("id") != "unsigned_core"):
    raise SystemExit("exact admitted unsigned Core seed required")
source = pathlib.Path("/ryeos/realizations/unsigned-core-seed")
target = pathlib.Path.cwd() / "products/signed-core-seed/tree"
if not source.is_dir() or source.is_symlink() or target.exists():
    raise SystemExit("unsafe Core seed capture source or target")
for path in source.rglob("*"):
    info = path.lstat()
    if path.is_symlink() or not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
        raise SystemExit("unsigned Core seed contains an unsafe entry")
target.parent.mkdir(parents=True, exist_ok=False)
shutil.copytree(source, target, symlinks=False)
manifest = target / ".ai/manifest.yaml"
if not manifest.is_file() or manifest.is_symlink():
    raise SystemExit("unsigned Core manifest is absent or unsafe")
manifest.write_text(signed)
json.dump({
    "schema": "ryeos.signed_core_seed_capture.v1",
    "materialization_result_hash": request["materialization_result_hash"],
    "signed_tree_manifest_hash": request["signed_tree_manifest_hash"],
    "manifest_item_hash": request["manifest_item_hash"],
}, sys.stdout, sort_keys=True, separators=(",", ":"))
print()
