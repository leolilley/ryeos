# ryeos:signed:2026-09-29T06:43:34Z:c86f29ad8bf066573a7c460aad242253c07e7a5ab80302f39d17c1d2f81ef2d3:ZGwF8e+T55DrJppvrwRfLb3c7rr2TPgeOTmCZFdVsICtUzk5Iuon+KqBXGq1zZJBWg6mWWDyxMQEqqGIglqOAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/static-link-inputs
#   version: "2.1.0"
#   description: Reproduce exact admitted supplementary static-link inputs as a retained product
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false
#   config_resolve:
#     type: strict_signed_project_bundle
#     bundle_name: standard
#     config_path: bundles/standard/.ai/config/development/ryeos/static-link-contract-intent.yaml
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#     - id: static-link-inputs
#       kind: tree
#       mode: pinned
#       digest: 2d949d2079ffa86459ccba2960b73332893aa9adebb0fb65b1f9e4d0576b5ff1
#       mount_root: execution_runtime
#       mount: static-link-inputs

"""Offline reproduction only; no acquisition, binding or qualification grant."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import sys

SOURCE = Path("/ryeos/realizations/static-link-inputs")
OUTPUT = Path("products/static-link-inputs")
CONTRACT = Path(".ai/config/development/ryeos/static-link-inputs.yaml")
INTENT_CONFIG = "bundles/standard/.ai/config/development/ryeos/static-link-contract-intent.yaml"
HASH = re.compile(r"[0-9a-f]{64}")
MAX_CONTRACT_BYTES = 65536


def admitted_contract_expectation(request):
    # The empty caller schema is checked before the strict resolver injects
    # this projection from admitted project source. Installed Standard anchors
    # its publisher; installed configuration is never the selected contract.
    if not isinstance(request, dict) or set(request) != {"resolved_config"}:
        raise ValueError("static-input production requires the admitted contract intent")
    resolved = request["resolved_config"]
    if not isinstance(resolved, dict) or set(resolved) != {"value", "source"}:
        raise ValueError("static-input intent requires the strict Config projection")
    source = resolved["source"]
    if (not isinstance(source, dict) or set(source) != {
            "bundle_name", "config_path", "signer_fingerprint"} or
            source["bundle_name"] != "standard" or source["config_path"] != INTENT_CONFIG or
            not isinstance(source["signer_fingerprint"], str) or
            not HASH.fullmatch(source["signer_fingerprint"])):
        raise ValueError("static-input intent came from an unexpected source")
    intent = resolved["value"]
    if (not isinstance(intent, dict) or set(intent) != {
            "category", "name", "version", "schema", "expected_contract_body_sha256"} or
            intent["category"] != "development/ryeos" or
            intent["name"] != "static-link-contract-intent" or
            intent["schema"] != "ryeos.static_link_contract_intent.v1" or
            not isinstance(intent["version"], str) or not intent["version"] or
            not isinstance(intent["expected_contract_body_sha256"], str) or
            not HASH.fullmatch(intent["expected_contract_body_sha256"])):
        raise ValueError("unsupported or incomplete static-input contract intent")
    return intent["expected_contract_body_sha256"]


def stable_contract(project):
    # No live overlay, symlink ancestor, or second read may replace the bytes
    # compared with the independent intent. The runtime owns immutable source.
    parent = project
    for part in CONTRACT.parts[:-1]:
        parent = parent / part
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError("unsafe static-input contract parent")
    descriptor = os.open(project / CONTRACT, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or not 1 <= before.st_size <= MAX_CONTRACT_BYTES:
            raise ValueError("unsafe or oversized static-input contract")
        chunks, total = [], 0
        while True:
            chunk = os.read(descriptor, min(65536, MAX_CONTRACT_BYTES + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
            if total > MAX_CONTRACT_BYTES:
                raise ValueError("static-input contract exceeds its finite bound")
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_size,
                              value.st_mtime_ns, value.st_ctime_ns)
    if identity(before) != identity(after):
        raise ValueError("static-input contract changed while read")
    return b"".join(chunks)


def helper(name):
    # The executor supplies __file__ from its verified read-only Tool source.
    path = Path(__file__).parent / "lib" / (name + ".py")
    spec = importlib.util.spec_from_file_location("static_inputs_" + name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def assemble(project, request):
    project = Path(project)
    if not project.is_absolute() or project.is_symlink() or not project.is_dir():
        raise ValueError("missing absolute admitted project context")
    expected = admitted_contract_expectation(request)
    envelope, body = stable_contract(project).split(b"\n", 1)
    # The independently signed intent selects the exact body; the source file
    # cannot authenticate its own observation or supply a replacement pin.
    if (not envelope.startswith(b"# ryeos:signed:")
            or hashlib.sha256(body).hexdigest() != expected):
        raise ValueError("static-input contract differs from exact admitted recipe")
    contract = helper("inputs").parse_contract(body.decode("utf-8", "strict"))
    destination = project / OUTPUT
    parent = destination.parent
    if parent.is_symlink() or (parent.exists() and not parent.is_dir()):
        raise ValueError("unsafe static-input product parent")
    parent.mkdir(mode=0o700, exist_ok=True)
    evidence = helper("produce").produce(SOURCE, destination, contract)
    return {"ok": True, "output_path": OUTPUT.as_posix(),
            "input_evidence": evidence, "network_acquisition": False,
            "binding_published": False}


def main():
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    request = sys.stdin.buffer.read(65537)
    if len(request) > 65536:
        raise ValueError("static-input production request exceeds its bound")
    print(json.dumps(assemble(sys.argv[2], json.loads(request)), sort_keys=True))


if __name__ == "__main__":
    main()
