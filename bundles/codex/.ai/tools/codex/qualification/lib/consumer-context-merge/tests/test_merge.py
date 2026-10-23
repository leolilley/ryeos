"""Merge-tool cases. Digests here are synthetic, format-valid test values only."""

import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

import yaml

MERGE = Path(__file__).resolve().parents[1] / "merge.py"
spec = importlib.util.spec_from_file_location("consumer_context_merge", MERGE)
merge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(merge)


def digest(letter):
    return letter * 64


EMITTED = {
    "policy_ref": "config:ryeos/environments/qualification/native-authoring",
    "relationship_name": "runtime_to_external_authoring_worker",
    "subject_manifest_hash": digest("a"),
    "bundle_generation_identity": "generation-synthetic-1",
    "verifier_parameters": {
        "external_candidate_qualification_context": {
            "schema": "ryeos.external_candidate_qualification_context.v3",
            "requirement_digest": digest("1"),
            "profile_hash": digest("2"),
            "source_binding_hash": digest("3"),
            "source_content_manifest_hash": digest("4"),
            "provider_executable_manifest_hash": digest("5"),
            "execution_environment_digest": digest("6"),
        }
    },
    "consumer_execution_context": {
        "worker_ref": "worker:codex/external-hosted-authoring",
        "product_declaration_id": "guest-runtime",
        "environment_ref": "config:codex/environments/external-authoring",
        "worker_execution_ref": "worker_execution:codex/bounded-turn",
        "environment_binding": "environment",
    },
}

TEMPLATE = {
    "category": "ryeos/environments/qualification",
    "version": "1.0.0",
    "product_qualification_policy": {
        "schema": "ryeos.product_qualification_policy.v5",
        "verifier_ref": "tool:codex/qualification/consumer-runtime-verifier",
        "subject_declaration_id": "guest-runtime",
        "allowed_claims": ["bounded_candidate_capture"],
        "minimum_verifier_process_settlement": "scope_empty",
        "verifier_parameters": {
            "schema": "ryeos.codex.consumer_verifier.v4",
            "controller": {"scenario_id": "remote_codex", "maximum_duration_ms": 120000},
            "expected_command_output": "probe",
            "responses_origin": "http://127.0.0.1:1",
            "outer_network": "shared_guest_loopback",
            "qualification_worker": {
                "schema": "ryeos.codex.q_command.v3",
                "environment_id": "ryeos-external-candidate",
                "command": "printf probe",
                "command_shell": "zsh",
                "command_presentation": "zsh -c 'printf probe'",
                "cwd": "/workspace",
                "output": "probe",
                "exit_code": 0,
                "expected_final_content_b64": "YWZ0ZXI=",
            },
        },
        "producer_scenarios": {"remote_codex": {"consumer_worker_request": {}}},
    },
}


def refusal(case, fn, *args):
    with case.assertRaises(merge.MergeRefused) as caught:
        fn(*args)
    return str(caught.exception)


class MergeTests(unittest.TestCase):
    def setUp(self):
        self.schema = merge.verifier_config_schema()

    def merged(self, template=TEMPLATE, emitted=EMITTED):
        policy = copy.deepcopy(template)
        merge.merge(policy, merge.parse_emitted(copy.deepcopy(emitted)), False, self.schema)
        return policy

    def test_merge_populates_both_contexts_and_validates(self):
        section = self.merged()["product_qualification_policy"]
        self.assertEqual(
            section["verifier_parameters"]["external_candidate_qualification_context"],
            EMITTED["verifier_parameters"]["external_candidate_qualification_context"],
        )
        self.assertEqual(
            section["consumer_execution_context"], EMITTED["consumer_execution_context"]
        )

    def test_template_without_codex_side_data_is_refused(self):
        for field in merge.CODEX_SIDE_FIELDS:
            template = copy.deepcopy(TEMPLATE)
            del template["product_qualification_policy"]["verifier_parameters"][field]
            refusal(self, self.merged, template)
        template = copy.deepcopy(TEMPLATE)
        template["product_qualification_policy"]["verifier_parameters"] = {}
        refusal(self, self.merged, template)
        template = copy.deepcopy(TEMPLATE)
        template["product_qualification_policy"]["producer_scenarios"] = {}
        refusal(self, self.merged, template)

    def test_malformed_emitted_fields_are_refused(self):
        def mutate(path, value):
            doc = copy.deepcopy(EMITTED)
            node = doc
            for key in path[:-1]:
                node = node[key]
            if value is KeyError:
                del node[path[-1]]
            else:
                node[path[-1]] = value
            return doc

        context = ("verifier_parameters", "external_candidate_qualification_context")
        cases = [
            (("subject_manifest_hash",), "short"),
            (("bundle_generation_identity",), ""),
            (("policy_ref",), KeyError),
            (context + ("schema",), "ryeos.external_candidate_qualification_context.v2"),
            (("consumer_execution_context", "worker_ref"), ""),
            (("consumer_execution_context", "environment_binding"), KeyError),
        ]
        for field in merge.DIGEST_FIELDS:
            cases.append((context + (field,), "A" * 64))
            cases.append((context + (field,), digest("a")[:63]))
            cases.append((context + (field,), KeyError))
        for path, value in cases:
            refusal(self, merge.parse_emitted, mutate(path, value))
        extra = copy.deepcopy(EMITTED)
        extra["invented"] = 1
        refusal(self, merge.parse_emitted, extra)

    def test_merged_policy_that_breaks_the_tool_schema_is_refused(self):
        template = copy.deepcopy(TEMPLATE)
        template["product_qualification_policy"]["verifier_parameters"]["outer_network"] = "open"
        refusal(self, self.merged, template)

    def test_populated_policy_is_idempotent_and_replace_gated(self):
        policy = self.merged()
        emitted = merge.parse_emitted(copy.deepcopy(EMITTED))
        self.assertFalse(merge.merge(policy, emitted, False, self.schema))
        other = copy.deepcopy(EMITTED)
        other["verifier_parameters"]["external_candidate_qualification_context"][
            "profile_hash"] = digest("9")
        refusal(self, merge.merge, policy, merge.parse_emitted(other), False, self.schema)
        self.assertTrue(merge.merge(policy, merge.parse_emitted(other), True, self.schema))
        self.assertEqual(
            policy["product_qualification_policy"]["verifier_parameters"][
                "external_candidate_qualification_context"]["profile_hash"],
            digest("9"),
        )

    def test_cli_round_trip_and_second_run_leaves_file_untouched(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            policy_path = root / "policy.yaml"
            policy_path.write_text(
                "# ryeos:signed:2026-01-01T00:00:00Z:x:y:z\n" + yaml.safe_dump(TEMPLATE)
            )
            emitted_path = root / "emitted.json"
            emitted_path.write_text(json.dumps(EMITTED))
            argv = [str(policy_path), "--emitted", str(emitted_path)]
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(merge.main(argv), 0)
            first = policy_path.read_text()
            self.assertFalse(first.startswith("# ryeos:signed:"))
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(merge.main(argv), 0)
            self.assertEqual(policy_path.read_text(), first)
            self.assertEqual(
                yaml.safe_load(first), self.merged(), "CLI output equals library merge"
            )


if __name__ == "__main__":
    unittest.main()
