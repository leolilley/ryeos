"""Focused source-level checks; these do not claim namespace/worker acceptance."""

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]
OWNER = ROOT / ".ai/tools/ryeos/development/authoring-environment-production"
sys.path.insert(0, str(OWNER / "lib"))
import utilities
SPEC = importlib.util.spec_from_file_location("authoring_production", OWNER / "lib/production.py")
production = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(production)


class FakeElfTools:
    """Deliberately synthetic readelf/patchelf responses, not an ELF execution."""

    def __init__(self, *, corrupt_symbols=False):
        self.changed = set()
        self.corrupt_symbols = corrupt_symbols
        self.calls = []

    def symbols(self, path):
        if self.corrupt_symbols and path in self.changed:
            return ([], [("moved",)])
        return ([], [])

    def facts(self, path):
        shell = path.name == "zsh"
        fixed = path in self.changed
        return {"dynamic": shell, "interpreter": [] if not shell else [
                    production.RUNTIME_ROOT + "/lib/ld-linux-x86-64.so.2" if fixed else "/lib64/ld-linux-x86-64.so.2"],
                "needed": [], "runpath": [production.RUNTIME_ROOT + "/lib"] if fixed else [],
                "nodeflib": fixed, "rpath": False}

    def run(self, name, *args):
        self.calls.append((name, args))
        path = Path(args[-1])
        path.write_bytes(path.read_bytes() + b"\nnormalized relocation")
        self.changed.add(path)
        return ""


class ProductionTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="ryeos-authoring-production-test-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.inputs = self.root / "inputs"
        self.inputs.mkdir()
        self.built_utilities = self.root / "built-utilities"
        self.built_utilities.mkdir()
        self.source_config = {"fixture": "exact utility source contract"}
        self.support_config = {"support": {"fixture": "exact build support contract"}}
        self.config = {
            "category": "development/ryeos", "name": "authoring-environment-inputs",
            "version": "1.0.0", "schema": production.SCHEMA, "source_date_epoch": 123,
            "inputs": {}, "files": {}, "built_utility_files": {},
            "relocate": ["environment/bin/zsh"],
            "provenance": {"test": "synthetic bytes; not an executable artifact"},
        }
        for command in sorted(production.REQUIRED_COMMANDS):
            target = f"environment/bin/{command}"
            self.add_file(f"utilities/{command}",
                          target if command not in production.BUILT_UTILITY_COMMANDS else None,
                          mode=0o755)
            if command in production.BUILT_UTILITY_COMMANDS:
                member = f"bin/{command}"
                self.add_built_file(member, mode=0o755)
                self.config["built_utility_files"][target] = member
        self.add_built_file("licenses/NOTICE")
        self.add_built_file("corresponding-sources/upstream.tar")
        self.add_built_file("source-contract.json", production.canonical_json(self.source_config))
        self.add_built_file("build-evidence.json", production.canonical_json(
            utilities.build_evidence(self.source_config, self.support_config["support"])))
        self.add_file("licenses/NOTICE", "environment/licenses/NOTICE")
        self.add_file("sources/upstream.tar", "corresponding-sources/upstream.tar")
        for name in production.ELF_TOOLS.values():
            self.add_file(name, mode=0o755)
        self.add_file("lib/ld-linux-x86-64.so.2", "environment/lib/ld-linux-x86-64.so.2", mode=0o755)

    def add_file(self, source, target=None, *, mode=0o644, data=b"synthetic fixture"):
        path = self.inputs / source
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(mode)
        self.config["inputs"][source] = {"sha256": production.sha256(path), "bytes": len(data), "mode": mode}
        if target:
            self.config["files"][target] = source

    def add_built_file(self, member, data=b"synthetic built utility", *, mode=0o644):
        path = self.built_utilities / member
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(mode)

    def assemble(self, name="output", **kwargs):
        return production.assemble(
            self.inputs, self.built_utilities, self.root / name, self.config,
            self.source_config, self.support_config, tools=FakeElfTools(**kwargs))

    def assemble_with_tools(self, name, tools):
        return production.assemble(
            self.inputs, self.built_utilities, self.root / name, self.config,
            self.source_config, self.support_config, tools=tools)

    def test_inventory_requires_the_exact_supported_commands(self):
        production.checked_inputs(self.inputs, self.config)
        self.assertEqual(len(production.REQUIRED_COMMANDS), 43)
        del self.config["built_utility_files"]["environment/bin/sed"]
        with self.assertRaisesRegex(ValueError, "exact finite command map"):
            production.validate_config(self.config)

    def test_repeat_production_has_equal_inventory_and_preserves_inputs(self):
        before = production.inventory(self.inputs)
        first = self.assemble("first")
        (self.inputs / "utilities/cat").touch()
        second = self.assemble("second")
        self.assertEqual(first, second)
        self.assertEqual(before, production.inventory(self.inputs))
        self.assertTrue((self.root / "first/corresponding-sources/upstream.tar").is_file())

    def test_corrupt_input_refuses_before_any_output_or_tool_execution(self):
        (self.inputs / "utilities/sed").write_bytes(b"drift")
        tools = FakeElfTools()
        with self.assertRaisesRegex(ValueError, "source bytes or modes"):
            self.assemble_with_tools("out", tools)
        self.assertFalse((self.root / "out").exists())
        self.assertEqual(tools.calls, [])

    def test_readonly_inputs_match_portable_identity_without_changing_storage_modes(self):
        for name, identity in self.config["inputs"].items():
            (self.inputs / name).chmod(identity["mode"] & ~0o222)
        before = production.inventory(self.inputs)
        self.assertEqual(before["licenses/NOTICE"]["mode"], 0o444)
        self.assertEqual(before["utilities/cat"]["mode"], 0o555)
        self.assertEqual(production.input_inventory(self.inputs), self.config["inputs"])
        self.assemble()
        self.assertEqual(production.inventory(self.inputs), before)
        outputs = production.inventory(self.root / "output")
        self.assertEqual(outputs["environment/licenses/NOTICE"]["mode"], 0o644)
        self.assertEqual(outputs["environment/bin/cat"]["mode"], 0o755)

    def test_portable_input_modes_preserve_executable_class_and_reject_special_files(self):
        for mode, expected in ((0o444, 0o644), (0o600, 0o644),
                               (0o555, 0o755), (0o700, 0o755), (0o641, 0o755)):
            self.assertEqual(production.portable_regular_mode(stat.S_IFREG | mode), expected)
        for kind in (stat.S_IFDIR, stat.S_IFLNK, stat.S_IFIFO, stat.S_IFSOCK):
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "regular file"):
                production.portable_regular_mode(kind | 0o755)
        (self.inputs / "licenses/NOTICE").chmod(0o555)
        with self.assertRaisesRegex(ValueError, "source bytes or modes"):
            production.checked_inputs(self.inputs, self.config)

    def test_special_input_refuses_without_reading_it(self):
        path = self.inputs / "licenses/NOTICE"
        path.unlink()
        os.mkfifo(path)
        with self.assertRaisesRegex(ValueError, "link or special"):
            production.checked_inputs(self.inputs, self.config)

    def test_receipts_still_detect_output_write_bit_changes(self):
        receipt = self.assemble()
        output = self.root / "output/environment/licenses/NOTICE"
        output.chmod(0o444)
        self.assertNotEqual(production.receipt(self.root / "output"), receipt)

    def test_extra_input_and_mode_change_are_identity_changes(self):
        self.add_file("extra")
        self.config["inputs"].pop("extra")
        with self.assertRaises(ValueError):
            production.checked_inputs(self.inputs, self.config)
        (self.inputs / "extra").unlink()
        (self.inputs / "utilities/cat").chmod(0o644)
        with self.assertRaises(ValueError):
            production.checked_inputs(self.inputs, self.config)

    def test_no_output_overwrite(self):
        self.assemble()
        before = production.inventory(self.root / "output")
        with self.assertRaisesRegex(ValueError, "already exists"):
            self.assemble()
        self.assertEqual(before, production.inventory(self.root / "output"))

    def test_symbol_changes_fail_before_inventory_success(self):
        with self.assertRaisesRegex(ValueError, "symbol ownership"):
            self.assemble(corrupt_symbols=True)
        self.assertFalse((self.root / "output/inventory.json").exists())

    def test_relocation_sets_search_policy_before_the_final_interpreter(self):
        executable = self.inputs / "utilities/zsh"
        tools = FakeElfTools()
        production.relocate_elf(executable, tools, production.RUNTIME_ROOT)
        patchelf_options = [
            "--set-rpath" if "--set-rpath" in arguments else "--set-interpreter"
            for name, arguments in tools.calls if name == "patchelf"
        ]
        self.assertEqual(patchelf_options, ["--set-rpath", "--set-interpreter"])
        self.assertEqual(tools.facts(executable)["interpreter"], [
            production.RUNTIME_ROOT + "/lib/ld-linux-x86-64.so.2",
        ])

    def test_unclosed_interpreter_or_dependency_refuses(self):
        self.assemble()
        class BadTools(FakeElfTools):
            def facts(self, path):
                result = super().facts(path)
                if path.name == "cat":
                    result["interpreter"] = ["/usr/lib/host-loader"]
                return result
        with self.assertRaisesRegex(ValueError, "unclosed interpreter"):
            production.check_closure(self.root / "output/environment",
                                     {**self.config["files"], **self.config["built_utility_files"]},
                                     BadTools(), set(), runtime_root=production.RUNTIME_ROOT)

    def test_selected_file_links_and_ancestor_links_are_refused(self):
        selected = self.inputs / "utilities/cat"
        selected.unlink()
        selected.symlink_to("sed")
        with self.assertRaises(ValueError):
            production.checked_inputs(self.inputs, self.config)
        with self.assertRaises(ValueError):
            production.ordinary_member(self.inputs, "utilities/cat")
        self.inputs.rename(self.root / "retained")
        self.inputs.symlink_to(self.root / "retained", target_is_directory=True)
        with self.assertRaises(ValueError):
            production.inventory(self.inputs)

    def test_paths_do_not_normalize_or_escape(self):
        for path in ("/etc/passwd", "../file", "a//b", "a/./b", "a/../b", "a/", "", "a:b"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                production.relative(path)

    def test_unknown_config_fields_and_arbitrary_output_layout_refuse(self):
        changed = copy.deepcopy(self.config)
        changed["command"] = "/bin/sh"
        with self.assertRaises(ValueError):
            production.validate_config(changed)
        self.config["files"]["../../escape"] = "utilities/cat"
        with self.assertRaises(ValueError):
            production.validate_config(self.config)

    def test_built_product_metadata_and_selected_bytes_are_required(self):
        (self.built_utilities / "bin/sed").write_bytes(b"different selected bytes")
        receipt = self.assemble()
        self.assertEqual(
            production.sha256(self.built_utilities / "bin/sed"),
            production.sha256(self.root / "output/environment/bin/sed"))
        self.assertIn("built_utility_product_sha256",
                      json.loads((self.root / "output/provenance.json").read_text()))
        self.assertGreater(receipt["bytes"], 0)
        evidence = self.built_utilities / "build-evidence.json"
        changed = json.loads(evidence.read_text())
        changed["effects"] = "unverified"
        evidence.write_bytes(production.canonical_json(changed))
        with self.assertRaisesRegex(ValueError, "evidence differs"):
            self.assemble("bad-evidence")

    def test_built_evidence_is_derived_by_the_actual_producer_owner(self):
        observed = json.loads((self.built_utilities / "build-evidence.json").read_text())
        expected = utilities.build_evidence(
            self.source_config, self.support_config["support"])
        self.assertEqual(observed, expected)
        self.assertEqual(
            observed["recipe_source_sha256"],
            production.sha256(OWNER / "lib/utilities.py"))
        self.assertEqual(
            observed["support_contract_sha256"],
            hashlib.sha256(
                production.canonical_json(self.support_config["support"])).hexdigest())
        self.assertNotEqual(
            observed["support_contract_sha256"],
            hashlib.sha256(production.canonical_json(self.support_config)).hexdigest())
        self.assertEqual(
            production.validate_built_utilities(
                self.built_utilities, self.config,
                self.source_config, self.support_config),
            production.input_inventory(self.built_utilities))

    def test_built_product_missing_command_or_extra_top_level_member_refuses(self):
        (self.built_utilities / "bin/sed").unlink()
        with self.assertRaisesRegex(ValueError, "incomplete command inventory"):
            self.assemble("missing-command")
        self.add_built_file("bin/sed", mode=0o755)
        self.add_built_file("ambient-helper")
        with self.assertRaisesRegex(ValueError, "unexpected built utility product member"):
            self.assemble("extra-member")

    def test_missing_sources_or_transformer_is_not_qualified(self):
        changed = copy.deepcopy(self.config)
        del changed["files"]["corresponding-sources/upstream.tar"]
        with self.assertRaisesRegex(ValueError, "corresponding source"):
            production.validate_config(changed)
        del self.config["inputs"][production.ELF_TOOLS["patchelf"]]
        with self.assertRaisesRegex(ValueError, "ELF authoring tools"):
            production.validate_config(self.config)

    def test_payload_bounds_apply_before_copy(self):
        self.config["inputs"]["utilities/cat"]["bytes"] = production.MAX_FILE_BYTES + 1
        with self.assertRaises(ValueError):
            self.assemble()
        self.assertFalse((self.root / "output").exists())

    def test_nonexecutable_commands_and_loaders_refuse_before_assembly(self):
        for source in ("lib/ld-linux-x86-64.so.2", production.ELF_TOOLS["loader"]):
            with self.subTest(source=source):
                config = copy.deepcopy(self.config)
                config["inputs"][source]["mode"] = 0o644
                with self.assertRaisesRegex(ValueError, "executable mode"):
                    production.validate_config(config)
        (self.built_utilities / "bin/sed").chmod(0o644)
        with self.assertRaisesRegex(ValueError, "not executable"):
            self.assemble("nonexecutable-built")
        self.assertFalse((self.root / "output").exists())

    def test_loader_is_required_independently_of_needed_libraries(self):
        config = copy.deepcopy(self.config)
        del config["files"][production.RUNTIME_LOADER]
        with self.assertRaisesRegex(ValueError, "interpreter must be included"):
            production.validate_config(config)
        self.assemble()
        (self.root / "output/environment/lib/ld-linux-x86-64.so.2").unlink()
        with self.assertRaises(FileNotFoundError):
            production.check_closure(self.root / "output/environment",
                                     {**self.config["files"], **self.config["built_utility_files"]},
                                     FakeElfTools(), set(), runtime_root=production.RUNTIME_ROOT)

    def test_loader_cannot_itself_depend_on_another_loader(self):
        class DependentLoader(FakeElfTools):
            def facts(self, path):
                result = super().facts(path)
                if path.name == "ld-linux-x86-64.so.2":
                    result["needed"] = ["libc.so.6"]
                return result
        with self.assertRaisesRegex(ValueError, "independently loadable"):
            self.assemble_with_tools("output", DependentLoader())

    def test_static_pie_is_not_confused_with_external_runtime_dependencies(self):
        class StaticPieTools(FakeElfTools):
            def facts(self, path):
                result = super().facts(path)
                if path.name == "rg":
                    result["dynamic"] = True
                return result
        before = production.sha256(self.inputs / "utilities/rg")
        self.assemble_with_tools("output", StaticPieTools())
        self.assertEqual(before, production.sha256(self.root / "output/environment/bin/rg"))
        self.config["relocate"] = ["environment/bin/rg", "environment/bin/zsh"]
        with self.assertRaisesRegex(ValueError, "only declared dynamic ELF"):
            self.assemble_with_tools("bad", StaticPieTools())

    def test_changed_output_cannot_pass_independent_comparison(self):
        first = self.assemble()
        (self.root / "output/environment/bin/sed").write_bytes(b"modified candidate")
        second = self.assemble("verification")
        self.assertEqual(first, second)
        self.assertNotEqual(production.receipt(self.root / "output"), second)

    def test_operation_and_runtime_contracts_do_not_request_callbacks_or_host_tools(self):
        for name in ("assemble.py", "verify.py"):
            source = (OWNER / name).read_text()
            self.assertIn("effects: live", source)
            self.assertIn("filesystem_authority: captured_execution", source)
            self.assertIn("network_authority: isolated", source)
            self.assertIn("protocol:ryeos/core/opaque", source)
            self.assertIn("800d4969489634cc", source)
            self.assertNotIn("cc090b3d53dd41c0", source)
            self.assertIn("type: multi", source)
            self.assertIn("authoring-utility-sources.yaml", source)
            self.assertIn("authoring-build-support.yaml", source)
            self.assertIn("built-utilities is selected by that same Graph", source)
            self.assertNotIn("mode: captured", source)
            self.assertNotIn("locator:", source)
            self.assertNotIn("shared_exclusive", source)
        runtime = (OWNER / "runtime.yaml").read_text()
        self.assertIn("source_scope:", runtime)
        self.assertIn("RYEOS_VERIFIED_CODE_MAP", runtime)
        self.assertIn("realization:producer-python/lib/ld-musl-x86_64.so.1", runtime)
        self.assertNotIn("local_binary", runtime)
        # Empty PATH is still a typed path contribution, not an unproven
        # runtime-descriptor overwrite of the protected process field.
        self.assertIn('env_config:\n', runtime)
        self.assertIn('  env_paths:\n    PATH:\n      prepend: []\n      append: []', runtime)
        self.assertNotIn('    PATH:', runtime.split('\nconfig:\n', 1)[1])


if __name__ == "__main__":
    unittest.main()
