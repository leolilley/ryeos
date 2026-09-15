#!/usr/bin/env python3
"""Exercise the exact prebuilt verifier bytes, not RyeOS admission or E2E."""
import argparse
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("dynamic_harness", ROOT / "run-live-acceptance.py")
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)
ELF_SPEC = importlib.util.spec_from_file_location("fixture_elf", ROOT / "check-static-elf.py")
ELF = importlib.util.module_from_spec(ELF_SPEC)
ELF_SPEC.loader.exec_module(ELF)


def body(path, name):
    text = path.read_text()
    start = f"        # fixture-dynamic-{name}-begin\n"
    end = f"        # fixture-dynamic-{name}-end\n"
    if text.count(start) != 1 or text.count(end) != 1:
        raise SystemExit("fixture literal body markers are missing or ambiguous")
    return "\n".join(line[8:] for line in text.split(start)[1].split(end)[0].splitlines())


def check_tool_sources(verifier, probe):
    sources = {path: path.read_bytes() for path in [verifier, probe]}
    for path, content in sources.items():
        text = content.decode()
        if text.count("command: bin:fixture-dynamic-product-verifier") != 1:
            raise SystemExit(f"{path} does not select the one packaged verifier executable")
        if "filesystem_authority: captured_execution" not in text:
            raise SystemExit(f"{path} does not require captured execution")
        if any(forbidden in text for forbidden in ["local_binary", "/usr/bin/", "env_config:"]):
            raise SystemExit(f"{path} retains an undeclared host executable dependency")
    if "external_product_slots:" not in sources[verifier].decode():
        raise SystemExit("direct verifier has no selected product slots")
    if "external_product_slots:" in sources[probe].decode():
        raise SystemExit("Graph child must inherit normalized inputs without new selectors")
    return sources


def main():
    expected_return = {"schema": "ryeos.product_build_accepted_result.v1",
                       "kind": "product_build_accepted_result", "products": []}
    graph_return = {"definition_ref": "graph:test/recorded-dynamic-products",
                    "status": "completed", "success": True,
                    "result": expected_return,
                    "state": {"products": {**expected_return, "earlier": True}}}
    assert HARNESS.accepted_products({"result": {"result": graph_return}}) == expected_return
    try:
        HARNESS.accepted_products({"result": {"result": {**graph_return, "success": False}}})
    except SystemExit:
        pass
    else:
        raise AssertionError("failed Graph return was accepted from checkpoint state")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verifier-binary", type=Path, required=True)
    args = parser.parse_args()
    if not args.verifier_binary.is_absolute():
        parser.error("verifier_binary must be an explicit absolute path")
    if not args.verifier_binary.is_file() or args.verifier_binary.is_symlink():
        parser.error("verifier_binary must be one regular non-symlink file")

    producer = ROOT / "project/.ai/tools/test/produce-dynamic.yaml"
    verifier = ROOT / "bundle-overlay/.ai/tools/test/verify-dynamic-products.yaml"
    probe = ROOT / "bundle-overlay/.ai/tools/test/probe-dynamic-products.yaml"
    graph = ROOT / "bundle-overlay/.ai/graphs/test/verify-dynamic-products.yaml"
    source_before = check_tool_sources(verifier, probe)
    binary_before = args.verifier_binary.read_bytes()
    ELF.check(args.verifier_binary)
    graph_text = graph.read_text()
    if "product_selections:" in graph_text:
        raise SystemExit("ordinary Graph probe must not carry authored selectors")
    if "item_id: tool:test/probe-dynamic-products" not in graph_text:
        raise SystemExit("Graph no longer calls the exact installed probe")

    workflow = (ROOT / "project/.ai/graphs/test/dynamic-qualification-workflow.yaml").read_text()
    assert 'thread_id: "${execution.thread_id}"' in workflow
    assert 'verifier_thread_id: "${dispatch.child_thread_id}"' in workflow
    assert 'verifier_chain_root_id: "${result.thread.chain_root_id}"' in workflow
    assert "product_selections:" in workflow
    assert workflow.count("witness_source:") == 4
    assert workflow.count("kind: local_capture") == 2
    assert workflow.count("item_id: service:external-content/compose-product") == 2
    assert "item_id: service:external-content/qualify-product" in workflow
    assert "consumer_project_path:" not in workflow
    assert "service:threads/list" not in workflow

    bash = shutil.which("bash")
    if bash is None:
        raise SystemExit("fixture producer requires Bash; no alternate interpreter")
    results = []
    with tempfile.TemporaryDirectory(prefix="ryeos-dynamic-product-fixture-") as directory:
        for variant in ["alpha", "beta"]:
            project = Path(directory) / variant
            project.mkdir()
            (project / "variant.txt").write_text(variant + "\n")
            subprocess.run(
                [bash, "--noprofile", "--norc", "-c", body(producer, "producer"),
                 "fixture", str(project)],
                check=True, capture_output=True, timeout=30,
            )
            (project / "qualification").mkdir()
            expected = {}
            for name in ["subject", "auxiliary"]:
                source = project / "products/dynamic" / name
                target = project / "qualification" / name
                shutil.copyfile(source, target)
                target.chmod(0o444)
                expected[name] = HARNESS.manifest_for(source.read_bytes())
            result = json.loads(subprocess.run(
                [args.verifier_binary, str(project)], check=True, capture_output=True,
                text=True, timeout=30,
            ).stdout)
            assert result == {
                "schema": "ryeos.product_qualification_result.v1",
                "subject_manifest_hash": expected["subject"],
                "claims": ["bounded_payload_pair"],
                "probe_evidence": {
                    "auxiliary_manifest_hash": expected["auxiliary"],
                    "network_contacted": False,
                },
            }
            results.append(result)
            auxiliary = project / "qualification/auxiliary"
            auxiliary.chmod(0o644)
            auxiliary.write_text("different-aux\n")
            rejected = subprocess.run(
                [args.verifier_binary, str(project)], capture_output=True, text=True, timeout=30,
            )
            assert rejected.returncode != 0 and not rejected.stdout

    if args.verifier_binary.read_bytes() != binary_before:
        raise SystemExit("fixture verifier bytes changed while being exercised")
    if {path: path.read_bytes() for path in source_before} != source_before:
        raise SystemExit("fixture Tool source changed while one verifier was exercised")
    assert results[0]["subject_manifest_hash"] != results[1]["subject_manifest_hash"]
    for path in (ROOT / "project/.ai").rglob("*.yaml"):
        assert "expected_manifest_hash:" not in path.read_text()
    print("two unknown manifests, one static verifier, and mismatched-slot refusal checked")
    print("No Cargo, signing, RyeOS admission, node, capture, composition, or qualification was run")


if __name__ == "__main__":
    main()
