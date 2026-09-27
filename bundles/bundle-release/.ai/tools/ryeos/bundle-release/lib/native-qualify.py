#!/usr/bin/env python3
# ryeos:signed:2026-09-23T13:41:33Z:6626e26b7e01ea37604b140cc4f1f63815c2de5823ddb0fec37092a5c9ded42b:W1MhkAf6sDyiTHfyYaKJ6lBsiGqebpt7TjCfPF+oZHOJK7e5WsaJZRxcKxj0eoM/mxddZ8zawyBxnXDOU/jwAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Qualify one exact admitted signed bundle-tree realization."""
import json, os, pathlib, stat, sys
p=json.load(sys.stdin)
if p!={}: raise SystemExit("qualification verifier accepts no caller-authored parameters")
sealed=json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS","null"))
if not isinstance(sealed,list) or len(sealed)!=2 or {entry.get("id") for entry in sealed}!={"python","subject"}: raise SystemExit("exact admitted Python and subject required")
subject=next(entry for entry in sealed if entry["id"]=="subject")
root=pathlib.Path("/ryeos/realizations/native-bundle")
manifest=root/".ai/manifest.yaml"
if not manifest.is_file() or manifest.is_symlink(): raise SystemExit("bundle manifest is absent or unsafe")
manifest_bytes=manifest.read_bytes()
# Canonical release builds emit a JSON body (valid YAML), then the constrained
# publisher prepends the RyeOS signature comment. Do not parse YAML by regex.
manifest_lines=manifest_bytes.decode("utf-8").splitlines()
if sum(line.startswith("# ryeos:signed:") for line in manifest_lines)!=1: raise SystemExit("bundle manifest has no unique RyeOS signature")
manifest_body="\n".join(line for line in manifest_lines if not line.startswith("# ryeos:signed:"))
manifest_value=json.loads(manifest_body)
name=manifest_value.get("name") if isinstance(manifest_value,dict) else None
if name=="core" or not isinstance(name,str) or not name: raise SystemExit("invalid non-core bundle identity")
seen=set()
for path in root.rglob("*"):
    info=path.lstat()
    if path.is_symlink(): raise SystemExit("bundle contains symbolic link")
    if path.is_dir(): continue
    if not stat.S_ISREG(info.st_mode): raise SystemExit("bundle contains special filesystem object")
    inode=(info.st_dev,info.st_ino)
    if inode in seen or info.st_nlink!=1: raise SystemExit("bundle contains hard link")
    seen.add(inode)
binary_root=root/".ai/bin"
binary_count=0
if binary_root.exists():
    for binary in binary_root.rglob("*"):
        if binary.is_file():
            binary_count+=1
            if binary.lstat().st_mode & 0o111==0: raise SystemExit("native payload is non-executable")
if binary_count==0: raise SystemExit("native bundle has no executable .ai/bin payload")
checks=["captured-product-binding","bundle-manifest-present","non-core-bundle","no-symbolic-links","no-hard-links"]
checks.append("native-payloads-executable")
thread_id=os.environ.get("RYE_THREAD_ID")
if not thread_id: raise SystemExit("qualification has no admitted thread identity")
json.dump({"schema":"ryeos.product_qualification_result.v1","subject_manifest_hash":subject["manifest_hash"],
           "claims":["native_bundle_release_checks_v1"],
           "probe_evidence":{"schema":"ryeos.native_bundle_qualification.v1",
             "bundle_name":name,"binary_count":binary_count,"checks":checks,"verifier_thread_id":thread_id}},
          sys.stdout,sort_keys=True,separators=(",",":"));print()
