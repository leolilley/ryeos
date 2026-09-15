"""Focused source checks; installed Tool execution is separate evidence."""

import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT / ".ai/tools/ryeos/development/repository-validation"
SPEC = importlib.util.spec_from_file_location("validation", SOURCE / "lib/validation.py")
owner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(owner)


def config():
    raw = (ROOT / ".ai/config/development/ryeos/repository-validation.yaml").read_text()
    return json.loads(raw.split("\n", 1)[1] if raw.startswith("# ryeos:signed:") else raw)


class ValidationTests(unittest.TestCase):
    def setUp(self):
        self.config = config()

    def test_evidence_allowlist_does_not_admit_builds_or_private_state(self):
        allowed = ["development-cargo/README.md", "development-cargo/qualification.json",
                   "repository-validation/README.md", "repository-validation/qualification.json"]
        ignored = ["development-cargo/target/output", "development-cargo/.ai/state/private",
                   "repository-validation/output.bin", "repository-validation/.ai/state/private"]
        for expected, paths in ((False, allowed), (True, ignored)):
            for path in paths:
                result = subprocess.run(["git", "check-ignore", "--no-index", "-q", "--",
                                         f"tests/e2e/{path}"], cwd=ROOT, check=False)
                self.assertIn(result.returncode, (0, 1))
                self.assertEqual(result.returncode == 0, expected, path)

    def test_authored_rules_are_complete_and_use_current_crate_layout(self):
        owner.validate_config(self.config)
        self.assertIn("crates", self.config["text_checks"]["naming"][0]["roots"])
        self.assertNotIn("docs", self.config["text_checks"]["naming"][0]["roots"])

    def test_missing_or_empty_configuration_never_becomes_success(self):
        for key in self.config:
            broken = copy.deepcopy(self.config)
            del broken[key]
            with self.assertRaises(ValueError):
                owner.validate_config(broken)
        self.config["limits"]["max_files"] = True
        with self.assertRaises(ValueError):
            owner.validate_config(self.config)

    def test_real_repository_dependency_layers(self):
        failures, count = owner.dependency_layers(ROOT, self.config)
        self.assertEqual(failures, [])
        self.assertGreater(count, 30)

    def test_dependency_aliases_build_targets_and_cycles(self):
        data = {"dependencies": {"alias": {"package": "b"}},
                "target": {"cfg(unix)": {"build-dependencies": {"c": "1"}}},
                "dev-dependencies": {"d": "1"}}
        self.assertEqual(owner.dependencies(data, {"b", "c", "d"}, {}), {"b", "c"})
        self.assertEqual(owner.find_cycle({"a": {"b"}, "b": {"a"}}), ["a", "b", "a"])
        self.assertIsNone(owner.find_cycle({"a": {"b"}, "b": set()}))

    def test_inherited_alias_uses_cargo_workspace_package_identity(self):
        data = {"target": {"cfg(unix)": {"build-dependencies": {"alias": {"workspace": True}}}}}
        self.assertEqual(owner.dependencies(data, {"forbidden"}, {
            "alias": {"package": "forbidden", "path": "crates/forbidden"}
        }), {"forbidden"})
        with self.assertRaisesRegex(ValueError, "inheritance"):
            owner.dependencies(data, {"forbidden"}, {})

    def test_exact_forbidden_dependency(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text('[workspace]\nmembers=["a", "b"]\n')
            for name in ("a", "b"):
                (root / name).mkdir()
                (root / name / "Cargo.toml").write_text(f'[package]\nname="{name}"\n')
            self.config["dependency_layers"]["forbidden_edges"] = {"a": ["b"]}
            self.assertEqual(owner.dependency_layers(root, self.config), ([], 2))
            (root / "a/Cargo.toml").write_text('[package]\nname="a"\n[build-dependencies]\nb="1"\n')
            self.assertEqual(owner.dependency_layers(root, self.config)[0], ["forbidden dependency: a -> b"])

    def test_leading_dash_literal_is_checked_and_diagnostic_is_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rule = self.config["text_checks"]["naming"][0]
            rule["roots"] = ["sample.md"]
            value = rule["patterns"][0]
            (root / "sample.md").write_text("ordinary text\n")
            self.assertEqual(owner.text_check(root, self.config, "naming"), ([], 1))
            (root / "sample.md").write_text(f"example {value} with private unrelated text\n")
            failures, count = owner.text_check(root, self.config, "naming")
            self.assertEqual(count, 1)
            self.assertEqual(failures, ["sample.md:1: forbidden text"])

    def test_missing_root_symlink_and_oversized_input_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rule = self.config["text_checks"]["naming"][0]
            rule["roots"] = ["sample.md"]
            with self.assertRaises(FileNotFoundError):
                owner.text_check(root, self.config, "naming")
            (root / "sample.md").symlink_to(ROOT / "README.md")
            with self.assertRaisesRegex(ValueError, "symlink"):
                owner.text_check(root, self.config, "naming")
            (root / "sample.md").unlink()
            (root / "sample.md").write_text("too long")
            self.config["limits"]["max_file_bytes"] = 1
            with self.assertRaisesRegex(ValueError, "bound"):
                owner.text_check(root, self.config, "naming")

    def test_empty_or_excessive_inventory_is_not_success(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rule = self.config["text_checks"]["naming"][0]
            rule["roots"] = ["source"]
            (root / "source").mkdir()
            with self.assertRaisesRegex(ValueError, "empty"):
                owner.text_check(root, self.config, "naming")
            (root / "source/a.md").write_text("fine")
            self.config["limits"]["max_files"] = 1
            with self.assertRaisesRegex(ValueError, "bound"):
                owner.text_check(root, self.config, "naming")

    def test_only_configured_exclusions_skip_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "source").mkdir()
            (root / "source/a.md").write_text("okay")
            rule = self.config["text_checks"]["naming"][0]
            (root / "source/b.md").write_text(rule["patterns"][0])
            rule["roots"] = ["source"]
            rule["exclude"] = ["source/b.md"]
            self.assertEqual(owner.text_check(root, self.config, "naming"), ([], 1))

    def test_entries_reuse_existing_python_without_new_worker_grants(self):
        import yaml
        worker = yaml.safe_load((ROOT / ".ai/config/development/ryeos/worker-environment.yaml").read_text())
        for name in ("dependency-layers", "naming", "no-content-wrap", "cli-presentation"):
            source = (SOURCE / (name + ".py")).read_text()
            header = "\n".join(line[2:] for line in source.splitlines()
                               if line.startswith("# ") and not line.startswith("# ryeos:signed:"))
            tool = yaml.safe_load(header.split("\nReuse the", 1)[0])["ryeos-tool"]
            self.assertEqual(tool["executor_id"], "tool:ryeos/development/authoring-environment-production/runtime")
            self.assertEqual(tool["filesystem_authority"], "captured_execution")
            self.assertEqual(tool["network_authority"], "isolated")
            self.assertEqual(tool["external_content"][0]["digest"],
                             "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf")
            self.assertFalse(tool["config_schema"]["additionalProperties"])
        self.assertFalse(any("repository-validation" in route["item_ref"]
                             for route in worker["workload_client"]["executions"]))


if __name__ == "__main__":
    unittest.main()
