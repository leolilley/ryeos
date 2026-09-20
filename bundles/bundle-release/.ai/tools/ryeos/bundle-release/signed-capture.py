# ryeos:signed:2026-09-20T01:14:46Z:c9f092979f6e8efcb527e1712b67cba0d14adaff48a6c7d1ac015b28cd3238a8:UoN909aneefrmwUx+3yRG+1FTAEOkwW5imrHbLpmk04X7rUeRQUmUcjSDSdfl2Gxs7sT/AfqWIH+CyNSmt8rBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/python3
"""Apply the exact publisher manifest to one admitted unsigned bundle product."""
import hashlib, json, os, pathlib, re, shutil, stat, sys

HASH = re.compile(r"[0-9a-f]{64}")
request = json.load(sys.stdin)
required = {"release_input", "materialization_result_hash", "signed_tree_manifest_hash",
            "manifest_item_hash", "signed_manifest"}
if set(request) != required:
    raise SystemExit("closed signed-capture request required")
if any(not isinstance(request[name], str) or not HASH.fullmatch(request[name])
       for name in ("materialization_result_hash", "signed_tree_manifest_hash", "manifest_item_hash")):
    raise SystemExit("invalid signed-capture coordinate")
signed = request["signed_manifest"]
if not isinstance(signed, str) or len(signed.encode()) > 131072:
    raise SystemExit("signed manifest exceeds its finite bound")
if hashlib.sha256(signed.encode()).hexdigest() != request["manifest_item_hash"]:
    raise SystemExit("signed manifest bytes changed")
sealed = json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "null"))
if not isinstance(sealed, list) or len(sealed) != 1 or sealed[0].get("id") != "unsigned_bundle":
    raise SystemExit("exact admitted unsigned bundle required")
source = pathlib.Path("/ryeos/realizations/unsigned-native-bundle")
target = pathlib.Path.cwd() / "products/signed-native-bundle/tree"
if not source.is_dir() or source.is_symlink() or target.exists():
    raise SystemExit("unsafe signed-capture source or target")
for path in source.rglob("*"):
    info = path.lstat()
    if path.is_symlink() or not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
        raise SystemExit("unsigned bundle contains an unsafe entry")
target.parent.mkdir(parents=True, exist_ok=False)
shutil.copytree(source, target, symlinks=False)
manifest = target / ".ai/manifest.yaml"
if not manifest.is_file() or manifest.is_symlink():
    raise SystemExit("unsigned bundle manifest is absent or unsafe")
manifest.write_text(signed)
json.dump({"schema":"ryeos.signed_bundle_capture.v1",
           "materialization_result_hash":request["materialization_result_hash"],
           "signed_tree_manifest_hash":request["signed_tree_manifest_hash"],
           "manifest_item_hash":request["manifest_item_hash"]},
          sys.stdout, sort_keys=True, separators=(",", ":")); print()
