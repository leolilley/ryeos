#!/usr/bin/env python3
"""Static regressions for the bundle-release execution closure.

These checks deliberately inspect the signed source assets. Runtime products
and consumer bindings are checked elsewhere; this file prevents the authored
release surface from reintroducing ambient host executables or authority.
"""

from pathlib import Path
import ast
import re
from types import SimpleNamespace
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
ASSET = ROOT / "bundles/bundle-release/.ai"
TOOLS = ASSET / "tools/ryeos/bundle-release"
GRAPHS = ASSET / "graphs/ryeos/bundle-release"
RELATIONSHIPS = ASSET / "config/bundle-release/execution-environment-products.yaml"
TOOL_RELATIONSHIPS = ASSET / "config/bundle-release/execution-tool-products.yaml"
SOURCE_AUTHORITY_FILES = (
    ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
    ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release_execution.rs",
    ROOT / "crates/daemon/ryeos-app/src/bundle_publication/admitted_build.rs",
)

PYTHON_TOOLS = {
    "native-build": ("tool", "native-build"),
    "portable-build": ("tool", "portable-build"),
    "native-qualify": ("tool", "native-qualify"),
    "portable-qualify": ("tool", "portable-qualify"),
    "signed-capture": ("tool", "signed-capture"),
    "portable-signed-capture": ("tool", "portable-signed-capture"),
    "core-seed-build": ("tool", "core-seed-build"),
    "core-seed-capture": ("tool", "core-seed-capture"),
    "core-seed-qualify": ("tool", "core-seed-qualify"),
    "substrate-build": ("tool", "substrate-build"),
    "substrate-qualify": ("tool", "substrate-qualify"),
}
BUILD_SOURCES = (
    TOOLS / "lib/native-build.py",
    TOOLS / "lib/core-seed-build.py",
)
HOST_BUILD_VARIABLES = {
    "PATH",
    "HOME",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "LIBRARY_PATH",
    "CC",
    "CXX",
    "AR",
    "XDG_CONFIG_HOME",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
}
REALIZATION_PATH = re.compile(r"/ryeos/realizations/([A-Za-z0-9][A-Za-z0-9._-]*)")


class StrictLoader(yaml.SafeLoader):
    pass


def _strict_mapping(loader, node, deep=False):
    mapping = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in mapping:
            raise ValueError(f"duplicate YAML key: {key}")
        mapping[key] = loader.construct_object(value_node, deep=deep)
    return mapping


StrictLoader.add_constructor(
    yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG,
    _strict_mapping,
)


def load_yaml(path: Path):
    return yaml.load(path.read_text(), Loader=StrictLoader)


def slots_by_mount(document):
    return {
        slot.get("mount"): slot
        for slot in document.get("external_product_slots", [])
        if isinstance(slot, dict) and isinstance(slot.get("mount"), str)
    }


def assert_realization_interpreter(test, tool):
    command = tool.get("config", {}).get("command")
    test.assertNotEqual(command, "/usr/bin/python3")
    test.assertEqual(command, "${interpreter}")
    interpreter = tool.get("env_config", {}).get("interpreter")
    test.assertIsInstance(interpreter, dict)
    test.assertEqual(interpreter.get("type"), "realization_member")
    test.assertEqual(interpreter.get("realization_id"), "python")
    relative_path = interpreter.get("relative_path")
    test.assertIsInstance(relative_path, str)
    test.assertRegex(relative_path, r"^python/bin/python3(?:\.[0-9]+)?$")


def assert_slot(test, document, slot_id, mount):
    slots = slots_by_mount(document)
    test.assertIn(mount, slots, f"missing realization mount {mount}")
    test.assertEqual(slots[mount].get("id"), slot_id)
    test.assertEqual(slots[mount].get("mount_root"), "execution_runtime")
    test.assertEqual(slots[mount].get("kind"), "tree")
    relationship = slots[mount].get("relationship_ref")
    test.assertIsInstance(relationship, str)
    test.assertTrue(relationship.startswith("config:"))


class BundleReleaseExecutionClosureTests(unittest.TestCase):
    def test_release_graphs_explicitly_hand_exact_selections_to_child_tools(self):
        expected = {
            "portable-build": ("portable-build", {"python"}),
            "native-build": ("native-build", {"python", "platform", "cargo-vendor", "static-link-inputs"}),
            "signed-capture": ("signed-capture", {"python", "unsigned_bundle"}),
            "portable-signed-capture": ("portable-signed-capture", {"python", "unsigned_bundle"}),
            "core-seed-build": ("core-seed-build", {"python", "platform", "cargo-vendor", "static-link-inputs"}),
            "core-seed-capture": ("core-seed-capture", {"python", "unsigned_core"}),
            "substrate-build": ("substrate-build", {"python"}),
        }
        tool_relationships = {
            relationship["name"]: relationship
            for relationship in load_yaml(TOOL_RELATIONSHIPS)["product_relationships"]["relationships"]
        }
        for graph_name, (tool_name, ids) in expected.items():
            with self.subTest(graph=graph_name):
                graph = load_yaml(GRAPHS / f"{graph_name}.yaml")
                tool = load_yaml(TOOLS / f"{tool_name}.yaml")
                action = next(iter(graph["config"]["nodes"].values()))["action"]
                self.assertEqual(action["item_id"], f"tool:ryeos/bundle-release/{tool_name}")
                self.assertEqual(action["product_selections"], "${inputs.child_product_selections}")
                schema = graph["config"]["config_schema"]
                self.assertIn("child_product_selections", schema["required"])
                self.assertEqual(schema["properties"]["child_product_selections"]["type"], "array")
                graph_slots = {slot["id"]: slot for slot in graph["external_product_slots"]}
                tool_slots = {slot["id"]: slot for slot in tool["external_product_slots"]}
                self.assertEqual(set(graph_slots), ids)
                self.assertEqual(set(tool_slots), ids)
                for slot_id in ids:
                    self.assertEqual(tool_slots[slot_id]["mount"], graph_slots[slot_id]["mount"])
                    if slot_id in {"python", "platform", "cargo-vendor", "static-link-inputs"}:
                        self.assertEqual(tool_slots[slot_id]["relationship_ref"],
                                         "config:bundle-release/execution-tool-products")
                        relationship = tool_relationships[tool_slots[slot_id]["relationship"]]
                        self.assertEqual(relationship["consumer"], {
                            "canonical_ref": action["item_id"], "declaration_id": slot_id,
                        })

    def test_shared_build_helpers_are_in_authenticated_tool_source_scope(self):
        runtime = load_yaml(TOOLS / "runtime.yaml")
        self.assertEqual(runtime["source_scope"], {
            "location": "item_directory",
            "load_roots": ["item_directory"],
            "materialization": "read_only",
        })
        for name in ("native-build", "core-seed-build"):
            with self.subTest(tool=name):
                tool = load_yaml(TOOLS / (name + ".yaml"))
                self.assertEqual(tool["executor_id"], "tool:ryeos/bundle-release/runtime")
                self.assertNotIn("source_scope", tool)
                body = (TOOLS / "lib" / (name + ".py")).read_text()
                for helper in ("release-cargo.py", "release-elf.py"):
                    self.assertTrue((TOOLS / "lib" / helper).is_file())
                    self.assertIn('pathlib.Path(__file__).with_name("' + helper + '")', body)
                self.assertIn("release_cargo.validate_source_configuration(root)", body)

    def test_build_tools_require_the_same_strict_signed_ownership_resolution(self):
        expected = {
            "type": "strict_signed_project_bundle",
            "bundle_name": "bundle-release",
            "config_path": "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml",
        }
        for name in ("native-build", "portable-build", "core-seed-build"):
            with self.subTest(tool=name):
                tool = load_yaml(TOOLS / f"{name}.yaml")
                self.assertEqual(tool["config_resolve"], expected)
                source = TOOLS / "lib" / ("core-seed-build.py" if name == "core-seed-build" else "native-build.py")
                body = source.read_text()
                self.assertIn('verified_ownership_projection(request["resolved_config"])', body)
                self.assertNotIn("scripts/release/bundle-payload-ownership.py", body)

    def test_release_tool_entrypoints_resolve_only_through_admitted_source_mount(self):
        # Source members remain typed until launch redeems the retained closure
        # in its private runtime namespace. Neither installed host paths nor
        # assumed project directories grant executable source authority.
        self.assertEqual({path.stem for path in TOOLS.glob("*.yaml")}, set(PYTHON_TOOLS) | {"runtime"})
        runtime = load_yaml(TOOLS / "runtime.yaml")
        self.assertEqual(runtime["executor_id"], "@subprocess")
        self.assertEqual(runtime["source_scope"], {
            "location": "item_directory",
            "load_roots": ["item_directory"],
            "materialization": "read_only",
        })
        for name in PYTHON_TOOLS:
            with self.subTest(tool=name):
                tool = load_yaml(TOOLS / (name + ".yaml"))
                self.assertEqual(tool["executor_id"], "tool:ryeos/bundle-release/runtime")
                self.assertNotIn("source_scope", tool)
                args = tool["config"]["args"]
                self.assertEqual(args[:2], ["-I", "-B"])
                self.assertEqual(len(args), 3)
                self.assertEqual(set(args[2]), {"source_member"})
                entry = Path(args[2]["source_member"])
                self.assertFalse(entry.is_absolute())
                self.assertNotIn("..", entry.parts)
                self.assertEqual(entry.parent, Path("lib"))
                self.assertTrue((TOOLS / entry).is_file())

    def test_build_commands_select_exact_owned_bins_per_package(self):
        # Execute the real command-building loops without building or touching
        # products. A package can own multiple selected bins, and share other
        # bins with a different bundle; package-only Cargo selection is unsafe.
        expected = [
            {"cargo_package": "shared", "binary": "owned-b", "build_class": "release"},
            {"cargo_package": "shared", "binary": "owned-a", "build_class": "release"},
            {"cargo_package": "other", "binary": "other-owned", "build_class": "release"},
            {"cargo_package": "worker", "binary": "worker-owned", "build_class": "static"},
        ]
        for path in BUILD_SOURCES:
            with self.subTest(path=path.name):
                tree = ast.parse(path.read_text())
                loops = [node for node in ast.walk(tree) if isinstance(node, ast.For)
                         and isinstance(node.target, ast.Name)
                         and node.target.id == "build_class"]
                self.assertEqual(len(loops), 1)
                calls = []
                scope = dict(expected=expected, cargo="/declared/cargo", triple="host-triple",
                             base_env={"RUSTFLAGS": "declared-flags"}, root=Path("/admitted"),
                             targets={kind: Path("/private") / kind for kind in ("release", "static")},
                             processes=[], subprocess=SimpleNamespace(
                                 run=lambda argv, **kwargs: calls.append((argv, kwargs))))
                exec(compile(ast.Module(body=loops, type_ignores=[]), str(path), "exec"), scope)
                self.assertEqual(len(calls), 3)
                actual = {}
                for argv, kwargs in calls:
                    self.assertEqual(argv.count("-p"), 1)
                    package = argv[argv.index("-p") + 1]
                    actual[package] = [argv[i + 1] for i, arg in enumerate(argv) if arg == "--bin"]
                    for flag in ("--locked", "--frozen", "--offline"):
                        self.assertIn(flag, argv)
                    self.assertTrue(kwargs["check"])
                    self.assertEqual(kwargs["cwd"], Path("/admitted"))
                    self.assertEqual("+crt-static" in kwargs["env"]["RUSTFLAGS"], package == "worker")
                self.assertEqual(actual, {"shared": ["owned-a", "owned-b"],
                                          "other": ["other-owned"], "worker": ["worker-owned"]})
                self.assertEqual(len(scope["processes"]), 3)

    def test_release_source_authority_has_no_git_execution_dependency(self):
        forbidden = (
            "/usr/bin/git",
            'Command::new("git")',
            "git archive",
            "git show",
            "CleanGitSourceSnapshotAuthority",
        )
        for path in SOURCE_AUTHORITY_FILES:
            with self.subTest(path=path.relative_to(ROOT)):
                source = path.read_text()
                for needle in forbidden:
                    self.assertNotIn(needle, source)
        execution = SOURCE_AUTHORITY_FILES[1].read_text()
        self.assertIn("PinnedProjectMaterialization", execution)
        self.assertIn("authoritative_file_bounded", execution)
        handler = SOURCE_AUTHORITY_FILES[0].read_text()
        self.assertIn("ProjectSource::PushedHead", handler)
        self.assertIn("current pushed snapshot", handler)

    def test_every_release_yaml_rejects_duplicate_mapping_keys(self):
        for path in sorted(ASSET.rglob("*.yaml")):
            with self.subTest(path=path.relative_to(ASSET)):
                load_yaml(path)

    def test_python_tools_never_select_an_ambient_interpreter(self):
        for name, (owner_kind, owner_name) in PYTHON_TOOLS.items():
            with self.subTest(tool=name):
                tool = load_yaml(TOOLS / f"{name}.yaml")
                assert_realization_interpreter(self, tool)
                owner_path = (GRAPHS if owner_kind == "graph" else TOOLS) / f"{owner_name}.yaml"
                owner = load_yaml(owner_path)
                assert_slot(self, owner, "python", "python-gnu")
                producer = load_yaml(ROOT / ".ai/config/development/ryeos/gnu-python-production-inputs.yaml")
                runtime_root = producer["runtime"]["runtime_root"]
                self.assertEqual(runtime_root, "/ryeos/realizations/python-gnu/python")
                relative_path = tool["env_config"]["interpreter"]["relative_path"]
                self.assertTrue(("/ryeos/realizations/python-gnu/" + relative_path).startswith(runtime_root + "/"))

    def test_hard_coded_realization_paths_have_an_owning_root_slot(self):
        for name, (owner_kind, owner_name) in PYTHON_TOOLS.items():
            with self.subTest(tool=name):
                tool_path = TOOLS / f"{name}.yaml"
                source_path = TOOLS / "lib" / f"{name}.py"
                body = tool_path.read_text()
                if source_path.is_file():
                    body += "\n" + source_path.read_text()
                mounts = set(REALIZATION_PATH.findall(body))
                owner_path = (GRAPHS if owner_kind == "graph" else TOOLS) / f"{owner_name}.yaml"
                declared = set(slots_by_mount(load_yaml(owner_path)))
                self.assertEqual(mounts - declared, set())

    def test_builders_use_only_the_selected_platform_and_vendor(self):
        inherited = re.compile(
            r"os\.environ(?:\.get)?\(\s*['\"](" + "|".join(sorted(HOST_BUILD_VARIABLES)) + r")[\"']"
        )
        inherited_family = re.compile(
            r"os\.environ(?:\.get)?\(\s*['\"](?:LD_|PKG_CONFIG_)[A-Za-z0-9_]*[\"']"
        )
        for source_path in BUILD_SOURCES:
            with self.subTest(source=source_path.name):
                source = source_path.read_text()
                self.assertIsNone(inherited.search(source))
                self.assertIsNone(inherited_family.search(source))
                self.assertNotRegex(source, r"command\s*=\s*\[\s*[\"']cargo[\"']")
                self.assertIn("/ryeos/realizations/platform/rust/bin/cargo", source)
                calls = [node for node in ast.walk(ast.parse(source))
                         if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
                         and isinstance(node.func.value, ast.Name)
                         and node.func.value.id == "release_cargo"
                         and node.func.attr == "build_environment"]
                self.assertEqual(len(calls), 1)
                self.assertEqual([ast.unparse(arg) for arg in calls[0].args[:2]],
                                 ["private_root", "triple"])
                self.assertEqual(ast.literal_eval(calls[0].args[2]),
                                 "/ryeos/realizations/static-link-inputs")
                self.assertIn('"--frozen"', source)
                self.assertIn('"--offline"', source)
                self.assertIn("source may not provide Cargo configuration", source)
                self.assertNotIn("--dynamic-linker,", source)
                self.assertIn("release_elf.normalize_output_elf(destination", source)
                self.assertIn('"payload_transforms"', source)

        for graph_name in ("native-build", "core-seed-build"):
            with self.subTest(graph=graph_name):
                graph = load_yaml(GRAPHS / f"{graph_name}.yaml")
                assert_slot(self, graph, "python", "python-gnu")
                assert_slot(self, graph, "platform", "platform")
                assert_slot(self, graph, "cargo-vendor", "cargo-vendor")
                assert_slot(self, graph, "static-link-inputs", "static-link-inputs")

    def test_environment_relationships_bind_exact_root_consumers(self):
        document = load_yaml(RELATIONSHIPS)
        relationships = {
            relationship["name"]: relationship
            for relationship in document["product_relationships"]["relationships"]
        }
        expected = {
            "python_to_portable_build": ("graph:ryeos/bundle-release/portable-build", "python"),
            "python_to_native_build": ("graph:ryeos/bundle-release/native-build", "python"),
            "python_to_core_seed_build": ("graph:ryeos/bundle-release/core-seed-build", "python"),
            "python_to_signed_capture": ("graph:ryeos/bundle-release/signed-capture", "python"),
            "python_to_portable_signed_capture": ("graph:ryeos/bundle-release/portable-signed-capture", "python"),
            "python_to_core_seed_capture": ("graph:ryeos/bundle-release/core-seed-capture", "python"),
            "python_to_substrate_build": ("graph:ryeos/bundle-release/substrate-build", "python"),
            "python_to_substrate_qualify": ("tool:ryeos/bundle-release/substrate-qualify", "python"),
            "python_to_native_qualify": ("tool:ryeos/bundle-release/native-qualify", "python"),
            "python_to_portable_qualify": ("tool:ryeos/bundle-release/portable-qualify", "python"),
            "python_to_core_seed_qualify": ("tool:ryeos/bundle-release/core-seed-qualify", "python"),
            "platform_to_native_build": ("graph:ryeos/bundle-release/native-build", "platform"),
            "platform_to_core_seed_build": ("graph:ryeos/bundle-release/core-seed-build", "platform"),
            "cargo_vendor_to_native_build": ("graph:ryeos/bundle-release/native-build", "cargo-vendor"),
            "cargo_vendor_to_core_seed_build": ("graph:ryeos/bundle-release/core-seed-build", "cargo-vendor"),
            "static_link_inputs_to_native_build": ("graph:ryeos/bundle-release/native-build", "static-link-inputs"),
            "static_link_inputs_to_core_seed_build": ("graph:ryeos/bundle-release/core-seed-build", "static-link-inputs"),
        }
        self.assertEqual(set(relationships), set(expected))
        for name, (consumer_ref, declaration_id) in expected.items():
            with self.subTest(relationship=name):
                self.assertEqual(relationships[name]["consumer"], {
                    "canonical_ref": consumer_ref,
                    "declaration_id": declaration_id,
                })
                qualification = relationships[name]["qualification"]
                self.assertIsInstance(qualification["policy_ref"], str)
                self.assertTrue(qualification["required_claims"])

    def test_static_link_inputs_are_development_owned_and_bounded(self):
        relationships = load_yaml(RELATIONSHIPS)["product_relationships"]["relationships"]
        for relationship in relationships:
            if not relationship["name"].startswith("static_link_inputs_to_"):
                continue
            with self.subTest(relationship=relationship["name"]):
                self.assertEqual(relationship["producer"], {
                    "canonical_ref": "graph:ryeos/development/static-link-inputs-production",
                    "recipe_binding": "product_recipe",
                    "product_name": "static_link_inputs",
                    "parameters": {},
                })
                self.assertEqual(relationship["required_product"], {
                    "shape": "tree", "storage": "large_content",
                    "bounds": {"maximum_entries": 64, "maximum_depth": 8,
                               "maximum_file_bytes": 8388608, "maximum_total_bytes": 16777216},
                })
                self.assertEqual(relationship["qualification"], {
                    "policy_ref": "config:ryeos/environments/qualification/static-link-inputs",
                    "required_claims": ["static_link_inputs_x86_64_linux_gnu_v1"],
                })

    def test_calibration_requires_all_four_qualified_product_selections(self):
        graph = load_yaml(GRAPHS / "authority-calibrate.yaml")
        schema = graph["config"]["config_schema"]
        environment = schema["properties"]["execution_environment"]
        expected = {"python_runtime", "platform", "cargo_vendor", "static_link_inputs"}
        self.assertFalse(environment["additionalProperties"])
        self.assertEqual(set(environment["required"]), expected)
        self.assertEqual(set(environment["properties"]), expected)
        for selection in environment["properties"].values():
            self.assertEqual(selection, {"$ref": "#/$defs/product_selection"})
        self.assertEqual(set(schema["$defs"]["product_selection"]["required"]),
                         {"product_witness_hash", "qualification_attestation_hash"})

    def test_non_build_roots_do_not_receive_compiler_authority(self):
        for graph_name in ("portable-build", "signed-capture", "portable-signed-capture", "core-seed-capture", "substrate-build"):
            with self.subTest(graph=graph_name):
                slots = set(slots_by_mount(load_yaml(GRAPHS / f"{graph_name}.yaml")))
                self.assertNotIn("platform", slots)
                self.assertNotIn("cargo-vendor", slots)
                self.assertNotIn("static-link-inputs", slots)

    def test_portable_lane_never_aliases_native_release_relationships(self):
        build = load_yaml(ASSET / "config/bundle-release/portable-build-products.yaml")
        build_relationship = build["product_relationships"]["relationships"][0]
        self.assertEqual(build["build_products"]["products"][0]["name"], "portable_bundle")
        self.assertEqual(build_relationship["name"], "portable_bundle_to_signed_capture")
        self.assertEqual(
            build_relationship["consumer"]["canonical_ref"],
            "graph:ryeos/bundle-release/portable-signed-capture",
        )

        capture = load_yaml(GRAPHS / "portable-signed-capture.yaml")
        self.assertEqual(
            capture["product_recipe"],
            "config:bundle-release/portable-signed-capture-products",
        )
        capture_slots = slots_by_mount(capture)
        self.assertEqual(
            capture_slots["unsigned-native-bundle"]["relationship"],
            "portable_bundle_to_signed_capture",
        )

        capture_recipe = load_yaml(
            ASSET / "config/bundle-release/portable-signed-capture-products.yaml"
        )
        self.assertEqual(capture_recipe["recipe_purpose"], "bundle_release_v1")
        self.assertEqual(
            capture_recipe["build_products"]["products"][0]["name"],
            "signed_portable_bundle",
        )
        capture_relationship = capture_recipe["product_relationships"]["relationships"][0]
        self.assertEqual(
            capture_relationship["name"],
            "signed_portable_bundle_to_release_qualification",
        )
        self.assertEqual(
            capture_relationship["producer"]["canonical_ref"],
            "graph:ryeos/bundle-release/portable-signed-capture",
        )
        self.assertEqual(
            capture_relationship["consumer"]["canonical_ref"],
            "tool:ryeos/bundle-release/portable-qualify",
        )
        self.assertEqual(
            capture_relationship["qualification"]["policy_ref"],
            "config:bundle-release/portable-qualification",
        )

        portable_policy = load_yaml(
            ASSET / "config/bundle-release/portable-qualification.yaml"
        )["product_qualification_policy"]
        self.assertEqual(
            portable_policy["verifier_ref"],
            "tool:ryeos/bundle-release/portable-qualify",
        )
        self.assertEqual(
            set(capture_relationship["qualification"]["required_claims"]),
            set(portable_policy["allowed_claims"]),
        )

        for recipe_name, policy_name in (
            ("portable-signed-capture-products", "portable-qualification"),
            ("calibration-portable-capture-products", "portable-qualification"),
            ("calibration-native-capture-products", "native-qualification"),
        ):
            with self.subTest(recipe=recipe_name):
                recipe = load_yaml(ASSET / f"config/bundle-release/{recipe_name}.yaml")
                relationship = recipe["product_relationships"]["relationships"][0]
                policy_ref = relationship["qualification"]["policy_ref"]
                self.assertEqual(policy_ref, f"config:bundle-release/{policy_name}")
                policy = load_yaml(ASSET / f"config/bundle-release/{policy_name}.yaml")[
                    "product_qualification_policy"
                ]
                self.assertEqual(
                    policy["verifier_ref"],
                    relationship["consumer"]["canonical_ref"],
                )
                self.assertLessEqual(
                    set(relationship["qualification"]["required_claims"]),
                    set(policy["allowed_claims"]),
                )

        qualifier = load_yaml(TOOLS / "portable-qualify.yaml")
        qualifier_slots = slots_by_mount(qualifier)
        self.assertEqual(
            qualifier_slots["native-bundle"]["relationship"],
            "signed_portable_bundle_to_release_qualification",
        )
        self.assertEqual(
            qualifier_slots["native-bundle"]["relationship_ref"],
            "config:bundle-release/portable-signed-capture-products",
        )

        portable = load_yaml(GRAPHS / "portable-build.yaml")
        self.assertEqual(portable["product_recipe"], "config:bundle-release/portable-build-products")
        self.assertEqual(set(slots_by_mount(portable)), {"python-gnu"})
        for tool_name in ("native-qualify", "portable-qualify", "core-seed-qualify", "substrate-qualify"):
            with self.subTest(tool=tool_name):
                slots = set(slots_by_mount(load_yaml(TOOLS / f"{tool_name}.yaml")))
                self.assertNotIn("platform", slots)
                self.assertNotIn("cargo-vendor", slots)
                self.assertNotIn("static-link-inputs", slots)

    def test_validator_rejects_the_previous_ambient_python_shape(self):
        with self.assertRaises(AssertionError):
            assert_realization_interpreter(
                self,
                {"config": {"command": "/usr/bin/python3"}},
            )

    def test_validator_rejects_realization_without_owner_slot(self):
        with self.assertRaises(AssertionError):
            assert_slot(self, {"external_product_slots": []}, "python", "python")


if __name__ == "__main__":
    unittest.main()
