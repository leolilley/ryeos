#!/usr/bin/env python3
"""Exercise release builders' feature selections without running Cargo."""
import hashlib
import importlib.machinery
import io
import json
import os
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
ASSET = ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib"
NATIVE = ASSET / "native-build.py"
CORE = ASSET / "core-seed-build.py"
TRIPLE = "x86_64-unknown-linux-gnu"
PACKAGE = "ryeos-handler-bins"


def ownership(bundle, binaries):
    if isinstance(binaries, str):
        binaries = [binaries]
    return {"schema": "ryeos.bundle_payload_ownership.v1",
            "kind": "bundle_payload_ownership",
            "bundles": [{"bundle_name": bundle, "bundle_sets": ["standard"],
                         "payloads": [{"binary": binary, "cargo_package": PACKAGE,
                                       "build_class": "release"}
                                      for binary in sorted(binaries)]}]}


def resolved(bundle, binaries):
    contract = ownership(bundle, binaries)
    return ({"value": {"category": "bundle-release", "version": "1.0.0",
                        "description": "fixture", "payload_ownership": contract},
             "source": {"bundle_name": "bundle-release",
                        "config_path": "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml",
                        "signer_fingerprint": "a" * 64}}, contract)


def required_features(binary):
    return ["basic-parsers"] if binary.startswith("rye-parser-") else ["full-handlers"]


def release_request(bundle, binaries, *, mutate=None, package=PACKAGE):
    if isinstance(binaries, str):
        binaries = [binaries]
    resolution, contract = resolved(bundle, binaries)
    digest = hashlib.sha256(json.dumps(contract, sort_keys=True, separators=(",", ":"),
                                       ensure_ascii=True).encode()).hexdigest()
    payloads = [{"bundle": bundle, "binary": binary, "cargo_package": package,
                 "build_class": "release", "bundle_sets": ["standard"],
                 "required_features": required_features(binary)}
                for binary in sorted(binaries)]
    features = sorted({feature for payload in payloads
                       for feature in payload["required_features"]})
    if "full-handlers" in features:
        features.remove("basic-parsers") if "basic-parsers" in features else None
    value = {"schema": "ryeos.bundle_release_input_plan.v2", "project_path": "/source",
             "bundle_name": bundle,
             "authored_manifest": {"name": bundle, "version": "1.0.0"},
             "source_snapshot_hash": "b" * 64, "predecessor_generation_hash": None,
             "target": {"kind": "triple", "triple": TRIPLE}, "build_profile": "release",
             "payload_ownership_item_ref": "config:bundle-release/payload-ownership",
             "payload_ownership_content_hash": digest, "payloads": payloads,
             "cargo_packages": [PACKAGE],
             "cargo_feature_selections": [{"cargo_package": PACKAGE,
                                           "features": features}],
             "build_classes": ["release"], "requires_binary_build": True,
             "clean_output_required": True, "ambient_target_reuse_allowed": False}
    if mutate:
        mutate(value, payloads)
    return {"release_input": value, "resolved_config": resolution}


class FakeLoader:
    def __init__(self, name):
        self.name = name

    def create_module(self, spec):
        return None

    def exec_module(self, module):
        if self.name == "release_cargo":
            module.PLATFORM = "fixture"
            module.validate_source_configuration = lambda root: None
            module.build_environment = lambda *args: {"RUSTFLAGS": ""}
        else:
            module.normalize_output_elf = lambda *args: {"normalized": True}


def fake_spec(name, location, **kwargs):
    return importlib.machinery.ModuleSpec(name, FakeLoader(name))


def fake_cargo_run(argv, *, cwd, env, check):
    assert check is True
    target = argv[argv.index("--target") + 1]
    binaries = [argv[i + 1] for i, token in enumerate(argv[:-1]) if token == "--bin"]
    for binary in binaries:
        output = Path(env["CARGO_TARGET_DIR"]) / target / "release" / binary
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(b"fixture binary")


def setup_project(root, bundle, *, signed=False):
    metadata = root / "bundles" / bundle / ".ai"
    metadata.mkdir(parents=True)
    manifest = {"name": bundle, "version": "1.0.0"}
    if signed:
        body = json.dumps(manifest, sort_keys=True, separators=(",", ":"))
        (metadata / "manifest.yaml").write_text("# ryeos:signed:fixture\n" + body + "\n")
    else:
        (metadata / "manifest.source.yaml").write_text(json.dumps(manifest))


class HandlerFeatureBuildContractTests(unittest.TestCase):
    def invoke(self, script, request):
        stdout = io.StringIO()
        with patch("sys.stdin", io.StringIO(json.dumps(request))), \
             patch("sys.stdout", stdout), \
             patch("importlib.util.spec_from_file_location", side_effect=fake_spec), \
             patch("resource.setrlimit"), \
             patch("subprocess.run", side_effect=fake_cargo_run) as cargo:
            runpy.run_path(str(script), run_name="__main__")
        return json.loads(stdout.getvalue()), cargo

    def test_native_basic_and_full_selections_reach_cargo_and_evidence(self):
        for binary in ("rye-parser-regex-kv", "ryeos-directive-launch-preparer"):
            with self.subTest(binary=binary), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                setup_project(root, "demo")
                old = Path.cwd()
                try:
                    os.chdir(root)
                    output, cargo = self.invoke(NATIVE, release_request("demo", binary))
                finally:
                    os.chdir(old)
                self.assertEqual(cargo.call_count, 1)
                argv = cargo.call_args.args[0]
                self.assertIn("--no-default-features", argv)
                self.assertEqual(argv[argv.index("--features") + 1], ",".join(required_features(binary)))
                self.assertEqual(argv[-2:], ["--bin", binary])
                self.assertEqual(output["processes"][0]["argv"], argv)
                self.assertTrue((root / "products/native-bundle/tree/.ai/bin" / TRIPLE / binary).is_file())

    def test_core_seed_uses_full_handler_selection(self):
        binaries = ["rye-parser-yaml-document", "ryeos-direct-execution-evidence"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            setup_project(root, "core", signed=True)
            old = Path.cwd()
            try:
                os.chdir(root)
                output, cargo = self.invoke(CORE, release_request("core", binaries))
            finally:
                os.chdir(old)
            argv = cargo.call_args.args[0]
            self.assertIn("--no-default-features", argv)
            self.assertEqual(argv[argv.index("--features") + 1], "full-handlers")
            self.assertEqual(output["processes"][0]["argv"], argv)
            self.assertEqual([argv[index + 1] for index, token in enumerate(argv[:-1])
                              if token == "--bin"], sorted(binaries))

    def test_stale_selection_is_rejected_before_cargo(self):
        mutations = (
            lambda value, payloads: value.update(cargo_feature_selections=[]),
            lambda value, payloads: value.update(cargo_feature_selections=[
                {"cargo_package": PACKAGE, "features": ["basic-parsers", "full-handlers"]}]),
            lambda value, payloads: value.update(cargo_feature_selections=[
                {"cargo_package": PACKAGE, "features": ["basic-parsers"]},
                {"cargo_package": PACKAGE, "features": ["basic-parsers"]}]),
            lambda value, payloads: value.update(cargo_feature_selections=[
                {"cargo_package": "ryeos-wrong-package", "features": ["basic-parsers"]}]),
            lambda value, payloads: value.update(cargo_feature_selections=[
                {"cargo_package": PACKAGE, "features": ["basic-parsers", "unlisted"]}]),
            lambda value, payloads: payloads[0].update(required_features=["full-handlers"]),
        )
        for script, bundle, binary in (
                (NATIVE, "demo", "rye-parser-regex-kv"),
                (CORE, "core", "rye-parser-yaml-document")):
            for mutation in mutations:
                with self.subTest(script=script.name, mutation=mutation), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    setup_project(root, bundle, signed=bundle == "core")
                    old = Path.cwd()
                    try:
                        os.chdir(root)
                        with patch("sys.stdin", io.StringIO(json.dumps(
                                release_request(bundle, binary, mutate=mutation)))), \
                             patch("importlib.util.spec_from_file_location", side_effect=fake_spec), \
                             patch("resource.setrlimit"), \
                             patch("subprocess.run") as cargo:
                            with self.assertRaises((ValueError, SystemExit)):
                                runpy.run_path(str(script), run_name="__main__")
                        cargo.assert_not_called()
                    finally:
                        os.chdir(old)


if __name__ == "__main__":
    unittest.main()
