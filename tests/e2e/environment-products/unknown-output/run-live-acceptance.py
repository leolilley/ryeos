#!/usr/bin/env python3
"""Bounded exact-coordinate E2E; never signs, installs, or manages a node."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import time

SPEC = importlib.util.spec_from_file_location(
    "fixture_assertions", Path(__file__).resolve().parents[1] / "assert-live-response.py"
)
ASSERT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ASSERT)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def manifest_for(payload):
    return hashlib.sha256(canonical({
        "entries": [{"blob_hash": hashlib.sha256(payload).hexdigest(), "kind": "file",
                     "mode": 420, "path": "content", "size": len(payload)}],
        "entry_count": 1, "kind": "external_content_manifest",
        "schema": "ryeos.external_content.tree.v2", "total_bytes": len(payload),
    }).encode()).hexdigest()


def object_matching(value, predicate, label):
    return ASSERT.one_object(value, predicate, label)


def accepted_products(produced):
    # Checkpoint state can retain an earlier action value. Only the completed
    # Graph return is the authoritative final producer answer.
    graph_result = produced["result"]["result"]
    if (graph_result.get("definition_ref") != "graph:test/recorded-dynamic-products"
            or graph_result.get("status") != "completed"
            or graph_result.get("success") is not True):
        raise SystemExit("producer wrapper has no successful exact Graph return")
    accepted = graph_result["result"]
    if (accepted.get("schema") != "ryeos.product_build_accepted_result.v1"
            or accepted.get("kind") != "product_build_accepted_result"):
        raise SystemExit("producer Graph did not return accepted named products")
    return accepted


class Run:
    def __init__(self, args):
        self.args = args
        self.counter = 0
        self.env = dict(os.environ, RYEOS_APP_ROOT=str(args.app_root),
                        RYEOSD_URL=args.daemon_url)

    def command(self, label, *arguments):
        self.counter += 1
        stem = self.args.evidence_dir / f"{self.counter:03d}-{label}"
        command = [str(self.args.ryeos_bin), *arguments]
        with stem.with_suffix(".request.json").open("x") as request:
            json.dump(command, request)
        with stem.with_suffix(".json").open("xb") as out:
            with stem.with_suffix(".stderr").open("xb") as err:
                try:
                    result = subprocess.run(command, stdout=out, stderr=err,
                                            env=self.env, timeout=self.args.command_timeout)
                except subprocess.TimeoutExpired:
                    raise SystemExit(f"command timed out; do not relaunch; inspect {stem}")
        if result.returncode:
            raise SystemExit(f"command failed; no retry or relaunch; inspect {stem}")
        return ASSERT.load(stem.with_suffix(".json"))

    def execution(self, label, project, target, selections=None):
        arguments = ["--project", str(project), "execute", target,
                     "--current-head", "--async", "--no-stream"]
        if selections is not None:
            arguments += ["--product-selections", canonical([
                {"target": {"kind": "root"}, "selection": selection}
                for selection in selections
            ])]
        launch = self.command(f"{label}-launch", *arguments, "--input", "{}")
        accepted = object_matching(launch, lambda item:
            isinstance(item.get("thread_id"), str)
            and item.get("status") in {"accepted", "running", "started"}, "accepted root")
        thread = accepted["thread_id"]
        root = accepted.get("chain_root_id", thread)
        deadline = time.monotonic() + self.args.command_timeout
        while True:
            response = self.command(f"{label}-thread", "thread", "get", "--thread-id", thread)
            terminal = object_matching(response, lambda item:
                item.get("thread_id") == thread and item.get("chain_root_id") == root
                and isinstance(item.get("status"), str), "exact execution thread")
            if terminal["status"] == "completed":
                if terminal.get("successor_thread_id") is not None:
                    raise SystemExit("fixture requires an exact non-continued terminal")
                return response, thread, root
            if terminal["status"] in {"failed", "killed", "cancelled", "continued"}:
                raise SystemExit(f"fixture stopped at terminal state {terminal['status']}")
            if time.monotonic() >= deadline:
                raise SystemExit(f"accepted root remains pending: {root}; do not relaunch")
            time.sleep(1)

    def cohort(self, label, project, snapshot):
        graph = self.args.verifier_kind == "graph"
        verifier_ref = ("graph" if graph else "tool") + ":test/verify-dynamic-products"
        consumer_ref = "config:test/dynamic-" + ("graph-" if graph else "") + "qualified-consumer"
        qualification_relationship = "subject_to_graph_consumer" if graph else "subject_to_consumer"
        payload = (project / "variant.txt").read_bytes()
        if payload not in {b"alpha\n", b"beta\n"}:
            raise SystemExit("fixture project input must be exactly alpha or beta plus newline")
        expected = {"subject": manifest_for(payload),
                    "auxiliary": manifest_for(payload[:-1] + b"-aux\n")}
        if self.args.verifier_kind == "workflow":
            return self.workflow_cohort(label, project, snapshot, expected)
        produced, _, _ = self.execution(
            f"{label}-produce", project, "graph:test/recorded-dynamic-products")
        accepted = accepted_products(produced)
        if accepted["producer_project_snapshot_hash"] != snapshot:
            raise SystemExit("producer used a different generation than the explicit context")
        products = {item["product_name"]: item for item in accepted["products"]}
        if set(products) != set(expected):
            raise SystemExit("producer did not return exactly the two named products")
        selections = [{
            "declaration_id": name,
            "witness_hash": ASSERT.require_hash("product witness", products[name]["witness_hash"]),
            "witness_source": {"kind": "local_capture"},
            "qualification_hash": None,
        } for name in sorted(products)]
        request = {
            "consumer_ref": verifier_ref,
            "project_context": {"snapshot_hash": snapshot},
            "selections": selections,
            "maximum_bytes": 256,
        }
        response = self.command(f"{label}-compose-verifier",
            "external-content", "compose-product", canonical(request))
        composed = object_matching(response, lambda item:
            "selected_effective_definition_digest" in item and "bindings" in item,
            "two-slot composition response")
        if composed["selections"] != selections or composed["project_context"] != request["project_context"]:
            raise SystemExit("composition changed the exact selector batch or project context")
        if composed["consumer"].get("kind") != "installed_bundle":
            raise SystemExit("Bundle verifier was reclassified as a project consumer")
        observed = {}
        for binding in composed["bindings"]:
            for declaration in binding["declaration_ids"]:
                if declaration in observed:
                    raise SystemExit("duplicate composed declaration")
                observed[declaration] = binding["manifest_hash"]
        if observed != expected or len(composed["bindings"]) != 2:
            raise SystemExit("composition did not bind both exact distinct file manifests")
        verified, thread, root = self.execution(
            f"{label}-verify", project, verifier_ref, selections)
        result = object_matching(verified, lambda item:
            item.get("schema") == "ryeos.product_qualification_result.v1", "verifier result")
        if (result["subject_manifest_hash"] != expected["subject"]
            or result["claims"] != ["bounded_payload_pair"]
            or result["probe_evidence"] != {
                "auxiliary_manifest_hash": expected["auxiliary"], "network_contacted": False}):
            raise SystemExit("unchanged verifier did not prove the actual selected byte pair")
        qualified = self.command(f"{label}-qualify", "external-content", "qualify-product",
            products["subject"]["witness_hash"], canonical({"kind": "local_capture"}),
            qualification_relationship, root, thread)
        proof = object_matching(qualified, lambda item:
            isinstance(item.get("qualification_hash"), str) and "coordinate_id" in item,
            "qualification publication")
        qualification_hash = ASSERT.require_hash("qualification", proof["qualification_hash"])
        consumer_request = {
            "consumer_ref": consumer_ref,
            "project_context": {"snapshot_hash": snapshot},
            "selections": [{"declaration_id": "subject",
                "witness_hash": products["subject"]["witness_hash"],
                "witness_source": {"kind": "local_capture"},
                "qualification_hash": qualification_hash}],
            "maximum_bytes": 128,
        }
        final = self.command(f"{label}-compose-qualified-consumer",
            "external-content", "compose-product", canonical(consumer_request))
        consumer = object_matching(final, lambda item:
            "selected_effective_definition_digest" in item and "bindings" in item,
            "qualified consumer binding")
        if (consumer["selections"] != consumer_request["selections"]
            or len(consumer["bindings"]) != 1
            or consumer["bindings"][0]["manifest_hash"] != expected["subject"]):
            raise SystemExit("qualified consumer binding lost exact proof or product")
        return {
            "producer_project_snapshot_hash": accepted["producer_project_snapshot_hash"],
            "subject_manifest_hash": expected["subject"],
            "auxiliary_manifest_hash": expected["auxiliary"],
            "pre_selection_digest": composed["pre_selection_effective_definition_digest"],
            "selected_digest": composed["selected_effective_definition_digest"],
            "qualification_hash": qualification_hash,
            "verifier_thread_id": thread, "verifier_chain_root_id": root,
        }

    def workflow_cohort(self, label, project, snapshot, expected):
        # One admitted Graph performs every effectful operation. This harness only
        # launches and reads its exact root, then asserts its complete result.
        response, _, _ = self.execution(
            f"{label}-workflow", project, "graph:test/dynamic-qualification-workflow")
        result = object_matching(response, lambda item:
            item.get("schema") == "ryeos.fixture.dynamic_qualification_workflow.v1",
            "complete authored workflow result")
        if (result["project_context"] != {"snapshot_hash": snapshot}
            or result["products"]["producer_project_snapshot_hash"] != snapshot):
            raise SystemExit("authored workflow lost its exact admitted project context")
        products = {item["product_name"]: item for item in result["products"]["products"]}
        if set(products) != set(expected):
            raise SystemExit("authored workflow did not accept the exact two named products")
        selections = [{"declaration_id": name, "witness_hash": products[name]["witness_hash"],
                       "witness_source": {"kind": "local_capture"},
                       "qualification_hash": None} for name in sorted(products)]
        composed = result["verifier_composition"]
        bindings = {declaration: binding["manifest_hash"]
                    for binding in composed["bindings"]
                    for declaration in binding["declaration_ids"]}
        if (composed["selections"] != selections or bindings != expected
            or len(composed["bindings"]) != 2
            or composed["consumer"].get("kind") != "installed_bundle"
            or composed["project_context"] != result["project_context"]):
            raise SystemExit("authored workflow did not explicitly compose the exact verifier batch")
        proof_result = result["verifier_result"]
        if proof_result != {
            "schema": "ryeos.product_qualification_result.v1",
            "subject_manifest_hash": expected["subject"],
            "claims": ["bounded_payload_pair"],
            "probe_evidence": {"auxiliary_manifest_hash": expected["auxiliary"],
                               "network_contacted": False},
        }:
            raise SystemExit("action-selected verifier did not inspect the exact admitted pair")
        for field in ["verifier_thread_id", "verifier_chain_root_id"]:
            if not isinstance(result[field], str) or not result[field].startswith("T-"):
                raise SystemExit("workflow did not retain exact daemon-authored verifier coordinates")
        ASSERT.require_hash("verifier capsule", result["verifier_capsule_hash"])
        qualification = ASSERT.require_hash(
            "qualification", result["qualification"]["qualification_hash"])
        consumer = result["consumer_composition"]
        if (consumer["selections"] != [{
                "declaration_id": "subject", "witness_hash": products["subject"]["witness_hash"],
                "witness_source": {"kind": "local_capture"},
                "qualification_hash": qualification}]
            or len(consumer["bindings"]) != 1
            or consumer["bindings"][0]["manifest_hash"] != expected["subject"]):
            raise SystemExit("workflow did not finish qualification-gated consumer composition")
        return {
            "producer_project_snapshot_hash": snapshot,
            "subject_manifest_hash": expected["subject"],
            "auxiliary_manifest_hash": expected["auxiliary"],
            "pre_selection_digest": composed["pre_selection_effective_definition_digest"],
            "selected_digest": composed["selected_effective_definition_digest"],
            "qualification_hash": qualification,
            "verifier_thread_id": result["verifier_thread_id"],
            "verifier_chain_root_id": result["verifier_chain_root_id"],
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["ryeos-bin", "app-root", "project-a", "project-b", "evidence-dir"]:
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ["daemon-url", "snapshot-a", "snapshot-b"]:
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--command-timeout", type=int, default=120)
    parser.add_argument("--verifier-kind", choices=["tool", "graph", "workflow"], default="tool",
                        help="graph: inherited probe; workflow: Graph composes and selects Tool child")
    args = parser.parse_args()
    for name in ["ryeos_bin", "app_root", "project_a", "project_b", "evidence_dir"]:
        if not getattr(args, name).is_absolute():
            parser.error(f"{name} must be an explicit absolute path")
    for snapshot in [args.snapshot_a, args.snapshot_b]:
        ASSERT.require_hash("explicit project snapshot", snapshot)
    if args.snapshot_a == args.snapshot_b or args.project_a == args.project_b:
        parser.error("two distinct explicit project generations are required")
    if args.command_timeout < 1 or not args.daemon_url.startswith(("http://", "https://")):
        parser.error("positive timeout and explicit daemon URL required")
    definition_paths = [path.relative_to(Path(__file__).parent / "project")
                        for path in (Path(__file__).parent / "project" / ".ai").rglob("*.yaml")]
    first = {path: (args.project_a / path).read_bytes() for path in definition_paths}
    second = {path: (args.project_b / path).read_bytes() for path in definition_paths}
    if not first or first != second:
        parser.error("the two projects must have byte-identical signed .ai definitions")
    if (args.project_a / "variant.txt").read_bytes() == (args.project_b / "variant.txt").read_bytes():
        parser.error("the two regular input files must differ")
    args.evidence_dir.mkdir(exist_ok=False)
    run = Run(args)
    a = run.cohort("a", args.project_a, args.snapshot_a)
    b = run.cohort("b", args.project_b, args.snapshot_b)
    if a["pre_selection_digest"] != b["pre_selection_digest"]:
        raise SystemExit("the same signed verifier did not retain its input-independent D0")
    for key in ["producer_project_snapshot_hash", "subject_manifest_hash",
                "auxiliary_manifest_hash", "selected_digest", "qualification_hash"]:
        if a[key] == b[key]:
            raise SystemExit(f"two distinct products unexpectedly share {key}")
    with (args.evidence_dir / "acceptance.json").open("x") as destination:
        json.dump({"state": "passed", "a": a, "b": b,
                   "coverage": ("two root slots; two unknown manifest values; " + {
                       "graph": "selected Graph -> ordinary inherited probe",
                       "tool": "direct Tool verifier",
                       "workflow": "one Graph: explicit compose -> action-selected Tool -> qualify -> compose",
                   }[args.verifier_kind])},
                  destination, indent=2)
    print(f"unknown-output {args.verifier_kind}/two-root-slot acceptance passed")


if __name__ == "__main__":
    main()
