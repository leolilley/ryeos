import copy
import ast
import hashlib
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
VERIFIER_BOOTSTRAP_PATH = ROOT / "bundles/standard/.ai/config/development/ryeos/gnu-python-verifier-bootstrap-activation.yaml"
PRIVATE_PRODUCTS_PATH = ROOT / ".ai/config/development/ryeos/gnu-python-private-products.yaml"
PRIVATE_GRAPH_PATH = ROOT / ".ai/graphs/ryeos/development/gnu-python-private-production.yaml"
GPU_DRAFT_PRODUCTS_PATH = ROOT / "bundles/local-inference/.ai/config/ryeos-runtime/gnu-cuda-products-draft.yaml"
PRIVATE_VERIFIER_PATH = ROOT / "bundles/standard/.ai/tools/ryeos/environments/qualification/gnu-python-private.yaml"
PRIVATE_POLICY_PATH = ROOT / "bundles/standard/.ai/config/ryeos/environments/qualification/gnu-python-private.yaml"
PRIVATE_RUNTIME_PATH = ROOT / ".ai/config/development/ryeos/gnu-python-private-qualified-runtime.yaml"


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
        self.assertEqual(tool["config_schema"]["properties"]["profile"], {
            "type": "string", "enum": ["fixed_absolute_v1", "private_origin_v1"]})
        declarations = {entry["id"]: entry for entry in tool["external_content"]}
        archive = declarations["gnu-python-archives"]
        self.assertEqual(archive["digest"], contract()["archives"]["manifest"]["digest"])
        self.assertEqual(archive["mount_root"], "execution_runtime")
        self.assertEqual(archive["mount"], "gnu-python-archives")

    def test_exact_authored_contract_produces_only_after_exact_input_check(self):
        value = contract()
        self.assertEqual(value["schema"], "ryeos.gnu_python_production_inputs.v3")
        self.assertEqual(value["private_profile"], {
            "id": "private_origin_v1",
            "runtime_root": "/ryeos/realizations/python-gnu-private/python",
            "runpath_policy": "member-relative-origin-v1",
        })
        self.assertIs(validate_contract(value), value)
        with mock.patch(
            "gnu_python_production._check_archive_tree",
            side_effect=ValueError("fixture exact archive check"),
        ) as exact_archive:
            with self.assertRaisesRegex(ValueError, "fixture exact archive check"):
                produce(Path("/unused-after-refusal"), value)
        exact_archive.assert_called_once()

    def test_evidence_config_hash_covers_the_actual_v3_contract(self):
        value = contract()
        expected = hashlib.sha256(production.canonical_json(value)).hexdigest()
        self.assertEqual(production._config_digest(value), expected)
        changed = copy.deepcopy(value)
        changed["private_profile"]["runtime_root"] += "-changed"
        self.assertNotEqual(production._config_digest(value),
                            production._config_digest(changed))
        for field, replacement in (
            ("id", "other_profile"),
            ("runtime_root", "/tmp/python"),
            ("runpath_policy", "fixed"),
        ):
            changed = copy.deepcopy(value)
            changed["private_profile"][field] = replacement
            with self.assertRaisesRegex(ValueError, "private GNU Python profile"):
                validate_contract(changed)

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

    def test_private_profile_is_a_closed_tool_parameter(self):
        request = json.dumps({"resolved_config": contract(),
                              "profile": "private_origin_v1"}).encode()
        with mock.patch.object(sys, "argv", ["produce-gnu-python", "--project-path", "/tmp"]), \
                mock.patch.object(sys, "stdin") as stdin, \
                mock.patch("gnu_python_production.produce",
                           return_value={"output_path": production.PRIVATE_OUTPUT.as_posix()}) as run, \
                mock.patch.object(sys, "stdout", new=io.StringIO()):
            stdin.buffer = io.BytesIO(request)
            run_production()
        run.assert_called_once_with(Path("/tmp"), contract(), "private_origin_v1")

        request = json.dumps({"resolved_config": contract(), "profile": "invented"}).encode()
        with mock.patch.object(sys, "argv", ["produce-gnu-python", "--project-path", "/tmp"]), \
                mock.patch.object(sys, "stdin") as stdin:
            stdin.buffer = io.BytesIO(request)
            with self.assertRaisesRegex(ValueError, "unsupported GNU Python output profile"):
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
        self.assertEqual(verifier["external_content"], [{
            "id": "producer-python",
            "kind": "tree",
            "mode": "pinned",
            "digest": "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf",
            "mount_root": "execution_runtime",
            "mount": "producer-python",
        }])
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
            "realization_id": "producer-python",
            "relative_path": "lib/ld-musl-x86_64.so.1",
        })
        arguments = verifier["config"]["args"]
        self.assertEqual(
            arguments[0],
            "/ryeos/realizations/producer-python/python/bin/python3.14",
        )
        verifier_program = arguments[-2]["literal"]
        program = arguments[-1]["literal"]
        compile(verifier_program, str(VERIFIER_PATH), "exec")
        compile(program, str(VERIFIER_PATH), "exec")
        self.assertIn('set(by_id) != {"producer-python", "subject"}', verifier_program)
        self.assertIn('subject["manifest_hash"] == producer_manifest', verifier_program)
        self.assertIn('root + "/lib/ld-linux-x86-64.so.2"', verifier_program)
        self.assertIn('subprocess.run(', verifier_program)
        self.assertIn('result.get("subject_manifest_hash") != subject_hash', verifier_program)
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

    def test_private_profile_selects_distinct_graph_recipe_root_and_draft_worker_relation(self):
        graph = yaml.safe_load(PRIVATE_GRAPH_PATH.read_text())
        products = yaml.safe_load(PRIVATE_PRODUCTS_PATH.read_text())
        draft = yaml.safe_load(GPU_DRAFT_PRODUCTS_PATH.read_text())
        self.assertEqual(graph["product_recipe"],
                         "config:development/ryeos/gnu-python-private-products")
        action = graph["config"]["nodes"]["produce"]["action"]
        self.assertEqual(action["item_id"],
                         "tool:ryeos/development/authoring-environment-production/produce-gnu-python")
        self.assertEqual(action["params"], {"profile": "private_origin_v1"})
        self.assertEqual(products["build_products"]["output_roots"][0]["path"],
                         "products/gnu-python-relocatable-distribution")
        runtime = products["build_products"]["products"][0]
        self.assertEqual(runtime["name"], "runtime_private")
        self.assertEqual(runtime["path"], "products/gnu-python-relocatable-distribution")
        relation = draft["product_relationships"]["relationships"][0]
        self.assertEqual(relation["producer"], {
            "canonical_ref": "graph:ryeos/development/gnu-python-private-production",
            "recipe_binding": "product_recipe",
            "product_name": "runtime_private",
            "parameters": {},
        })
        self.assertEqual(relation["qualification"], {
            "policy_ref": "config:ryeos/environments/qualification/gnu-python-private",
            "required_claims": ["gnu_python_private_origin_startup_providers_v1",
                                "gnu_python_zlib_1_3_2"],
        })

    def test_private_qualification_verifier_refuses_without_origin_and_omission_checks(self):
        verifier = yaml.safe_load(PRIVATE_VERIFIER_PATH.read_text())
        policy = yaml.safe_load(PRIVATE_POLICY_PATH.read_text())
        runtime = yaml.safe_load(PRIVATE_RUNTIME_PATH.read_text())
        products = yaml.safe_load(PRIVATE_PRODUCTS_PATH.read_text())
        programs = [entry["literal"] for entry in verifier["config"]["args"]
                    if isinstance(entry, dict) and "literal" in entry]
        self.assertEqual(len(programs), 2)
        self.assertEqual(verifier["external_content"][0]["digest"],
                         "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf")
        self.assertEqual([slot["id"] for slot in verifier["external_product_slots"]],
                         ["subject", "fixed_control", "gnu-python-support"])
        self.assertEqual(verifier["external_product_slots"][1]["relationship"],
                         "runtime_to_private_qualification_control")
        self.assertEqual(verifier["external_product_slots"][2]["relationship"],
                         "prepared_inputs_to_gnu_python_private_qualification")
        for program in programs:
            compile(program, str(PRIVATE_VERIFIER_PATH), "exec")
        outer = programs[0]
        probe = programs[-1]
        self.assertEqual(policy["product_qualification_policy"]["allowed_claims"], [
            "gnu_python_private_origin_startup_providers_v1", "gnu_python_zlib_1_3_2"])
        self.assertEqual(runtime["external_product_slots"][0]["relationship"],
                         "runtime_private_to_qualified_runtime")
        relation = next(item for item in products["product_relationships"]["relationships"]
                        if item["name"] == "runtime_private_to_qualified_runtime")
        self.assertEqual(relation["qualification"]["policy_ref"],
                         "config:ryeos/environments/qualification/gnu-python-private")
        self.assertIn('"failure_before_python") is not True', outer)
        self.assertIn('"failure_output_sha256"', outer)
        self.assertIn('"provider_mappings"', outer)
        self.assertIn('"ryeos.gnu-python-elf-inventory.v3"', probe)
        self.assertIn('"private_origin_v1"', probe)
        self.assertIn('distribution + "/elf-inventory.json"', probe)
        self.assertIn('distribution + "/notices/python-build-standalone/PYTHON.json"', probe)
        self.assertIn('"$ORIGIN/../lib"', probe)
        self.assertIn('"$ORIGIN/../.."', probe)
        self.assertIn('"$ORIGIN"', probe)
        self.assertIn('"/proc/self/maps"', probe)
        self.assertIn('"/ryeos/realizations/python-gnu/python/lib/libgcc_s.so.1"', probe)
        self.assertIn('fixed_control_entry_dynamic = readelf_path(', probe)
        self.assertIn('"/ryeos/realizations/python-gnu/python/bin/python3.14", "-d")', probe)
        self.assertIn('fixed_control_entry_runpath != ["/ryeos/realizations/python-gnu/python/lib"]', probe)
        self.assertIn('"30c61ab012a4241bed033725a09b61f5fdd3bb7df95ee852d0b096520524c7af"', probe)
        self.assertIn('support + "/elf/bin/readelf"', probe)
        self.assertIn('actual_runpath != item["runpath"]', probe)
        self.assertIn('actual_nodeflib != item["nodeflib"]', probe)
        self.assertIn('read_regular(root + "/" + member', probe)
        self.assertIn('shutil.copytree(root, probe_root, symlinks=True)', probe)
        self.assertIn('"error while loading shared libraries: libgcc_s.so.1"', probe)
        self.assertNotIn('os.symlink(root + "/lib/" + name', probe)
        self.assertIn('"failure_before_python": True', probe)
        self.assertIn('"selected_copy_is_complete_runtime": True', probe)
        self.assertIn('"same_loader_complete_control_exit_code": positive_status', probe)
        self.assertIn('"same_loader_complete_control_started_python": os.path.exists(sentinel)', probe)
        self.assertIn('"failure_output_sha256": hashlib.sha256(failed_output).hexdigest()', probe)
        self.assertIn('"provider_mappings": {name: next(iter(paths))', probe)
        self.assertIn('os.path.exists(sentinel)', probe)
        self.assertIn('"gnu_python_private_origin_startup_providers_v1"', probe)
        self.assertIn('"gnu_python_zlib_1_3_2"', probe)
        self.assertIn("def run_limited(", probe)
        self.assertIn("limit + 1 - total", probe)
        self.assertNotIn("subprocess.run(", probe)
        tree = ast.parse(probe)
        re_import = next(node for node in tree.body
                         if isinstance(node, ast.Import) and
                         any(alias.name == "re" for alias in node.names))
        expected = next(node for node in tree.body
                        if isinstance(node, ast.FunctionDef) and
                        node.name == "expected_runpath")
        namespace = {}
        exec(compile(ast.Module(body=[re_import, expected], type_ignores=[]),
                     str(PRIVATE_VERIFIER_PATH), "exec"), namespace)
        path_for = namespace["expected_runpath"]
        self.assertEqual(path_for("bin/python3.14"), "$ORIGIN/../lib")
        self.assertEqual(path_for("lib/libc.so.6"), "$ORIGIN")
        self.assertEqual(path_for("lib/python3.14/lib-dynload/math.so"), "$ORIGIN/../..")
        with self.assertRaisesRegex(SystemExit, "unsafe member path"):
            path_for("../../etc/passwd")
        with self.assertRaisesRegex(SystemExit, "unsupported relative layout"):
            path_for("other/module.so")

    def test_independent_verifier_rejects_self_verification_before_subject_execution(self):
        verifier = yaml.safe_load(VERIFIER_PATH.read_text())
        program = verifier["config"]["args"][-2]["literal"]
        manifest = verifier["external_content"][0]["digest"]
        realization = lambda identity, mount: {
            "id": identity,
            "kind": "tree",
            "mode": "pinned",
            "manifest_hash": manifest,
            "entry_count": 1,
            "total_bytes": 1,
            "mount_root": "execution_runtime",
            "mount": mount,
        }
        environment = json.dumps([
            realization("producer-python", "producer-python"),
            realization("subject", "python-gnu"),
        ])
        with mock.patch.dict("os.environ", {"RYEOS_EXTERNAL_REALIZATIONS": environment}, clear=True), \
                mock.patch.object(sys, "argv", ["-c", "unused subject probe"]):
            with self.assertRaisesRegex(SystemExit, "may not qualify itself"):
                exec(compile(program, str(VERIFIER_PATH), "exec"), {})

    def test_independent_verifier_bootstrap_is_bound_to_the_verifier_consumer(self):
        activation = yaml.safe_load(VERIFIER_BOOTSTRAP_PATH.read_text())
        verifier = yaml.safe_load(VERIFIER_PATH.read_text())
        self.assertEqual(
            activation["consumer_ref"],
            "tool:ryeos/environments/qualification/gnu-python",
        )
        declaration = verifier["external_content"][0]
        component = activation["components"][0]
        self.assertEqual(component["id"], declaration["id"])
        self.assertEqual(component["storage"], "content")
        self.assertEqual(
            declaration["digest"],
            "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf",
        )
        self.assertEqual(
            activation["sources"][0]["sha256"],
            "7d6239902fb584ef27aee8c97f5e9a0e66409750611f74d7e16235ddd4278eef",
        )

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
            "schema": "ryeos.product_qualification_policy.v4",
            "verifier_ref": "tool:ryeos/environments/qualification/gnu-python",
            "subject_declaration_id": "subject",
            "allowed_claims": ["gnu_python_extension_startup_providers_v1",
                               "gnu_python_zlib_1_3_2"],
            "minimum_verifier_process_settlement": "trusted_process_group_absent",
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
