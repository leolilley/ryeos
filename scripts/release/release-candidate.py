#!/usr/bin/env python3
"""Build a release-candidate manifest and recompute its closure.

`build` emits one canonical-JSON `ryeos.release_candidate.v1` record naming a
populated bundle tree: signed items (content hash with the signature line
stripped, plus signer), the native set, the dependency closure, the
predecessor and the requested authority class. `recompute` rederives every
value from a tree and reports per-entry status in a
`ryeos.release_closure_report.v1`. A signature over an older candidate
authorizes nothing about a new tree; nothing is inherited from the manifest by
reference, it is only compared. An entry that cannot be recomputed is
`unavailable`, which is never success.

The `signer` recorded per item is the fingerprint claimed in that item's
signature envelope; no signature is verified here. Files outside `.ai/bin`
that carry no envelope are listed under `unsigned` with their content hash. A
file whose first line mentions the signature marker but does not parse as an
envelope is refused.

The payload-ownership contract is read from the working tree at
`--repository-root`, not from the source commit. `--source-commit` is
recorded and compared but not verified against that tree.

The manifest carries no signatures, timestamps or host paths. The authority
class is plain data; whether a class is acceptable is decided elsewhere.
The tool is read-only.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sys

sys.dont_write_bytecode = True

CANDIDATE_SCHEMA = "ryeos.release_candidate.v1"
REPORT_SCHEMA = "ryeos.release_closure_report.v1"
HASH = re.compile(r"[0-9a-f]{64}")
COMMIT = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})")
ENVELOPE = re.compile(
    r"(?:# |<!-- )ryeos:signed:.+:([0-9a-f]{64}):[A-Za-z0-9+/=]+:([0-9a-f]{64})(?: -->)?"
)


def _load(filename, name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


_audit = _load("audit-bundle-generation.py", "audit_bundle_generation")
_plan = _load("inspect-native-bundle-input.py", "inspect_native_bundle_input")
_native = _load("inspect-core-native-set.py", "inspect_core_native_set")


def fail(message):
    raise ValueError(message)


def canonical(value):
    return _native.canonical(value).decode()


def sha256_text(value):
    return hashlib.sha256(canonical(value).encode()).hexdigest()


def digest_of(record):
    return _native.digest_of(record)


def tree_items(bundle_root):
    """Split non-binary files into signed items and unsigned files.

    Signed items map to their stripped-content hash and claimed signer;
    unsigned files map to their full content hash.
    """
    items = {}
    unsigned = {}
    bin_root = bundle_root / ".ai" / "bin"
    for current, dirnames, filenames in os.walk(bundle_root, followlinks=False):
        dirnames.sort()
        base = Path(current)
        if base == bin_root:
            dirnames[:] = []
            continue
        for name in sorted(filenames):
            path = base / name
            relative = path.relative_to(bundle_root).as_posix()
            with open(path, "rb") as handle:
                first = handle.readline(4096)
                body = handle.read()
            if b"ryeos:signed:" not in first:
                unsigned[relative] = _native.sha256_file(path)
                continue
            try:
                match = ENVELOPE.fullmatch(first.decode("ascii").rstrip("\r\n"))
            except UnicodeDecodeError:
                match = None
            if not match:
                fail(f"malformed signature envelope: {relative}")
            claimed, signer = match.groups()
            content = hashlib.sha256(body).hexdigest()
            if content != claimed:
                fail(f"item body does not match its signature envelope: {relative}")
            items[relative] = {"hash": content, "signer": signer}
    return items, unsigned


def compute(repository_root, tree, bundle, triple, source_commit, interpreter,
            bundle_refs, ownership_config=None):
    """Derive the tree-dependent parts of a manifest.

    Returns (parts, unavailable_refs). A bundle reference whose digest is None
    cannot be derived and is returned in `unavailable_refs` instead.
    """
    if not COMMIT.fullmatch(source_commit):
        fail("source commit must be a lowercase 40 or 64 digit hex identity")
    bundle_root = tree / bundle
    if not bundle_root.is_dir() or bundle_root.is_symlink():
        fail(f"{bundle}: missing or unsafe bundle root")
    plan = _plan.plan(repository_root, bundle, None, triple,
                      ownership_config=ownership_config)
    ownership = plan["payload_ownership_content_hash"]
    audit = _audit.audit_bundle(bundle_root)
    violations = [f"{bundle}/{item}" for item in audit["violations"]]
    if triple != "portable":
        violations.extend(_audit.populated_violations(tree, triple, [bundle], plan["payloads"]))
    if violations:
        fail("populated tree refused: " + "; ".join(sorted(violations)))

    has_bin = os.path.lexists(bundle_root / ".ai" / "bin")
    native = _native.build_record(bundle_root, (), interpreter) if has_bin else None
    if native is None and interpreter is not None:
        fail("an interpreter was named for a bundle with no native set")

    closure = {}
    unavailable = []
    for name, digest in sorted(bundle_refs.items()):
        if digest is None:
            unavailable.append(name)
        else:
            closure[f"bundle:{name}"] = {"digest": digest, "computed_from": ownership}
    for payload in plan["payloads"]:
        binary = payload["binary"]
        closure[f"native:{binary}"] = {
            "digest": _native.sha256_file(bundle_root / ".ai" / "bin" / triple / binary),
            "computed_from": ownership,
        }
    if native is not None and native["interpreter"] is not None:
        closure["interpreter"] = {
            "digest": native["interpreter"]["sha256"],
            "computed_from": native["digest"],
        }
    items, unsigned = tree_items(bundle_root)
    parts = {
        "generation_inputs": {
            "audit_digest": sha256_text(audit),
            "ownership_contract_hash": ownership,
            "source_commit": source_commit,
            "target": triple,
        },
        "items": items,
        "unsigned": unsigned,
        "native": native,
        "closure": closure,
    }
    return parts, unavailable


def build(repository_root, tree, bundle, triple, version, source_commit, authority_class,
          publisher_fingerprint, interpreter=None, bundle_refs=None, predecessor=None,
          ownership_config=None):
    if not isinstance(version, str) or not version:
        fail("version is empty")
    if not isinstance(authority_class, str) or not authority_class:
        fail("authority class must be a non-empty string")
    if not HASH.fullmatch(publisher_fingerprint):
        fail("publisher fingerprint must be a lowercase SHA-256 identity")
    if predecessor is not None and not HASH.fullmatch(predecessor):
        fail("predecessor must be a lowercase SHA-256 identity")
    refs = dict(bundle_refs or {})
    for name, digest in refs.items():
        if not HASH.fullmatch(digest or ""):
            fail(f"bundle reference {name} needs a lowercase SHA-256 digest")
    parts, _ = compute(repository_root, tree, bundle, triple, source_commit, interpreter,
                       refs, ownership_config)
    record = {
        "schema": CANDIDATE_SCHEMA,
        "bundle": bundle,
        "version": version,
        **parts,
        "predecessor": predecessor,
        "authority": {"class": authority_class, "publisher_fingerprint": publisher_fingerprint},
    }
    record["digest"] = digest_of(record)
    return record


def _shape(condition, what):
    if not condition:
        fail(f"malformed manifest: {what}")


def load_manifest(path):
    record = json.loads(Path(path).read_text())
    _shape(isinstance(record, dict), "not an object")
    _shape(record.get("schema") == CANDIDATE_SCHEMA, "unknown schema")
    for key in ("digest", "bundle", "version"):
        _shape(isinstance(record.get(key), str), f"{key} must be a string")
    for key in ("generation_inputs", "items", "unsigned", "closure", "authority"):
        _shape(isinstance(record.get(key), dict), f"{key} must be an object")
    _shape(isinstance(record["generation_inputs"].get("target"), str),
           "generation_inputs.target must be a string")
    _shape(isinstance(record["authority"].get("class"), str), "authority.class must be a string")
    _shape(record.get("native") is None or isinstance(record["native"], dict),
           "native must be an object or null")
    _shape(record.get("predecessor") is None or isinstance(record["predecessor"], str),
           "predecessor must be a string or null")
    _shape(all(isinstance(v, dict) and set(v) == {"hash", "signer"}
               for v in record["items"].values()), "items entries need hash and signer")
    _shape(all(isinstance(v, dict) for v in record["closure"].values()),
           "closure entries must be objects")
    if record["digest"] != digest_of(record):
        fail("manifest digest does not match its content")
    return record


def flatten(value, prefix=""):
    if isinstance(value, dict) and value:
        out = {}
        for key in sorted(value):
            out.update(flatten(value[key], f"{prefix}/{key}" if prefix else key))
        return out
    return {prefix: value}


def view(parts):
    flat = {}
    for section in ("generation_inputs", "items", "unsigned", "native", "closure"):
        flat.update(flatten(parts[section], section))
    return flat


def recompute(repository_root, manifest, tree, source_commit, interpreter=None,
              bundle_digests=None, predecessor_manifest=None, ownership_config=None):
    bundle = manifest["bundle"]
    triple = manifest["generation_inputs"]["target"]
    supplied = dict(bundle_digests or {})
    declared_refs = [k[len("bundle:"):] for k in manifest["closure"] if k.startswith("bundle:")]
    refs = {name: supplied.get(name) for name in declared_refs}
    refs.update({name: d for name, d in supplied.items() if name not in refs})
    parts, missing_refs = compute(repository_root, tree, bundle, triple, source_commit,
                                  interpreter, refs, ownership_config)
    expected = view(manifest)
    actual = view(parts)

    unavailable_prefixes = [f"closure/bundle:{name}" for name in missing_refs]
    if interpreter is None and manifest["native"] and manifest["native"].get("interpreter"):
        unavailable_prefixes += ["closure/interpreter", "native/interpreter", "native/digest"]

    def is_unavailable(name):
        return any(name == p or name.startswith(p + "/") for p in unavailable_prefixes)

    entries = {}
    for name in sorted(set(expected) | set(actual) | {n for n in expected if is_unavailable(n)}):
        if is_unavailable(name):
            entries[name] = {"status": "unavailable"}
        elif name not in actual:
            entries[name] = {"status": "removed"}
        elif name not in expected:
            entries[name] = {"status": "added"}
        elif expected[name] == actual[name]:
            entries[name] = {"status": "match"}
        else:
            entries[name] = {"status": "changed", "expected": expected[name],
                             "actual": actual[name]}

    if missing_refs or unavailable_prefixes:
        entries["digest"] = {"status": "unavailable"}
    else:
        fresh = {
            "schema": CANDIDATE_SCHEMA, "bundle": bundle, "version": manifest["version"],
            **parts, "predecessor": manifest["predecessor"], "authority": manifest["authority"],
        }
        same = digest_of(fresh) == manifest["digest"]
        entries["digest"] = {"status": "match" if same else "changed"}

    predecessor_diff = None
    if predecessor_manifest is None:
        status = "unavailable" if manifest["predecessor"] is not None else "match"
    elif manifest["predecessor"] == predecessor_manifest["digest"]:
        status = "match"
        old_items = predecessor_manifest["items"]
        new_items = parts["items"]
        old_native = (predecessor_manifest["native"] or {}).get("digest")
        new_native = (parts["native"] or {}).get("digest")
        predecessor_diff = {
            "added_items": sorted(set(new_items) - set(old_items)),
            "changed_items": sorted(
                p for p in set(new_items) & set(old_items) if new_items[p]["hash"] != old_items[p]["hash"]
            ),
            "native_digest_equal": old_native == new_native,
            "removed_items": sorted(set(old_items) - set(new_items)),
            "signer_changes": [
                {"from": old_items[p]["signer"], "path": p, "to": new_items[p]["signer"]}
                for p in sorted(set(new_items) & set(old_items))
                if new_items[p]["signer"] != old_items[p]["signer"]
            ],
        }
    else:
        status = "changed"
    entries["predecessor"] = {"status": status}

    closure_digest = sha256_text({n: v for n, v in sorted(actual.items()) if not is_unavailable(n)})
    refused = sorted(n for n, e in entries.items() if e["status"] != "match")
    return {
        "schema": REPORT_SCHEMA,
        "bundle": bundle,
        "closure_digest": closure_digest,
        "entries": entries,
        "manifest_digest": manifest["digest"],
        "ok": not refused,
        "predecessor_diff": predecessor_diff,
        "refused": refused,
    }


def pairs(values, label):
    out = {}
    for item in values:
        name, sep, digest = item.partition("=")
        if not sep or not name or not HASH.fullmatch(digest):
            fail(f"{label} must be NAME=SHA256: {item}")
        if name in out:
            fail(f"duplicate {label}: {name}")
        out[name] = digest
    return out


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--repository-root", type=Path, required=True)
    common.add_argument("--tree", type=Path, required=True,
                        help="directory holding one populated tree per bundle name")
    common.add_argument("--source-commit", required=True)
    common.add_argument("--interpreter", type=Path)
    common.add_argument("--ownership-config", type=Path)
    b = sub.add_parser("build", parents=[common])
    b.add_argument("--bundle", required=True)
    b.add_argument("--triple", required=True)
    b.add_argument("--version", required=True)
    b.add_argument("--authority-class", required=True)
    b.add_argument("--publisher-fingerprint", required=True)
    b.add_argument("--predecessor", help="digest of the previous candidate")
    b.add_argument("--requires-bundle", action="append", default=[], metavar="NAME=SHA256")
    r = sub.add_parser("recompute", parents=[common])
    r.add_argument("--manifest", type=Path, required=True)
    r.add_argument("--predecessor-manifest", type=Path)
    r.add_argument("--bundle-digest", action="append", default=[], metavar="NAME=SHA256",
                   help="digest of a required bundle as found now")
    args = parser.parse_args(argv)
    root = args.repository_root.resolve(strict=True)
    tree = Path(os.path.abspath(args.tree))
    if args.command == "build":
        result = build(
            root, tree, args.bundle, args.triple, args.version, args.source_commit,
            args.authority_class, args.publisher_fingerprint, args.interpreter,
            pairs(args.requires_bundle, "required bundle"), args.predecessor,
            args.ownership_config,
        )
        code = 0
    else:
        manifest = load_manifest(args.manifest)
        previous = load_manifest(args.predecessor_manifest) if args.predecessor_manifest else None
        result = recompute(
            root, manifest, tree, args.source_commit, args.interpreter,
            pairs(args.bundle_digest, "bundle digest"), previous, args.ownership_config,
        )
        code = 0 if result["ok"] else 1
    sys.stdout.write(canonical(result) + "\n")
    return code


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        print(f"release candidate failed: {error}", file=sys.stderr)
        raise SystemExit(2)
