# ryeos:signed:2026-09-19T04:10:25Z:1ecb8abd2712aceded04a2d8feda7f09f9962ab5a905c0b1baa42335b478e6e7:bRvkwVwMvNA+nfUI/8SFkK16Gw/dhgkwhpt6gqNq0PhCCmZJs3CwS16m3EjXbEQei2eAlfLwzbKOqUx9JRBzAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/python3
"""Qualify the exact captured tree against its admitted release input."""
import hashlib, json, os, pathlib, re, stat, sys
HASH=re.compile(r"[0-9a-f]{64}")
p=json.load(sys.stdin)
if set(p)!={"release_input","release_input_digest","captured_tree_manifest_hash","manifest_item_hash"}: raise SystemExit("closed qualification request required")
if any(not isinstance(p[k],str) or not HASH.fullmatch(p[k]) for k in ("release_input_digest","captured_tree_manifest_hash","manifest_item_hash")): raise SystemExit("invalid qualification coordinate")
release=p["release_input"]
if not isinstance(release,dict): raise SystemExit("admitted release input required")
digest=hashlib.sha256(json.dumps(release,sort_keys=True,separators=(",",":")).encode()).hexdigest()
if digest!=p["release_input_digest"]: raise SystemExit("release input digest changed")
name=release.get("bundle_name"); payloads=release.get("payloads")
if name=="core" or not isinstance(name,str): raise SystemExit("invalid non-core bundle identity")
if not isinstance(payloads,list) or release.get("requires_binary_build")!=bool(payloads): raise SystemExit("payload/build-kind binding changed")
sealed=json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS","null"))
if not isinstance(sealed,list) or len(sealed)!=1 or sealed[0].get("id")!="subject": raise SystemExit("exact admitted subject required")
if sealed[0].get("manifest_hash")!=p["captured_tree_manifest_hash"]: raise SystemExit("captured tree differs from admitted qualification subject")
root=pathlib.Path("/ryeos/realizations/native-bundle")
manifest=root/".ai/manifest.yaml"
if not manifest.is_file() or manifest.is_symlink(): raise SystemExit("bundle manifest is absent or unsafe")
manifest_bytes=manifest.read_bytes()
if hashlib.sha256(manifest_bytes).hexdigest()!=p["manifest_item_hash"]: raise SystemExit("bundle manifest content differs from admission")
# Canonical release builds emit a JSON body (valid YAML), then the constrained
# publisher prepends the RyeOS signature comment. Do not parse YAML by regex.
manifest_body="\n".join(line for line in manifest_bytes.decode("utf-8").splitlines() if not line.startswith("# ryeos:signed:"))
manifest_value=json.loads(manifest_body)
manifest_name=manifest_value.get("name") if isinstance(manifest_value,dict) else None
if manifest_name!=name: raise SystemExit("bundle manifest name differs from admission")
if manifest_value!=release.get("authored_manifest"): raise SystemExit("bundle manifest differs from source-generated admission")
seen=set()
for path in root.rglob("*"):
    info=path.lstat()
    if path.is_symlink(): raise SystemExit("bundle contains symbolic link")
    if path.is_dir(): continue
    if not stat.S_ISREG(info.st_mode): raise SystemExit("bundle contains special filesystem object")
    inode=(info.st_dev,info.st_ino)
    if inode in seen or info.st_nlink!=1: raise SystemExit("bundle contains hard link")
    seen.add(inode)
checks=["captured-product-binding","bundle-name-binding","bundle-manifest-present"]
if payloads:
    target=release.get("target",{})
    triple=target.get("triple") if target.get("kind")=="triple" else None
    if not triple: raise SystemExit("native bundle has no exact target triple")
    expected=sorted(payload.get("binary") for payload in payloads)
    if any(not isinstance(binary,str) for binary in expected): raise SystemExit("invalid owned binary")
    binary_root=root/".ai/bin"/triple
    actual=sorted(path.name for path in binary_root.iterdir()) if binary_root.is_dir() else []
    if actual!=expected: raise SystemExit("captured native payload set differs from admission")
    for binary in expected:
        info=(binary_root/binary).lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o111==0: raise SystemExit("owned payload is absent or non-executable")
    checks.extend(["exact-owned-payloads","native-payloads-executable","exact-target-triple"])
    build_kind="native"
else:
    if release.get("target")!={"kind":"portable"}: raise SystemExit("data-only bundle is not portable")
    if (root/".ai/bin").exists(): raise SystemExit("data-only product contains generated binaries")
    checks.append("portable-data-only-plan"); build_kind="data_only"
checks.append("no-symbolic-links")
thread_id=os.environ.get("RYE_THREAD_ID")
if not thread_id: raise SystemExit("qualification has no admitted thread identity")
json.dump({"schema":"ryeos.product_qualification_result.v1","subject_manifest_hash":sealed[0]["manifest_hash"],
           "claims":["native_bundle_release_checks_v1"],
           "probe_evidence":{"schema":"ryeos.native_bundle_qualification.v1",**p,
             "bundle_name":name,"build_kind":build_kind,"checks":checks,"verifier_thread_id":thread_id}},
          sys.stdout,sort_keys=True,separators=(",",":"));print()
