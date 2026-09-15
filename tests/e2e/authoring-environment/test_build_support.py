"""Finite producer checks; synthetic ELF data is not execution qualification."""

import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / ".ai/tools/ryeos/development/authoring-environment-production/lib"))
import build_support as owner
import production


class ElfFixture:
    def __init__(self, corrupt=False):
        self.changed = set()
        self.corrupt = corrupt

    def facts(self, path):
        dynamic = path.name in {"sh", "make"}
        fixed = path in self.changed
        return {"dynamic": dynamic, "needed": ["libc.so.6"] if dynamic else [],
                "interpreter": ([str(owner.SUPPORT) + "/lib/ld-linux-x86-64.so.2" if fixed
                                 else "/lib64/ld-linux-x86-64.so.2"] if dynamic else []),
                "runpath": [str(owner.SUPPORT) + "/lib"] if fixed else [],
                "rpath": False, "nodeflib": fixed}

    def symbols(self, path):
        return ["corrupted"] if self.corrupt and path in self.changed else []

    def run(self, name, *args):
        assert name == "patchelf"
        option = "--set-interpreter" if "--set-interpreter" in args else "--set-rpath"
        assert args[args.index(option) + 1].startswith(str(owner.SUPPORT) + "/lib")
        path = Path(args[-1])
        path.write_bytes(path.read_bytes() + b"relocated")
        self.changed.add(path)


class BuildSupportTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="ryeos-build-support-test-")
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.inputs = self.root / "inputs"
        self.inputs.mkdir()
        files = {f"bin/{name}": f"image/bin/{name}" for name in owner.REQUIRED_SUPPORT_COMMANDS}
        for member in (*production.ELF_TOOLS.values(), "lib/ld-linux-x86-64.so.2", "lib/libc.so.6",
                       "licenses/GPL-2", "licenses/GPL-3", "licenses/LGPL-2.1"):
            files[member] = member if member.startswith("elf/") else "image/" + member
        for destination, source in files.items():
            path = self.inputs / source
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"\x7fELFsynthetic")
            path.chmod(0o755 if destination.startswith("bin/") or destination in production.ELF_TOOLS.values()
                       or destination == "lib/ld-linux-x86-64.so.2" else 0o644)
        self.config = {"category": "development/ryeos", "name": "authoring-build-support-inputs",
                       "version": "1.0.0", "schema": owner.SCHEMA, "source_date_epoch": 123,
                       "inputs": production.input_inventory(self.inputs), "files": files,
                       "provenance": {"test": "synthetic fixture, not an artifact"}}

    def test_assembly_uses_shared_relocation_and_existing_utility_contract(self):
        result = owner.assemble_support(self.inputs, self.root / "first", self.config, tools=ElfFixture())
        contract = result["support_contract"]
        self.assertEqual(set(contract), {"inputs", "commands", "notices"})
        self.assertEqual(contract["inputs"]["lib/libc.so.6"]["mode"], 0o644)
        provenance = json.loads((self.root / "first/RYEOS-BUILD-SUPPORT.json").read_text())
        self.assertEqual(set(provenance["transformations"]), {"bin/sh", "bin/make"})
        self.assertEqual(provenance["runtime_mount"], str(owner.SUPPORT))
        second = owner.assemble_support(self.inputs, self.root / "second", self.config, tools=ElfFixture())
        self.assertEqual(result, second)

    def test_missing_support_or_notices_and_extra_input_fail_before_output(self):
        for missing in ("bin/sh", "bin/make", "licenses/GPL-3"):
            config = copy.deepcopy(self.config)
            del config["files"][missing]
            with self.assertRaises(ValueError):
                owner.assemble_support(self.inputs, self.root / "out", config, tools=ElfFixture())
            self.assertFalse((self.root / "out").exists())
        (self.inputs / "undeclared").write_bytes(b"extra")
        with self.assertRaisesRegex(ValueError, "exact inventory"):
            owner.assemble_support(self.inputs, self.root / "out", self.config, tools=ElfFixture())

    def test_input_links_refuse_and_completed_output_is_never_overwritten(self):
        owner.assemble_support(self.inputs, self.root / "out", self.config, tools=ElfFixture())
        before = production.inventory(self.root / "out")
        with self.assertRaisesRegex(ValueError, "already exists"):
            owner.assemble_support(self.inputs, self.root / "out", self.config, tools=ElfFixture())
        self.assertEqual(production.inventory(self.root / "out"), before)
        (self.inputs / "link").symlink_to(self.inputs / "image/bin/sh")
        with self.assertRaises(ValueError):
            owner.assemble_support(self.inputs, self.root / "other", self.config, tools=ElfFixture())

    def test_symbol_change_or_absent_runtime_dependency_cannot_produce_receipt(self):
        with self.assertRaisesRegex(ValueError, "symbol ownership"):
            owner.assemble_support(self.inputs, self.root / "bad", self.config, tools=ElfFixture(corrupt=True))
        self.assertFalse((self.root / "bad/RYEOS-BUILD-SUPPORT.json").exists())
        source = self.config["files"].pop("lib/libc.so.6")
        del self.config["inputs"][source]
        (self.inputs / source).unlink()
        with self.assertRaises(FileNotFoundError):
            owner.assemble_support(self.inputs, self.root / "missing-lib", self.config, tools=ElfFixture())

    def test_output_collisions_and_broad_locations_are_not_selection(self):
        for target in ("../outside", "/usr/bin/sh", "bin/sh/child", "host/new"):
            config = copy.deepcopy(self.config)
            config["files"][target] = "image/bin/sh"
            with self.assertRaises(ValueError):
                owner.validate_config(config)

    def test_authored_input_contract_is_finite_and_covers_required_helpers(self):
        text = (ROOT / ".ai/config/development/ryeos/authoring-build-support-inputs.yaml").read_text()
        config = json.loads("\n".join(line for line in text.splitlines() if not line.startswith("#")))
        owner.validate_config(config)
        self.assertLess(sum(v["bytes"] for v in config["inputs"].values()), 22_000_000)

    def test_qualification_fixture_is_not_a_production_or_worker_granted_tool(self):
        import yaml
        fixture = (ROOT / "tests/e2e/authoring-environment/build_support_probe.py").read_text()
        header = yaml.safe_load("\n".join(line[2:] for line in fixture.splitlines()
                                        if line.startswith("# ")))["ryeos-tool"]
        self.assertEqual(header["filesystem_authority"], "captured_execution")
        self.assertEqual(header["network_authority"], "isolated")
        self.assertEqual(header["workspace_access"], "immutable_current_generation")
        self.assertEqual({entry["id"] for entry in header["external_content"]},
                         {"producer-python", "authoring-build-support", "platform"})
        for entry in header["external_content"]:
            self.assertEqual(entry["mode"], "pinned")
            self.assertEqual(entry["mount_root"], "execution_runtime")
        self.assertFalse((ROOT / ".ai/tools/ryeos/development/authoring-environment-production/"
                          "build-support-probe.py").exists())
        compile(fixture, "build_support_probe.py", "exec")
        self.assertIn('"fresh_utility_build": False', fixture)
        self.assertIn('work = Path("/tmp/build-support-probe")', fixture)
        self.assertNotIn('work = products /', fixture)

    def test_support_selection_matches_current_observed_producer_contract(self):
        import hashlib
        import yaml
        config = yaml.safe_load((ROOT / ".ai/config/development/ryeos/"
                                 "authoring-build-support.yaml").read_text())
        inputs = yaml.safe_load((ROOT / ".ai/config/development/ryeos/"
                                 "authoring-build-support-inputs.yaml").read_text())
        evidence = json.loads((ROOT / "tests/e2e/authoring-environment/"
                               "build-support-qualification.json").read_text())
        support = config["support"]
        assembly = evidence["current_support_contract"]
        self.assertEqual(hashlib.sha256(production.canonical_json(support["inputs"])).hexdigest(),
                         assembly["inventory_sha256"])
        self.assertEqual(hashlib.sha256(production.canonical_json(inputs)).hexdigest(),
                         assembly["input_contract_sha256"])
        self.assertEqual(len(support["inputs"]), assembly["files"])
        self.assertEqual(sum(v["bytes"] for v in support["inputs"].values()),
                         assembly["bytes"])
        self.assertRegex(assembly["product_witness_hash"], r"^[0-9a-f]{64}$")
        self.assertRegex(assembly["product_manifest_hash"], r"^[0-9a-f]{64}$")
        self.assertIn("tee", support["commands"])
        # Preserve the earlier full build proof as historical evidence. The
        # current record refreshes only the exact support output contract.
        self.assertEqual(evidence["support_with_tee"]["assembly"]["inventory_sha256"],
                         "e160361010e018c512ae7559e937c5bc3ea7594e92b5ae2beae84ff9556b4ec6")
        self.assertTrue(evidence["gates"]["fresh_utility_build"])
        self.assertTrue(evidence["support_with_tee"]["fresh_utility_build_pass"]["git_runtime_shell_verified"])
        self.assertFalse(evidence["gates"]["worker_driven_development"])

    def test_fresh_build_fixture_calls_existing_recipe_with_exact_offline_inputs(self):
        import yaml
        text = (ROOT / "tests/e2e/authoring-environment/utility_build_probe.py").read_text()
        header = text.split("\n\n", 1)[0]
        contract = yaml.safe_load("\n".join(line[2:] for line in header.splitlines()))["ryeos-tool"]
        self.assertEqual(contract["network_authority"], "isolated")
        self.assertEqual(contract["filesystem_authority"], "captured_execution")
        self.assertEqual(contract["workspace_access"], "immutable_current_generation")
        self.assertEqual({entry["id"] for entry in contract["external_content"]},
                         {"producer-python", "authoring-build-support", "platform", "source-inputs", "authoring-tools"})
        tool = (ROOT / ".ai/tools/ryeos/development/authoring-environment-production/build-utilities.py").read_text()
        authored = tool[tool.index("# ryeos-tool:"):].split("\n\n", 1)[0]
        tool_header = yaml.safe_load("\n".join(line[2:] for line in authored.splitlines()))["ryeos-tool"]
        for field in ("executor_id", "execution_protocol", "network_authority",
                      "filesystem_authority", "workspace_access", "config_schema", "config_resolve"):
            self.assertEqual(tool_header[field], contract[field])
        self.assertEqual(tool_header["external_content"],
                         [entry for entry in contract["external_content"]
                          if entry["id"] not in {"authoring-tools", "authoring-build-support"}])
        self.assertIn("authoring-build-support is selected by the enclosing producer Graph", tool)
        self.assertIn("from utility_production import main", tool)
        self.assertNotIn("build_utilities(", tool)
        execution = yaml.safe_load((ROOT / ".ai/config/execution/execution.yaml").read_text())
        self.assertEqual(execution["items"]["tool"][
            "ryeos/development/authoring-environment-production/build-utilities"]["timeout"], 1800)
        entry = (ROOT / ".ai/tools/ryeos/development/authoring-environment-production/lib/utility_production.py").read_text()
        self.assertIn('WORK = Path("/tmp/utility-build")', entry)
        self.assertIn('result = build_utilities(', entry)
        self.assertIn('result = produce(project, resolved)', text)
        self.assertNotIn('read_members(', text)
        self.assertNotIn("subprocess", text)
        self.assertNotIn("--setrlimit", text)
        compile(text, "utility_build_probe.py", "exec")


if __name__ == "__main__":
    unittest.main()
