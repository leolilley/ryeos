#!/usr/bin/env python3
"""Focused data-only build/qualifier regressions, not live publication proof."""

import json
import io
import copy
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
import runpy
import shutil
from unittest.mock import patch
import yaml


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "tests/fixtures/native-bundle-publication/central-auth-data-only-release.json"
INSPECTOR = ROOT / "scripts/release/inspect-native-bundle-input.py"
BUILD = ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib/native-build.py"
OWNERSHIP_CONFIG = ROOT / "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml"
QUALIFY = ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib/portable-qualify.py"
GRAPH = ROOT / "bundles/bundle-release/.ai/graphs/ryeos/bundle-release/publish.yaml"


def materialize_execution_source(work: Path) -> None:
    shutil.copytree(ROOT / "bundles", work / "bundles")
    parser = work / "scripts/release/bundle-payload-ownership.py"
    parser.parent.mkdir(parents=True)
    shutil.copy2(ROOT / "scripts/release/bundle-payload-ownership.py", parser)


def inspect_central_auth(bundle="central-auth", repository_root=ROOT):
    result = subprocess.run(
        [
            "/usr/bin/python3", str(INSPECTOR),
            "--repository-root", str(repository_root),
            "--bundle", bundle,
            "--source-snapshot-hash", "c" * 64,
            "--target", "portable",
            "--build-profile", "release",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def verified_ownership_fixture():
    # Simulate the parsed, engine-verified Tool input. This does not test the
    # engine's signature/trust resolution, which has its own focused checks.
    signed = OWNERSHIP_CONFIG.read_text(encoding="utf-8")
    return {
        "value": yaml.safe_load(signed),
        "source": {
            "bundle_name": "bundle-release",
            "config_path": "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml",
            "signer_fingerprint": signed.splitlines()[0].rsplit(":", 1)[1],
        },
    }


class DataOnlyBundleAcceptance(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.case = json.loads(FIXTURE.read_text(encoding="utf-8"))

    def test_real_bundle_has_a_zero_payload_portable_plan(self):
        plan = inspect_central_auth()
        self.assertEqual(plan["bundle_name"], self.case["bundle"])
        self.assertEqual(plan["target"], self.case["target"])
        self.assertEqual(plan["payloads"], [])
        self.assertEqual(plan["cargo_packages"], [])
        self.assertFalse(plan["requires_binary_build"])
        self.assertTrue(plan["clean_output_required"])
        self.assertFalse(plan["ambient_target_reuse_allowed"])

    def test_build_copies_the_real_tree_without_invoking_cargo(self):
        plan = inspect_central_auth()
        # Match the service's response envelope and the actual graph projection:
        # policy authority is not part of the closed, digest-bound build input.
        envelope = {
            "release_input": plan,
            "bundle_publication_policy_section_digest": "a" * 64,
            "trust_epoch": 1,
            "authored_version": "0.1.0",
            "bundle_manifest_format": "ryeos.bundle-manifest/v1",
        }
        graph = yaml.safe_load(GRAPH.read_text())["config"]["nodes"]
        projection = graph["inspect"]["assign"]["release_input"]
        self.assertEqual(projection, "${result.release_input}")
        plan = envelope[projection.removeprefix("${result.").removesuffix("}")]
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            materialize_execution_source(work)
            (work / "scripts/release/bundle-payload-ownership.py").unlink()
            fake_bin = work / "bin"
            fake_bin.mkdir()
            cargo_marker = work / "cargo-was-invoked"
            fake_cargo = fake_bin / "cargo"
            fake_cargo.write_text(
                "#!/bin/sh\nprintf invoked > \"$RYEOS_CARGO_MARKER\"\nexit 97\n",
                encoding="utf-8",
            )
            fake_cargo.chmod(0o755)
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_bin}:/usr/bin:/bin"
            environment["RYEOS_CARGO_MARKER"] = str(cargo_marker)
            result = subprocess.run(
                ["/usr/bin/python3", str(BUILD)],
                input=json.dumps({"release_input": plan, "resolved_config": verified_ownership_fixture()}),
                cwd=work,
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(cargo_marker.exists(), "data-only build invoked Cargo")
            testimony = json.loads(result.stdout)
            self.assertEqual(testimony["bundle_name"], "central-auth")
            self.assertEqual(testimony["cargo_packages"], [])
            self.assertEqual(testimony.get("build_kind"), "data_only")
            self.assertEqual(testimony.get("processes"), [])
            self.assertNotIn("process", testimony)

            source = ROOT / "bundles/central-auth"
            product = work / "products/native-bundle/tree"
            self.assertTrue((product / ".ai/manifest.yaml").is_file())
            source_files = sorted(
                path.relative_to(source) for path in source.rglob("*") if path.is_file()
            )
            product_files = sorted(
                path.relative_to(product) for path in product.rglob("*") if path.is_file()
            )
            self.assertEqual(product_files, source_files)
            for relative in source_files:
                if relative == Path(".ai/manifest.yaml"):
                    self.assertEqual(yaml.safe_load((product / relative).read_text()), plan["authored_manifest"])
                else:
                    self.assertEqual((product / relative).read_bytes(), (source / relative).read_bytes())
                mode = stat.S_IMODE((product / relative).stat().st_mode)
                self.assertIn(mode, {0o644, 0o755})
            self.assertEqual(sorted(path.name for path in product.parent.iterdir()), ["tree"])
            subprocess.run(
                [str(ROOT / "scripts/dev/sign-dev.sh"), str(product / ".ai/manifest.yaml")],
                check=True,
                capture_output=True,
                text=True,
            )
            evidence = io.StringIO()
            def realized_path(*parts):
                return product if parts == ("/ryeos/realizations/native-bundle",) else Path(*parts)
            # Explicit simulated admission for this verifier-unit regression.
            # This host invocation does not prove node admission or isolation.
            with patch("pathlib.Path", side_effect=realized_path), patch("sys.stdin", io.StringIO("{}")), patch("sys.stdout", evidence), patch.dict(os.environ, {
                "RYEOS_EXTERNAL_REALIZATIONS": json.dumps([
                    {"id": "python", "manifest_hash": "d" * 64},
                    {"id": "subject", "manifest_hash": "c" * 64},
                ]),
                "RYE_THREAD_ID": "test-qualification",
            }):
                runpy.run_path(str(QUALIFY), run_name="__main__")
            qualification = json.loads(evidence.getvalue())
            self.assertEqual(qualification["claims"], ["portable_bundle_release_checks_v1"])
            self.assertEqual(qualification["subject_manifest_hash"], "c" * 64)
            self.assertEqual(qualification["probe_evidence"]["binary_count"], 0)
            self.assertIn("no-native-payloads", qualification["probe_evidence"]["checks"])

    def test_finalize_metadata_expressions_resolve_from_inspection_envelope(self):
        plan = inspect_central_auth()
        manifest = yaml.safe_load((ROOT / "bundles/central-auth/.ai/manifest.yaml").read_text())
        metadata = {
            "release_input": plan,
            "bundle_publication_policy_section_digest": "a" * 64,
            "trust_epoch": 1,
            "authored_version": manifest["version"],
            "bundle_manifest_format": "ryeos.bundle-manifest/v1",
        }
        scope = {"inputs": {"substrate_protocol": 1}, "state": {"release_input": plan, "release_metadata": metadata}}
        nodes = yaml.safe_load(GRAPH.read_text())["config"]["nodes"]
        self.assertEqual(nodes["inspect"]["assign"]["release_metadata"], "${result}")
        params = nodes["finalize"]["action"]["params"]
        expressions = {
            "authored_version": params["generation"]["authored_version"],
            "substrate_protocol": params["generation"]["substrate_protocol"],
            "bundle_manifest_format": params["generation"]["bundle_manifest_format"],
            "trust_epoch": params["trust_epoch"],
            "bundle_publication_policy_section_digest": params["bundle_publication_policy_section_digest"],
        }
        expected = {**metadata, "substrate_protocol": 1}
        for field, expression in expressions.items():
            value = scope
            for part in expression.removeprefix("${").removesuffix("}").split("."):
                value = value[part]
            self.assertEqual(value, expected[field])

    def test_source_only_manifest_is_materialized_without_prebuilt_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            materialize_execution_source(work)
            # Local population may legitimately create the repository manifest.
            # Establish source-only state solely in our disposable fixture.
            manifest = work / "bundles/bundle-release/.ai/manifest.yaml"
            manifest.unlink(missing_ok=True)
            self.assertFalse(manifest.exists())
            plan = inspect_central_auth("bundle-release", repository_root=work)
            result = subprocess.run(
                ["/usr/bin/python3", str(BUILD)],
                input=json.dumps({"release_input": plan, "resolved_config": verified_ownership_fixture()}), cwd=directory,
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            generated = Path(directory) / "products/native-bundle/tree/.ai/manifest.yaml"
            self.assertEqual(yaml.safe_load(generated.read_text()), plan["authored_manifest"])

    def test_build_rejects_authority_fields_in_closed_input(self):
        plan = inspect_central_auth()
        plan["trust_epoch"] = 1
        plan["bundle_publication_policy_section_digest"] = "a" * 64
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                ["/usr/bin/python3", str(BUILD)],
                input=json.dumps({"release_input": plan, "resolved_config": verified_ownership_fixture()}), cwd=directory,
                capture_output=True, text=True,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("release input shape changed", result.stderr)

    def test_build_requires_engine_resolved_ownership_and_exact_projection(self):
        plan = inspect_central_auth()
        verified = verified_ownership_fixture()
        forged = copy.deepcopy(plan)
        forged["payloads"] = [{
            "bundle": "central-auth", "binary": "forged", "cargo_package": "forged",
            "build_class": "release", "bundle_sets": ["release-authority"],
        }]
        cases = [
            ({"release_input": plan}, "verified ownership resolution required"),
            ({"release_input": forged, "resolved_config": verified}, "exact ownership selection"),
            ({"release_input": plan, "resolved_config": {**verified, "source": {
                **verified["source"], "bundle_name": "another-bundle",
            }}}, "another source"),
            ({"release_input": plan, "resolved_config": {**verified, "value": {
                **verified["value"], "payload_ownership": {
                    **verified["value"]["payload_ownership"],
                    "bundles": verified["value"]["payload_ownership"]["bundles"] + [{
                        "bundle_name": "zz-data-only", "bundle_sets": ["release-authority"],
                        "payloads": [],
                    }],
                },
            }}}, "data-only bundles must be absent"),
        ]
        for request, message in cases:
            with self.subTest(message=message), tempfile.TemporaryDirectory() as directory:
                result = subprocess.run(
                    ["/usr/bin/python3", "-I", "-B", str(BUILD)],
                    input=json.dumps(request), cwd=directory,
                    capture_output=True, text=True,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)

    def test_qualification_has_distinct_truthful_captured_tree_checks(self):
        source = QUALIFY.read_text(encoding="utf-8")
        for check in self.case["qualification_checks"]:
            self.assertIn(check, source)
        self.assertIn("portable_bundle_release_checks_v1", source)
        self.assertNotIn("portable-data-only-plan", source)

    def test_data_only_uses_the_same_signing_and_remote_catalog_flow(self):
        graph = GRAPH.read_text(encoding="utf-8")
        ordered = [
            "service:bundle-release/generation-build",
            "service:bundle-release/request-tree-signing",
            "service:bundle-release/generation-capture",
            "service:bundle-release/generation-qualify",
            "service:bundle-release/generation-finalize",
            "service:bundle-release/request-authorization",
            *self.case["catalog_operations"],
        ]
        offsets = [graph.index(operation) for operation in ordered]
        self.assertEqual(offsets, sorted(offsets))
        self.assertNotIn("service:bundle-catalog/stage-local", graph)

    def test_normal_path_does_not_build_an_image_or_daemon(self):
        surface = "\n".join([
            GRAPH.read_text(encoding="utf-8"),
            BUILD.read_text(encoding="utf-8"),
            QUALIFY.read_text(encoding="utf-8"),
        ])
        for forbidden in ["docker build", "buildx", "Dockerfile.release", "ryeosd", "ryeos-substrate"]:
            self.assertNotIn(forbidden, surface)
        self.assertEqual(
            self.case["substrate_image_before"],
            self.case["substrate_image_after"],
        )


if __name__ == "__main__":
    unittest.main()
