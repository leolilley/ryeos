import copy
import ast
from pathlib import Path
import io
import json
import sys
import tempfile
import unittest
from unittest import mock

import yaml


ROOT = Path(__file__).resolve().parents[3]
LIB = ROOT / ".ai/tools/ryeos/development/authoring-environment-production/lib"
sys.path.insert(0, str(LIB))

import gnu_python_production as production
from gnu_python_production import produce, run_production, validate_contract


CONFIG_PATH = ROOT / ".ai/config/development/ryeos/gnu-python-production-inputs.yaml"
TOOL_PATH = ROOT / ".ai/tools/ryeos/development/authoring-environment-production/produce-gnu-python.py"
PRODUCTS_PATH = ROOT / ".ai/config/development/ryeos/gnu-python-products.yaml"
POLICY_PATH = ROOT / "bundles/standard/.ai/config/ryeos/environments/qualification/gnu-python.yaml"
CONSUMER_PATH = ROOT / ".ai/config/development/ryeos/gnu-python-qualified-runtime.yaml"
VERIFIER_PATH = ROOT / "bundles/standard/.ai/tools/ryeos/environments/qualification/gnu-python.yaml"


def contract():
    return yaml.safe_load(CONFIG_PATH.read_text())


class GnuPythonProductionTests(unittest.TestCase):
    def _qualification_helpers(self, *names):
        verifier = yaml.safe_load(VERIFIER_PATH.read_text())
        program = verifier["config"]["args"][-1]["literal"]
        tree = ast.parse(program)
        selected = [node for node in tree.body
                    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
                    and node.name in names]
        self.assertEqual({node.name for node in selected}, set(names))
        namespace = {}
        exec(compile(ast.Module(body=selected, type_ignores=[]),
                     str(VERIFIER_PATH), "exec"), namespace)
        return tuple(namespace[name] for name in names)

    def test_large_archive_pin_is_owned_by_the_large_content_capable_tool(self):
        header = []
        for line in TOOL_PATH.read_text().splitlines():
            if line.startswith("# ryeos:signed:"):
                continue
            if not line.startswith("#"):
                break
            if line.startswith("# "):
                header.append(line[2:])
        tool = yaml.safe_load("\n".join(header))["ryeos-tool"]
        self.assertEqual(tool["config_resolve"], {
            "type": "single",
            "spec": {
                "path": "development/ryeos/gnu-python-production-inputs.yaml",
                "mode": "first_match",
            },
        })
        declarations = {entry["id"]: entry for entry in tool["external_content"]}
        archive = declarations["gnu-python-archives"]
        self.assertEqual(archive["digest"], contract()["archives"]["manifest"]["digest"])
        self.assertEqual(archive["mount_root"], "execution_runtime")
        self.assertEqual(archive["mount"], "gnu-python-archives")

    def test_exact_authored_contract_produces_only_after_exact_input_check(self):
        value = contract()
        self.assertEqual(value["schema"], "ryeos.gnu_python_production_inputs.v2")
        self.assertIs(validate_contract(value), value)
        with mock.patch(
            "gnu_python_production._check_archive_tree",
            side_effect=ValueError("fixture exact archive check"),
        ) as exact_archive:
            with self.assertRaisesRegex(ValueError, "fixture exact archive check"):
                produce(Path("/unused-after-refusal"), value)
        exact_archive.assert_called_once()

    def test_single_config_resolve_body_is_consumed_without_a_path_map(self):
        value = contract()
        request = json.dumps({"resolved_config": value}).encode()
        with mock.patch.object(sys, "argv", ["produce-gnu-python", "--project-path", "/tmp"]), \
                mock.patch.object(sys, "stdin") as stdin, \
                mock.patch("gnu_python_production.produce", return_value={"output_path": "products/x"}) as production, \
                mock.patch.object(sys, "stdout", new=io.StringIO()):
            stdin.buffer = io.BytesIO(request)
            run_production()
        production.assert_called_once_with(Path("/tmp"), value)

        nested = json.dumps({"resolved_config": {"development/ryeos/gnu-python-production-inputs.yaml": value}}).encode()
        with mock.patch.object(sys, "argv", ["produce-gnu-python", "--project-path", "/tmp"]), \
                mock.patch.object(sys, "stdin") as stdin, \
                mock.patch("gnu_python_production.produce",
                           side_effect=lambda _project, supplied: validate_contract(supplied)):
            stdin.buffer = io.BytesIO(nested)
            with self.assertRaises(ValueError):
                run_production()

    def test_qualification_requirement_is_source_contract_not_proof(self):
        value = contract()
        requirement = value["qualification_requirements"]["zlib-ng"]
        self.assertEqual(requirement["expected_runtime_zlib_version"], "1.3.2")
        self.assertNotIn("runtime_probe", requirement)
        self.assertNotIn("activation_gates", value)
        invented = copy.deepcopy(value)
        invented["qualification_requirements"]["zlib-ng"]["runtime_probe"] = {
            "claimed_version": "1.3.2"
        }
        with self.assertRaisesRegex(ValueError, "invalid zlib qualification"):
            validate_contract(invented)

    def test_archive_manifest_and_needed_transform_are_exact(self):
        self.assertEqual(contract()["needed_replacements"], {
            "lib/libpython3.so": {
                "$ORIGIN/../lib/libpython3.14.so.1.0": "libpython3.14.so.1.0",
            },
        })
        self.assertEqual(contract()["needed_additions"], {
            "bin/python3.14": ["libgcc_s.so.1"],
        })
        compatibility = contract()["compatibility_files"]
        self.assertEqual(compatibility["lib/libgcc_s.so.1"], {
            "source": "elf/lib/libgcc_s.so.1",
            "bytes": 182856,
            "mode": 0o644,
            "sha256": "30c61ab012a4241bed033725a09b61f5fdd3bb7df95ee852d0b096520524c7af",
        })
        self.assertIn("lib/libpthread.so.0", compatibility)
        self.assertEqual(contract()["retained_gnu_sources"]["notices/gcc-COPYRIGHT"], {
            "source": "notices/gcc-COPYRIGHT",
            "bytes": 69004,
            "mode": 0o644,
            "sha256": "20390f8a6f3b1e4d7cb45dd8652dabb259bbef688cbad839bcdb0b9ba7252f79",
        })
        self.assertFalse(any("gcc" in member and member.startswith("sources/")
                             for member in contract()["retained_gnu_sources"]))
        for mutation in ("manifest", "needed"):
            value = copy.deepcopy(contract())
            if mutation == "manifest":
                value["archives"]["manifest"]["digest"] = "0" * 64
                message = "manifest identity"
            else:
                value["needed_replacements"]["lib/libpython3.so"] = {
                    "$ORIGIN/../lib/libpython3.14.so.1.0": "other.so"
                }
                # The map is syntactically safe and signed. Exact observed-edge
                # equality is deliberately enforced later by gnu_elf against
                # the selected archive, not invented by this Config parser.
                message = None
            if message:
                with self.assertRaisesRegex(ValueError, message):
                    validate_contract(value)
            else:
                validate_contract(value)

    def test_independent_verifier_uses_selected_runtime_and_sealed_subject(self):
        verifier = yaml.safe_load(VERIFIER_PATH.read_text())
        self.assertEqual(verifier["executor_id"], "@subprocess")
        self.assertEqual(verifier["execution_protocol"], "protocol:ryeos/core/opaque")
        self.assertEqual(verifier["network_authority"], "isolated")
        self.assertEqual(verifier["external_content"], [])
        self.assertEqual(verifier["external_product_slots"], [{
            "id": "subject",
            "relationship_ref": "config:development/ryeos/gnu-python-products",
            "relationship": "runtime_to_qualification_verifier",
            "kind": "tree",
            "mount_root": "execution_runtime",
            "mount": "python-gnu",
        }])
        self.assertEqual(verifier["env_config"]["interpreter"], {
            "type": "realization_member",
            "realization_id": "subject",
            "relative_path": "python/bin/python3.14",
        })
        arguments = verifier["config"]["args"]
        program = arguments[-1]["literal"]
        compile(program, str(VERIFIER_PATH), "exec")
        self.assertIn("RYEOS_EXTERNAL_REALIZATIONS", program)
        self.assertIn('zlib.ZLIB_RUNTIME_VERSION != "1.3.2"', program)
        self.assertIn('zlib.__spec__.origin != "built-in"', program)
        self.assertIn('_ctypes.__spec__.origin != "built-in"', program)
        self.assertIn('"shared_lib" in zlib_build[0]', program)
        self.assertIn('"shared_lib" in ctypes_build[0]', program)
        self.assertIn("e4596127ee78428301bfc9beac06547a14ab7ead983c4c390d5256ecde061fa9", program)
        self.assertIn("ryeos.gnu_python_runtime_probe.v2", program)
        self.assertIn("dlvsym", program)
        self.assertIn("libgcc_s.so.1", program)
        self.assertIn("libpthread.so.0", program)
        self.assertIn("/proc/self/maps", program)
        self.assertIn("os.O_NOFOLLOW", program)
        self.assertIn("gnu_python_extension_startup_providers_v1", program)
        self.assertIn('"LD_LIBRARY_PATH" in os.environ', program)
        self.assertIn('"LD_PRELOAD" in os.environ', program)
        self.assertNotIn('os.environ["LD_', program)
        self.assertNotIn("os.environ.setdefault", program)
        self.assertNotIn("lib-dynload/zlib", program)
        self.assertIn('"subject_manifest_hash": subject["manifest_hash"]', program)
        self.assertNotIn('"success": True', program)
        self.assertIn("print(json.dumps(result", program)

    def test_startup_provider_helpers_reject_spoofed_maps_and_missing_versions(self):
        mapped_provider_paths, probe_versioned_symbols = self._qualification_helpers(
            "mapped_provider_paths", "probe_versioned_symbols")
        runtime = "/ryeos/realizations/python-gnu/python/lib/"
        maps = "\n".join([
            f"1000-2000 r--p 0 00:00 1 {runtime}libgcc_s.so.1",
            f"2000-3000 r-xp 0 00:00 1 {runtime}libgcc_s.so.1",
            f"3000-4000 r--p 0 00:00 2 {runtime}libpthread.so.0",
        ])
        observed = mapped_provider_paths(maps, ("libgcc_s.so.1", "libpthread.so.0"))
        self.assertEqual(observed, {
            "libgcc_s.so.1": {runtime + "libgcc_s.so.1"},
            "libpthread.so.0": {runtime + "libpthread.so.0"},
        })
        spoofed = mapped_provider_paths(
            maps.replace(runtime + "libgcc_s.so.1", "/usr/lib/libgcc_s.so.1"),
            ("libgcc_s.so.1", "libpthread.so.0"))
        self.assertNotEqual(spoofed, observed)
        with self.assertRaisesRegex(SystemExit, "was deleted"):
            mapped_provider_paths(
                maps.replace("libgcc_s.so.1", "libgcc_s.so.1 (deleted)", 1),
                ("libgcc_s.so.1", "libpthread.so.0"))

        class FakeDlvsym:
            def __init__(self):
                self.argtypes = None
                self.restype = None
                self.missing = None

            def __call__(self, _handle, symbol, version):
                return 0 if (symbol, version) == self.missing else 1

        class FakeCtypes:
            c_void_p = object
            c_char_p = object

            def __init__(self):
                self.lookup = FakeDlvsym()
                self.handles = {
                    runtime + "libgcc_s.so.1": 11,
                    "libgcc_s.so.1": 11,
                }

            def CDLL(self, path, mode=None):
                if path is None:
                    return type("Process", (), {"dlvsym": self.lookup})()
                self.assert_mode = mode
                return type("Handle", (), {"_handle": self.handles[path]})()

        class FakeOs:
            RTLD_NOLOAD = 4
            RTLD_NOW = 2
            RTLD_LOCAL = 0

        fake = FakeCtypes()
        contracts = {"libgcc_s.so.1": [("_Unwind_GetIPInfo", "GCC_4.2.0")]}
        probes = probe_versioned_symbols(
            {"libgcc_s.so.1": runtime + "libgcc_s.so.1"}, contracts, fake, FakeOs)
        self.assertEqual(probes, [{
            "provider": "libgcc_s.so.1",
            "symbol": "_Unwind_GetIPInfo",
            "version": "GCC_4.2.0",
        }])
        fake.lookup.missing = (b"_Unwind_GetIPInfo", b"GCC_4.2.0")
        with self.assertRaisesRegex(SystemExit, "required versioned symbol"):
            probe_versioned_symbols(
                {"libgcc_s.so.1": runtime + "libgcc_s.so.1"}, contracts, fake, FakeOs)
        fake.lookup.missing = None
        fake.handles["libgcc_s.so.1"] = 12
        with self.assertRaisesRegex(SystemExit, "different startup provider"):
            probe_versioned_symbols(
                {"libgcc_s.so.1": runtime + "libgcc_s.so.1"}, contracts, fake, FakeOs)

    def test_qualification_policy_and_relationships_are_closed(self):
        products = yaml.safe_load(PRODUCTS_PATH.read_text())
        roots = {
            root["name"]: root
            for root in products["build_products"]["output_roots"]
        }
        for product in products["build_products"]["products"]:
            self.assertIn(product["storage"], {"content", "large_content"})
            if product["source"]["kind"] == "workspace_output":
                self.assertEqual(product["storage"], roots[product["source"]["root"]]["storage"])
        relationships = {
            relationship["name"]: relationship
            for relationship in products["product_relationships"]["relationships"]
        }
        self.assertEqual(list(relationships), sorted(relationships))
        verifier = relationships["runtime_to_qualification_verifier"]
        self.assertEqual(verifier["consumer"], {
            "canonical_ref": "tool:ryeos/environments/qualification/gnu-python",
            "declaration_id": "subject",
        })
        self.assertEqual(verifier["qualification"], {
            "policy_ref": None,
            "required_claims": [],
        })
        qualified = relationships["runtime_to_qualified_runtime"]
        self.assertEqual(qualified["qualification"], {
            "policy_ref": "config:ryeos/environments/qualification/gnu-python",
            "required_claims": ["gnu_python_extension_startup_providers_v1",
                                "gnu_python_zlib_1_3_2"],
        })
        policy = yaml.safe_load(POLICY_PATH.read_text())["product_qualification_policy"]
        self.assertEqual(policy, {
            "schema": "ryeos.product_qualification_policy.v1",
            "verifier_ref": "tool:ryeos/environments/qualification/gnu-python",
            "subject_declaration_id": "subject",
            "allowed_claims": ["gnu_python_extension_startup_providers_v1",
                               "gnu_python_zlib_1_3_2"],
            "verifier_parameters": {},
        })
        consumer = yaml.safe_load(CONSUMER_PATH.read_text())
        self.assertEqual(consumer["external_product_slots"][0]["relationship"],
                         "runtime_to_qualified_runtime")
        for source in (PRODUCTS_PATH, POLICY_PATH, CONSUMER_PATH, VERIFIER_PATH):
            self.assertNotIn("tool:test/", source.read_text())
            self.assertNotIn("config:test/", source.read_text())

    def test_failed_build_stays_inside_exact_output_partition(self):
        with tempfile.TemporaryDirectory() as scratch:
            project = Path(scratch)
            outside = project / "outside"
            outside.write_text("retained")
            archive = {"member": "install.tar", "bytes": 1, "sha256": "0" * 64}
            config = {"archives": {"install": archive, "metadata": archive}}

            def fail_extract(_archive, destination, **_kwargs):
                self.assertEqual(
                    destination,
                    project.joinpath(*production.OUTPUT.parts, "install-selection"))
                destination.mkdir(parents=True)
                (destination / "link").symlink_to(outside)
                raise ValueError("primary producer failure")

            with mock.patch.object(production, "validate_contract"), \
                    mock.patch.object(production, "_check_archive_tree",
                                      return_value={"install.tar": Path("archive")}), \
                    mock.patch.object(production, "ordinary_member"), \
                    mock.patch.object(production, "extract_install",
                                      side_effect=fail_extract):
                with self.assertRaisesRegex(ValueError, "primary producer failure"):
                    production.produce(project, config)
            self.assertTrue(project.joinpath(*production.OUTPUT.parts).is_dir())
            self.assertFalse((project / "products/gnu-python-production-staging").exists())
            self.assertEqual(outside.read_text(), "retained")

if __name__ == "__main__":
    unittest.main()
