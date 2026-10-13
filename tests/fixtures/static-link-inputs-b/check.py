#!/usr/bin/env python3
"""Check the isolated B source overlay and, optionally, its real export tree."""

import argparse
import base64
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import tomllib

import yaml


HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
OVERLAY = HERE / "overlay"
CONTRACT = Path(".ai/config/development/ryeos/static-link-inputs.yaml")
INTENT = Path("bundles/standard/.ai/config/development/ryeos/static-link-contract-intent.yaml")
POLICY = Path("bundles/standard/.ai/config/ryeos/environments/qualification/static-link-inputs.yaml")
TOOL = Path(".ai/tools/ryeos/development/static-link-inputs/assemble.py")
MEMBER = "usr/lib/x86_64-linux-gnu/libmcheck.a"
MEMBER_DIGEST = "c735a6c71d4e69b0a069ff3430c8c5e0ffccc3b201854d792644506feaaec5f1"
A_MANIFEST = "e16652fd10ff718182fe2b09fd8a4c913fc8c994baacf833e3e6a29a69750ebf"
B_MANIFEST = "2d949d2079ffa86459ccba2960b73332893aa9adebb0fb65b1f9e4d0576b5ff1"
ENVELOPE = re.compile(rb"# ryeos:signed:.+:([0-9a-f]{64}):([^:\n]+):([0-9a-f]{64})")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def signed_body(path, public_key=None, fingerprint=None):
    envelope, body = path.read_bytes().split(b"\n", 1)
    match = ENVELOPE.fullmatch(envelope)
    require(match is not None, f"missing signed envelope: {path}")
    digest, encoded_signature, signer = match.groups()
    require(hashlib.sha256(body).hexdigest().encode() == digest, f"stale signed body: {path}")
    if public_key is not None:
        require(signer.decode() == fingerprint, f"unexpected signer: {path}")
        with tempfile.TemporaryDirectory() as directory:
            temporary = Path(directory)
            # Ed25519 subjectPublicKeyInfo prefix followed by the trusted raw key.
            (temporary / "public.der").write_bytes(bytes.fromhex("302a300506032b6570032100") + public_key)
            (temporary / "hash").write_bytes(digest)
            (temporary / "signature").write_bytes(base64.b64decode(encoded_signature, validate=True))
            verification = subprocess.run(
                ["openssl", "pkeyutl", "-verify", "-pubin", "-keyform", "DER",
                 "-inkey", str(temporary / "public.der"), "-rawin", "-in", str(temporary / "hash"),
                 "-sigfile", str(temporary / "signature")],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
            )
            require(verification.returncode == 0, f"invalid publisher signature: {path}")
    return body


def tool_header(body):
    comments = []
    for line in body.decode().splitlines():
        if not line.startswith("#"):
            break
        if line.startswith("# "):
            comments.append(line[2:])
    return yaml.safe_load("\n".join(comments))["ryeos-tool"]


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def manifest(contract, root=None):
    names = set(contract["inputs"])
    directories = {str(parent) for name in names for parent in Path(name).parents if str(parent) != "."}
    entries = [{"path": name, "kind": "dir"} for name in directories]
    if root is not None:
        require(stat.S_ISDIR(root.lstat().st_mode), "realization root is not a directory")
        found_files, found_directories = set(), set()
        for path in root.rglob("*"):
            name = path.relative_to(root).as_posix()
            metadata = path.lstat()
            if stat.S_ISDIR(metadata.st_mode):
                require(name in directories and stat.S_IMODE(metadata.st_mode) == 0o755,
                        f"unexpected realization directory: {name}")
                found_directories.add(name)
            else:
                require(name in names and stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1,
                        f"unexpected realization member: {name}")
                expected = contract["inputs"][name]
                require(metadata.st_size == expected["bytes"] and
                        stat.S_IMODE(metadata.st_mode) == expected["mode"],
                        f"realization metadata differs: {name}")
                with path.open("rb") as stream:
                    digest = hashlib.file_digest(stream, "sha256").hexdigest()
                require(digest == expected["sha256"], f"realization bytes differ: {name}")
                found_files.add(name)
        require(found_files == names and found_directories == directories,
                "realization closure is incomplete")
    for name, expected in contract["inputs"].items():
        entries.append({"path": name, "kind": "file", "mode": expected["mode"],
                        "blob_hash": expected["sha256"], "size": expected["bytes"]})
    entries.sort(key=lambda entry: entry["path"].encode())
    value = {"schema": "ryeos.external_content.large.v2",
             "kind": "external_large_content_manifest", "entries": entries,
             "entry_count": len(entries),
             "total_bytes": sum(item["bytes"] for item in contract["inputs"].values())}
    canonical = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(canonical).hexdigest(), value


def check(realization):
    trust = tomllib.loads((REPO / ".dev-keys/PUBLISHER_DEV_TRUST.toml").read_text())
    public_key = base64.b64decode(trust["public_key"].removeprefix("ed25519:"), validate=True)
    require(len(public_key) == 32 and hashlib.sha256(public_key).hexdigest() == trust["fingerprint"],
            "invalid fixture publisher trust key")
    bodies = {path: signed_body(OVERLAY / path, public_key, trust["fingerprint"])
              for path in (CONTRACT, INTENT, POLICY, TOOL)}
    a_contract = json.loads(signed_body(REPO / CONTRACT))
    b_contract = json.loads(bodies[CONTRACT])
    expected = copy.deepcopy(a_contract)
    expected["version"] = "1.1.0"
    expected["inputs"][MEMBER] = {"source": "/" + MEMBER, "bytes": 1360, "mode": 0o644,
                                   "sha256": MEMBER_DIGEST}
    require(b_contract == expected, "B contract changes more than its version and real member")
    parser_path = REPO / ".ai/tools/ryeos/development/static-link-inputs/lib/inputs.py"
    spec = importlib.util.spec_from_file_location("static_input_contract", parser_path)
    parser = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(parser)
    require(parser.parse_contract(bodies[CONTRACT].decode()) == b_contract,
            "production contract parser rejected B")

    a_intent = yaml.safe_load(signed_body(REPO / INTENT))
    b_intent = yaml.safe_load(bodies[INTENT])
    expected_intent = {**a_intent, "version": "1.1.0",
                       "expected_contract_body_sha256": hashlib.sha256(bodies[CONTRACT]).hexdigest()}
    require(b_intent == expected_intent, "B intent does not select exactly the B contract")

    a_policy = yaml.safe_load(signed_body(REPO / POLICY))
    b_policy = yaml.safe_load(bodies[POLICY])
    expected_policy = copy.deepcopy(a_policy)
    expected_policy["version"] = "2.1.0"
    expected_policy["product_qualification_policy"]["verifier_parameters"]["files"][MEMBER] = {
        "bytes": 1360, "sha256": MEMBER_DIGEST}
    require(b_policy == expected_policy, "B verifier policy changes more than its member profile")
    profile = b_policy["product_qualification_policy"]["verifier_parameters"]
    require(set(profile["files"]) == set(b_contract["inputs"]), "B contract and verifier sets differ")

    a_tool = signed_body(REPO / TOOL)
    b_tool = bodies[TOOL]
    expected_tool = a_tool.replace(b'version: "2.0.0"', b'version: "2.1.0"', 1).replace(
        A_MANIFEST.encode(), B_MANIFEST.encode(), 1)
    require(b_tool == expected_tool, "B producer Tool changes more than version and input pin")
    b_declaration = tool_header(b_tool)
    input_pin = next(item["digest"] for item in b_declaration["external_content"]
                     if item["id"] == "static-link-inputs")
    require(input_pin == B_MANIFEST, "B producer Tool does not pin its B realization")

    a_digest, a_value = manifest(a_contract)
    b_digest, b_value = manifest(b_contract, realization)
    require(a_digest == A_MANIFEST and b_digest == input_pin,
            "large-content manifest differs from the known A or pinned B identity")
    require((a_value["entry_count"], a_value["total_bytes"]) == (17, 9989612),
            "A manifest baseline changed")
    require((b_value["entry_count"], b_value["total_bytes"]) == (18, 9990972),
            "B realization bounds changed")
    if realization is not None:
        # The B Tool changes declaration bytes only, so execute its unchanged
        # producer body with the real B mount and admitted-intent projection.
        producer = load_module("static_input_producer_b_check", REPO / TOOL)
        verifier = load_module(
            "static_input_verifier_b_check",
            REPO / "bundles/standard/.ai/tools/ryeos/environments/qualification/static-link-inputs.py")
        producer.SOURCE = realization
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            candidate_contract = project / CONTRACT
            candidate_contract.parent.mkdir(parents=True)
            shutil.copyfile(OVERLAY / CONTRACT, candidate_contract)
            request = {"resolved_config": {"value": b_intent, "source": {
                "bundle_name": "standard", "config_path": INTENT.as_posix(),
                "signer_fingerprint": trust["fingerprint"]}}}
            result = producer.assemble(project, request)
            require(result["input_evidence"]["file_count"] == 12 and
                    result["input_evidence"]["total_file_bytes"] == 9990972,
                    "production helper did not copy the exact B inputs")
            verifier.inventory(project / result["output_path"], verifier.expected_files(profile))
            try:
                verifier.inventory(project / result["output_path"],
                                   verifier.expected_files(a_policy["product_qualification_policy"]
                                                           ["verifier_parameters"]))
            except ValueError:
                pass
            else:
                raise ValueError("A verifier profile accepted the B closure")
    print(f"B source signatures, independent pins, and manifest pass: {b_digest}")
    if realization is not None:
        print(f"Real 12-file B realization, producer copy, and verifier inventory pass: {realization}")


if __name__ == "__main__":
    arguments = argparse.ArgumentParser(description=__doc__)
    arguments.add_argument("--realization", type=Path,
                           help="offline bootstrap output to verify byte for byte")
    check(arguments.parse_args().realization)
