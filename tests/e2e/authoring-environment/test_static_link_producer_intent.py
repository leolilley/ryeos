"""Source contracts and production helper tests, not installed admission proof.

Fixtures model an already admitted Config projection and realization mount.
The existing engine strict resolver owns signature verification and sealing.
"""

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import yaml


ROOT = Path(__file__).resolve().parents[3]
TOOL = ROOT / ".ai/tools/ryeos/development/static-link-inputs/assemble.py"
INTENT_PATH = "bundles/environment-qualification/.ai/config/development/ryeos/static-link-contract-intent.yaml"
INTENT = ROOT / INTENT_PATH
SIGNER = "a" * 64


def load_tool():
    spec = importlib.util.spec_from_file_location("static_producer", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tool_header():
    lines = []
    for line in TOOL.read_text().splitlines():
        if not line.startswith("#"):
            break
        if line.startswith("# ") and not line.startswith("# ryeos:signed:"):
            lines.append(line[2:])
    return yaml.safe_load("\n".join(lines))["ryeos-tool"]


def fixture_contract(version):
    name = f"usr/lib/x86_64-linux-gnu/libfixture-{version}.a"
    data = f"synthetic retained archive {version}\n".encode()
    contract = {
        "category": "development/ryeos", "name": "static-link-inputs", "version": version,
        "schema": "ryeos.development.static_link_inputs.v1",
        "publisher_image": "docker.io/library/rust@sha256:" + "b" * 64,
        "target": "x86_64-unknown-linux-gnu",
        "inputs": {name: {"source": "/" + name, "bytes": len(data), "mode": 0o644,
                          "sha256": hashlib.sha256(data).hexdigest()}},
    }
    return json.dumps(contract, sort_keys=True, indent=2).encode() + b"\n", name, data


def authored_request(body):
    intent = yaml.safe_load(INTENT.read_text())
    intent["expected_contract_body_sha256"] = hashlib.sha256(body).hexdigest()
    return {"resolved_config": {"value": intent, "source": {
        "bundle_name": "standard", "config_path": INTENT_PATH,
        "signer_fingerprint": SIGNER,
    }}}


def prepare_fixture(root, module, version):
    project, source = root / "project", root / "realization"
    project.mkdir()
    source.mkdir()
    body, name, data = fixture_contract(version)
    contract_path = project / module.CONTRACT
    contract_path.parent.mkdir(parents=True)
    contract_path.write_bytes(b"# ryeos:signed:synthetic-source-envelope\n" + body)
    member = source / name
    member.parent.mkdir(parents=True)
    member.write_bytes(data)
    member.chmod(0o644)
    return project, source, contract_path, body, name, data


class StaticLinkProducerIntentTests(unittest.TestCase):
    def setUp(self):
        self.module = load_tool()

    def test_tool_uses_exact_source_intent_and_keeps_independent_realization(self):
        tool = tool_header()
        self.assertEqual(tool["config_schema"], {
            "type": "object", "properties": {}, "additionalProperties": False,
        })
        self.assertEqual(tool["config_resolve"], {
            "type": "strict_signed_project_bundle", "bundle_name": "standard",
            "config_path": INTENT_PATH,
        })
        self.assertEqual(tool["workspace_access"], "immutable_current_generation")
        self.assertEqual(tool["filesystem_authority"], "captured_execution")
        self.assertEqual(tool["network_authority"], "isolated")
        subject = next(item for item in tool["external_content"] if item["id"] == "static-link-inputs")
        self.assertEqual(subject["mode"], "pinned")
        self.assertEqual(subject["mount_root"], "execution_runtime")
        self.assertEqual(subject["mount"], "static-link-inputs")
        self.assertRegex(subject["digest"], r"^[0-9a-f]{64}$")
        graph = yaml.safe_load((ROOT / ".ai/graphs/ryeos/development/static-link-inputs-production.yaml").read_text())
        self.assertEqual(graph["config"]["nodes"]["produce"]["action"]["params"], {})
        self.assertNotIn("CONTRACT_BODY_SHA256", TOOL.read_text())
        _, body = (ROOT / self.module.CONTRACT).read_bytes().split(b"\n", 1)
        self.assertEqual(yaml.safe_load(INTENT.read_text())["expected_contract_body_sha256"],
                         hashlib.sha256(body).hexdigest())

    def test_two_contract_generations_use_same_cli_and_production_helpers(self):
        tool_hash = hashlib.sha256(TOOL.read_bytes()).hexdigest()
        # Only the admitted mount path is substituted in this process fixture;
        # expectation travels through production stdin, not a patched constant.
        entry = """
import importlib.util, pathlib, sys
tool, realization, project = sys.argv[1:]
spec = importlib.util.spec_from_file_location('producer_entry', tool)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.SOURCE = pathlib.Path(realization)
sys.argv = [tool, '--project-path', project]
module.main()
"""
        for version in ("1.0.0", "1.1.0"):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as temporary:
                project, source, path, body, name, data = prepare_fixture(Path(temporary), self.module, version)
                before = path.stat()
                source_before = (source / name).stat()
                run = subprocess.run(
                    [sys.executable, "-I", "-B", "-c", entry, str(TOOL), str(source), str(project)],
                    input=json.dumps(authored_request(body)).encode(), stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE, timeout=10, check=False,
                    env={"LANG": "C", "LC_ALL": "C", "PATH": ""},
                )
                self.assertEqual(run.returncode, 0, run.stderr.decode())
                result = json.loads(run.stdout)
                self.assertTrue(result["ok"])
                self.assertFalse(result["network_acquisition"])
                self.assertFalse(result["binding_published"])
                self.assertEqual((project / result["output_path"] / name).read_bytes(), data)
                self.assertEqual((source / name).read_bytes(), data)
                self.assertEqual(path.read_bytes().split(b"\n", 1)[1], body)
                for first, last in ((before, path.stat()), (source_before, (source / name).stat())):
                    self.assertEqual((first.st_ino, first.st_size, first.st_mtime_ns, first.st_ctime_ns),
                                     (last.st_ino, last.st_size, last.st_mtime_ns, last.st_ctime_ns))
                with mock.patch.object(self.module, "SOURCE", source):
                    with self.assertRaisesRegex(ValueError, "already exists"):
                        self.module.assemble(project, authored_request(body))
        self.assertEqual(hashlib.sha256(TOOL.read_bytes()).hexdigest(), tool_hash)

    def test_swapped_body_intents_and_wrong_source_products_refuse(self):
        for selected, rejected in (("1.0.0", "1.1.0"), ("1.1.0", "1.0.0")):
            with self.subTest(selected=selected), tempfile.TemporaryDirectory() as temporary:
                project, source, path, body, name, data = prepare_fixture(Path(temporary), self.module, selected)
                other, _, _ = fixture_contract(rejected)
                with mock.patch.object(self.module, "SOURCE", source):
                    with self.assertRaisesRegex(ValueError, "differs from exact admitted recipe"):
                        self.module.assemble(project, authored_request(other))
                    self.assertFalse((project / self.module.OUTPUT).exists())
                    (source / name).write_bytes(b"X" * len(data))
                    with self.assertRaisesRegex(ValueError, "digest mismatch"):
                        self.module.assemble(project, authored_request(body))
                    self.assertFalse((project / self.module.OUTPUT).exists())

    def test_missing_wrong_source_or_self_reported_pin_refuses(self):
        body, _, _ = fixture_contract("1.0.0")
        correct = authored_request(body)
        invalid = [{}, {"expected_contract_body_sha256": hashlib.sha256(body).hexdigest()},
                   {"resolved_config": correct["resolved_config"]["value"]}]
        for key, value in (("bundle_name", "bundle-release"), ("config_path", ".ai/config/intent.yaml"),
                           ("signer_fingerprint", "unsigned")):
            request = copy.deepcopy(correct)
            request["resolved_config"]["source"][key] = value
            invalid.append(request)
        for key, value in (("expected_contract_body_sha256", "A" * 64), ("output", "source"),
                           ("schema", "ryeos.static_link_contract_intent.v2")):
            request = copy.deepcopy(correct)
            request["resolved_config"]["value"][key] = value
            invalid.append(request)
        for request in invalid:
            with self.subTest(request=request), self.assertRaises(ValueError):
                self.module.admitted_contract_expectation(request)

    def test_stable_contract_read_rejects_race_and_symlink_ancestor(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project, _, path, _, _, _ = prepare_fixture(root, self.module, "1.0.0")
            real_fstat = os.fstat
            calls = 0

            def changed(descriptor):
                nonlocal calls
                value = real_fstat(descriptor)
                calls += 1
                if calls == 2:
                    fields = list(value)
                    fields[8] += 1
                    return os.stat_result(fields)
                return value

            with mock.patch.object(self.module.os, "fstat", side_effect=changed):
                with self.assertRaisesRegex(ValueError, "changed while read"):
                    self.module.stable_contract(project)
            (project / ".ai").rename(root / "outside")
            (project / ".ai").symlink_to(root / "outside", target_is_directory=True)
            with self.assertRaisesRegex(ValueError, "unsafe.*parent"):
                self.module.stable_contract(project)

    def test_existing_reader_and_exact_byte_parser_share_strict_validation(self):
        parser = self.module.helper("inputs")
        path = ROOT / self.module.CONTRACT
        self.assertEqual(parser.read_contract(path), parser.parse_contract(path.read_text()))
        with self.assertRaisesRegex(ValueError, "duplicate"):
            parser.parse_contract('{"schema":"first","schema":"second"}')
        with self.assertRaisesRegex(ValueError, "oversized"):
            parser.parse_contract(" " * 65537)
        body, _, _ = fixture_contract("1.0.0")
        contract = json.loads(body)
        contract["inputs"]["../escape.a"] = next(iter(contract["inputs"].values()))
        with self.assertRaisesRegex(ValueError, "unsafe relative path"):
            parser.parse_contract(json.dumps(contract))


if __name__ == "__main__":
    unittest.main()
