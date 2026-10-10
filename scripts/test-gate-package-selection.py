#!/usr/bin/env python3
"""Check canonical gate scope using a fake Cargo; never build or sign bundles."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class GatePackageSelection(unittest.TestCase):
    def invoke(self, *args):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "argv.json"
            cargo = root / "cargo"
            cargo.write_text(
                "#!/usr/bin/env python3\n"
                "import json,os,sys\n"
                "with open(os.environ['GATE_ARGV_LOG'],'w') as out:\n"
                " json.dump(sys.argv[1:],out)\n"
            )
            cargo.chmod(0o700)
            environment = dict(os.environ, CARGO=str(cargo), GATE_ARGV_LOG=str(log),
                               GATE_BUILD_JOBS="1", GATE_TEST_THREADS="1")
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/gate.sh"), *args],
                env=environment, capture_output=True, text=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            return json.loads(log.read_text())

    def test_default_keeps_complete_workspace(self):
        args = self.invoke()
        self.assertIn("--workspace", args)
        self.assertIn("ryeos-handler-bins/graph-handlers", args[args.index("--features") + 1])

    def test_native_package_never_widens_to_workspace_or_product_features(self):
        for selection in [("-p", "lillux-sqlite"), ("--package=lillux-sqlite",),
                          ("-plillux-sqlite",)]:
            with self.subTest(selection=selection):
                args = self.invoke(*selection, "--locked", "--offline")
                self.assertNotIn("--workspace", args)
                self.assertNotIn("--features", args)
                self.assertIn("--locked", args)
                self.assertIn("--offline", args)

    def test_selected_scheduler_consumer_gets_only_its_composition_feature(self):
        for package in ["ryeosd", "ryeos-cli", "ryeos-api"]:
            with self.subTest(package=package):
                args = self.invoke("--bundle-set", "core-scheduling", "-p", package)
                self.assertNotIn("--workspace", args)
                self.assertEqual(args[args.index("--features") + 1], f"{package}/scheduling")

    def test_standard_selects_provider_setup_for_api_and_cli_consumers(self):
        for package in ["ryeosd", "ryeos-api", "ryeos-cli"]:
            with self.subTest(package=package):
                args = self.invoke("--bundle-set", "standard", "-p", package)
                features = f"{package}/scheduling"
                features += f",{package}/model-provider-setup"
                self.assertEqual(args[args.index("--features") + 1], features)
                self.assertNotIn("--workspace", args)

    def test_reduced_daemon_keeps_scheduler_absent(self):
        args = self.invoke("--bundle-set", "hosted-node", "-p", "ryeosd")
        self.assertNotIn("--workspace", args)
        self.assertNotIn("--features", args)

    def test_version_and_glob_preserve_cargo_selection_and_scoped_features(self):
        for selection in ["ryeos-api@0.1.0", "ryeos-api*"]:
            with self.subTest(selection=selection):
                args = self.invoke("--bundle-set", "core-scheduling", "-p", selection)
                self.assertNotIn("--workspace", args)
                self.assertIn(selection, args)
                self.assertEqual(args[args.index("--features") + 1], "ryeos-api/scheduling")

    def test_explicit_workspace_remains_explicit(self):
        args = self.invoke("--workspace", "-p", "lillux-sqlite")
        self.assertEqual(args.count("--workspace"), 1)
        self.assertIn("--features", args)


if __name__ == "__main__":
    unittest.main()
