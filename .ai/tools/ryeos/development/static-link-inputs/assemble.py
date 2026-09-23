# ryeos:signed:2026-09-22T04:12:04Z:ebd91bb29b15b95c3393b55fe67d1dcab48fe1695df1b9c5ea5e2b78f1af8a8b:pJsiQxOPfYTM+IHw1KvlZZ6cAwIDcEovdUJB/zvszvI5cIg85KK7HO9Q9AG1+cyAEx8beAvmqxc6dbbLNuqMBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/static-link-inputs
#   version: "1.0.0"
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
#       digest: e16652fd10ff718182fe2b09fd8a4c913fc8c994baacf833e3e6a29a69750ebf
#       mount_root: execution_runtime
#       mount: static-link-inputs

"""Offline reproduction only; no acquisition, binding or qualification grant."""
import hashlib
import importlib.util
import json
from pathlib import Path
import sys

SOURCE = Path("/ryeos/realizations/static-link-inputs")
OUTPUT = Path("products/static-link-inputs")
CONTRACT = Path(".ai/config/development/ryeos/static-link-inputs.yaml")
CONTRACT_BODY_SHA256 = "83e22140d7e6e7cf696816d8a150c5f4b2714d8c7898a5c54653d78f23f5d681"


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
    if not isinstance(request, dict) or request.get("resolved_config") not in ({}, None):
        raise ValueError("static-input production accepts no resolved configuration")
    path = project / CONTRACT
    if path.is_symlink() or not path.is_file() or path.stat().st_size > 65536:
        raise ValueError("unsafe static-input contract")
    if not path.resolve().is_relative_to(project.resolve()):
        raise ValueError("static-input contract escaped admitted project")
    envelope, body = path.read_bytes().split(b"\n", 1)
    # Project admission supplies signature authority. Pin the exact signed body
    # here, rather than accepting arbitrary project bytes or caller configuration.
    if (not envelope.startswith(b"# ryeos:signed:")
            or hashlib.sha256(body).hexdigest() != CONTRACT_BODY_SHA256):
        raise ValueError("static-input contract differs from exact admitted recipe")
    contract = helper("inputs").read_contract(path)
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
