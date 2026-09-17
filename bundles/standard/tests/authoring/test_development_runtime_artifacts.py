from __future__ import annotations

import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock

import yaml


ROOT = Path(__file__).resolve().parents[4]
AUTHOR_PATH = ROOT / "bundles/standard/authoring/development-runtime/author_tree.py"
SOURCE_AUTHOR_PATH = ROOT / "bundles/standard/authoring/development-runtime/author_source_tree.py"
GNU_ACQUISITION = ROOT / ".ai/config/development/ryeos/gnu-python-acquisition.yaml"
GNU_PRODUCTION = ROOT / ".ai/config/development/ryeos/gnu-python-production-inputs.yaml"
SOURCE_ACQUISITION = ROOT / ".ai/config/development/ryeos/authoring-source-acquisition.yaml"
AUTHORING_INPUTS = ROOT / ".ai/config/development/ryeos/authoring-environment-inputs.yaml"
GNU_ACTIVATION = ROOT / "bundles/standard/.ai/config/development/ryeos/gnu-python-archives-activation.yaml"
GNU_VERIFIER = ROOT / "bundles/standard/.ai/tools/ryeos/environments/qualification/gnu-python-archives.py"
SOURCE_ACTIVATION = ROOT / "bundles/standard/.ai/config/development/ryeos/authoring-source-inputs-activation.yaml"
SOURCE_VERIFIER = ROOT / "bundles/standard/.ai/tools/ryeos/environments/qualification/authoring-source-inputs.py"
spec = importlib.util.spec_from_file_location("author_tree", AUTHOR_PATH)
author_tree = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(author_tree)


class DevelopmentRuntimeArtifactTests(unittest.TestCase):
    def test_gnu_acquisition_is_the_exact_offline_producer_selection(self) -> None:
        acquisition = author_tree.load_config(GNU_ACQUISITION)
        production = yaml.safe_load(GNU_PRODUCTION.read_text())
        selected = {source["target"]: source for source in acquisition["sources"]}
        for role in ("install", "metadata"):
            expected = production["archives"][role]
            actual = selected[expected["member"]]
            self.assertEqual(
                {key: actual[key] for key in ("url", "bytes", "sha256")},
                {key: expected[key] for key in ("url", "bytes", "sha256")},
            )
        zstd = production["license_supplements"]["zstd"]
        self.assertEqual(
            {key: selected[zstd["member"]][key] for key in ("url", "bytes", "sha256")},
            {key: zstd[key] for key in ("url", "bytes", "sha256")},
        )
        build = production["qualification_requirements"]["zlib-ng"]["upstream_build"]
        self.assertEqual(
            {key: selected[build["member"]][key] for key in ("bytes", "sha256")},
            {key: build[key] for key in ("bytes", "sha256")},
        )
        self.assertEqual(
            acquisition["artifact"]["manifest_digest"],
            production["archives"]["manifest"]["digest"],
        )

    def test_synthetic_tree_authors_deterministically_and_refuses_moved_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache, output = root / "cache", root / "output"
            cache.mkdir()
            first, second = b"alpha", b"beta" * 10
            sources = []
            for target, value, mode in (("a", first, 0o644), ("nested/b", second, 0o755)):
                digest = hashlib.sha256(value).hexdigest()
                cached = cache / f"{digest}-{Path(target).name}"
                cached.write_bytes(value)
                sources.append({
                    "target": target,
                    "url": "https://example.invalid/" + target,
                    "bytes": len(value),
                    "sha256": digest,
                    "mode": mode,
                })
            staging = root / "identity"
            author_tree.copy_inputs({"sources": sources}, cache, staging, True)
            _, manifest_digest = author_tree.manifest(staging, "content")
            config = {
                "category": "development/ryeos",
                "name": "fixture",
                "version": "1.0.0",
                "description": "fixture",
                "schema": author_tree.SCHEMA,
                "source_date_epoch": 1_700_000_000,
                "artifact": {
                    "release_tag": "fixture-v1",
                    "archive": "fixture.tar.gz",
                    "prefix": "fixture",
                    "storage": "content",
                    "manifest_digest": manifest_digest,
                    "entries": 3,
                    "total_bytes": len(first) + len(second),
                },
                "sources": sources,
            }
            config_path = root / "config.yaml"
            config_path.write_text(yaml.safe_dump(config, sort_keys=False))
            receipt = author_tree.author(config_path, cache, output, True)
            self.assertEqual(receipt["manifest_digest"], manifest_digest)
            with tarfile.open(output / "fixture.tar.gz", "r:gz") as archive:
                self.assertEqual(
                    [member.name for member in archive],
                    ["fixture", "fixture/a", "fixture/nested", "fixture/nested/b"],
                )
            moved = cache / f"{sources[0]['sha256']}-a"
            moved.write_bytes(b"other")
            with self.assertRaisesRegex(ValueError, "wrong identity"):
                author_tree.author(config_path, cache, root / "other-output", True)

    def test_authoring_has_no_node_or_publication_authority(self) -> None:
        source = AUTHOR_PATH.read_text()
        for forbidden in ("subprocess", "ryeos ", "gh release", "private_key", "external-content bind"):
            self.assertNotIn(forbidden, source)
        self.assertIn("urllib.request", source)
        self.assertIn("authored tree differs from its signed RyeOS identity", source)

    def test_activation_and_projectless_verifier_bind_the_authored_tree(self) -> None:
        acquisition = author_tree.load_config(GNU_ACQUISITION)
        activation = yaml.safe_load(GNU_ACTIVATION.read_text())
        artifact = acquisition["artifact"]
        self.assertEqual(
            activation["consumer_ref"],
            "tool:ryeos/environments/qualification/gnu-python-archives",
        )
        self.assertEqual(activation["components"], [{
            "id": "gnu-python-archives",
            "storage": "large_content",
            "shape": {
                "kind": "whole_archive_tree",
                "source": "gnu-python-archives-release",
                "prefix": artifact["prefix"],
                "bounds": {
                    "maximum_entries": artifact["entries"],
                    "maximum_depth": 1,
                    "maximum_file_bytes": max(source["bytes"] for source in acquisition["sources"]),
                    "maximum_total_bytes": artifact["total_bytes"],
                },
            },
        }])
        source = GNU_VERIFIER.read_text()
        self.assertIn(artifact["manifest_digest"], source)
        for item in acquisition["sources"]:
            self.assertIn(item["target"], source)
            self.assertIn(item["sha256"], source)
        self.assertNotIn("subprocess", source)

    def test_authoring_source_activation_is_raw_input_not_prepared_product(self) -> None:
        activation = yaml.safe_load(SOURCE_ACTIVATION.read_text())
        self.assertEqual(
            activation["consumer_ref"],
            "tool:ryeos/environments/qualification/authoring-source-inputs",
        )
        component = activation["components"][0]
        self.assertEqual(component["id"], "authoring-source-inputs")
        self.assertEqual(component["storage"], "large_content")
        self.assertEqual(component["shape"]["prefix"], "ryeos-authoring-source-inputs-v2")
        source = SOURCE_VERIFIER.read_text()
        self.assertIn("c66ac1c984e0793416106cd2fefa1a45d1b00c17b5865ff751e41594a3d39857", source)
        self.assertIn("external_large_content_manifest", source)
        self.assertNotIn("cc090b3d53dd41c0", source)
        self.assertNotIn("subprocess", source)

    def test_authoring_source_publisher_matches_the_admitted_input_contract(self) -> None:
        acquisition = yaml.safe_load(SOURCE_ACQUISITION.read_text())
        contract = yaml.safe_load(AUTHORING_INPUTS.read_text())
        expected = {
            item["name"]: {"bytes": item["bytes"], "sha256": item["sha256"]}
            for item in acquisition["bootstrap_archives"]
        }
        self.assertEqual(expected, contract["provenance"]["utility_bootstrap_archives"])
        self.assertEqual(
            acquisition["workload_package"]["sha256"],
            contract["provenance"]["workload_package_sha256"],
        )
        self.assertEqual(
            acquisition["elf_authoring_package"]["sha256"],
            contract["provenance"]["elf_authoring_package_sha256"],
        )
        source = SOURCE_AUTHOR_PATH.read_text()
        self.assertIn("--network=none", source)
        self.assertIn("--pull=never", source)
        for forbidden in ("ryeos ", "private_key", "external-content bind", "gh release"):
            self.assertNotIn(forbidden, source)


if __name__ == "__main__":
    unittest.main()
