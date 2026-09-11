"""Source composition checks, not a claim of installed worker qualification."""

from pathlib import Path
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[3]


def load(relative):
    return yaml.safe_load((ROOT / relative).read_text())


class DevelopmentEnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.environment = load(".ai/config/development/ryeos/worker-environment.yaml")

    def test_development_profile_admits_the_verified_native_build_file_limit(self):
        profile = load("bundles/.ai/node/init/profiles/development.yaml")
        # Native utility production exercised this finite descriptor ceiling;
        # Zig's libc build exceeds the former 1024 limit before linking.
        self.assertEqual(profile["policies"]["isolation"]["policy"]["limits"]["open_files"],
                         4096)

    def test_graph_owned_product_inputs_fit_the_signed_kind_contract(self):
        kind = load("bundles/standard/.ai/node/engine/kinds/graph/graph.kind-schema.yaml")
        contract = kind["execution"]["external_content"]
        self.assertEqual(contract["max_declarations"], 4)
        for name in ("authoring-environment-production", "authoring-built-utilities-production",
                     "gnu-python-production"):
            graph = load(f".ai/graphs/ryeos/development/{name}.yaml")
            slots = graph["external_product_slots"]
            self.assertLessEqual(len(slots) + len(graph.get("external_content", [])),
                                 contract["max_declarations"])
            for slot in slots:
                self.assertIn(slot["mount_root"], contract["allowed_mount_roots"])
                recipe = load(".ai/config/" + slot["relationship_ref"].removeprefix("config:") + ".yaml")
                relationship = next(item for item in recipe["product_relationships"]["relationships"]
                                    if item["name"] == slot["relationship"])
                product = relationship["required_product"]
                if product["storage"] == "large_content":
                    self.assertLessEqual(product["bounds"]["maximum_total_bytes"],
                                         contract["large_content"]["max_total_bytes"])

    def test_root_composes_authoring_product_and_protocol_ingress_not_child_compiler(self):
        baseline = load("bundles/codex/.ai/config/codex/environments/authoring.yaml")
        self.assertEqual(self.environment["schema"], "ryeos.worker_environment.v6")
        self.assertEqual(self.environment["schema"], baseline["schema"])
        self.assertEqual(self.environment["worker_ref"], baseline["worker_ref"])
        declarations = {entry["id"]: entry for entry in self.environment["external_content"]}
        self.assertEqual(declarations, {})
        binding, = self.environment["workload_client"]["bindings"]
        self.assertEqual(binding, {"kind": "structured_session"})
        self.assertEqual(self.environment["configuration"]["executable_search"],
                         [{"realization_id": "authoring-tools", "relative_directory": "bin"}])
        slots = {entry["id"]: entry for entry in self.environment["external_product_slots"]}
        self.assertEqual(slots, {"authoring-tools": {
            "id": "authoring-tools",
            "relationship_ref": "config:development/ryeos/authoring-environment-products",
            "relationship": "runtime_to_authoring_worker",
            "kind": "tree", "mount_root": "execution_runtime", "mount": "authoring-tools",
        }})
        self.assertEqual(baseline["external_content"][0]["digest"],
                         "1ca7a7fe9ecc1d19c38c986ba9df435ff7bbe6d4ba020f2643564b8bba0c84e6")
        self.assertNotIn("client", self.environment["workload_client"])
        self.assertIsNone(baseline["workload_client"])

    def test_native_runtime_adoption_requires_the_independent_bundle_policy(self):
        products = load(".ai/config/development/ryeos/authoring-environment-products.yaml")
        relationships = {item["name"]: item for item in
                         products["product_relationships"]["relationships"]}
        verifier = relationships["runtime_to_native_authoring_qualification"]
        self.assertEqual(verifier["consumer"], {
            "canonical_ref": "tool:ryeos/environments/qualification/native-authoring/verify",
            "declaration_id": "authoring-runtime",
        })
        self.assertEqual(verifier["qualification"], {
            "policy_ref": None, "required_claims": []})
        worker = relationships["runtime_to_authoring_worker"]
        self.assertEqual(worker["consumer"], {
            "canonical_ref": "config:development/ryeos/worker-environment",
            "declaration_id": "authoring-tools",
        })
        self.assertEqual(worker["qualification"], {
            "policy_ref": "config:ryeos/environments/qualification/native-authoring",
            "required_claims": ["authoring_runtime_closed"],
        })
        self.assertNotIn("expected_manifest_hash", str(verifier) + str(worker))

    def test_child_grants_match_exact_existing_signed_operations(self):
        routes = self.environment["workload_client"]["executions"]
        refs = [route["item_ref"] for route in routes]
        self.assertEqual(refs, sorted(set(refs)))
        self.assertEqual(refs, ["tool:ryeos/development/" + operation for operation in (
            "cargo-build", "cargo-check", "cargo-test", "format-check", "format-file", "platform-inspect")])
        for route in routes:
            self.assertTrue(route["item_ref"].startswith("tool:ryeos/development/"))
            tool = load(".ai/tools/" + route["item_ref"].removeprefix("tool:") + ".yaml")
            self.assertEqual(route["workspace_access"], tool["workspace_access"])
            self.assertEqual(route["effect_classes"], [tool["effects"]])
            self.assertEqual(route["ref_bindings"], {})
            self.assertEqual(route["calls"], [{"kind": "default"}])
            self.assertEqual(tool["network_authority"], "isolated")
            self.assertEqual(tool["filesystem_authority"], "captured_execution")
            self.assertEqual(tool["execution_protocol"], "protocol:ryeos/core/opaque")

    def test_project_bounds_fit_explicit_development_node_ceiling(self):
        request = self.environment["workload_client"]
        policy = load("bundles/.ai/node/init/profiles/development.yaml")["policies"]["execution"]["workload_client"]
        self.assertEqual(request["protocol"], policy["protocol"])
        for bound in ("max_in_flight", "max_invocations_per_boot", "max_lifetime_seconds"):
            self.assertGreater(request[bound], 0)
            self.assertLessEqual(request[bound], policy[bound])
        self.assertLessEqual(len(request["executions"]), policy["max_executions"])

    def test_vendor_uses_exact_inputs_without_network_or_host_cargo(self):
        tool = load(".ai/tools/ryeos/development/cargo-vendor.yaml")
        self.assertEqual(tool["config"]["command"], "realization:platform/rust/bin/cargo")
        self.assertEqual(tool["filesystem_authority"], "captured_execution")
        self.assertEqual(tool["network_authority"], "isolated")
        self.assertEqual(tool["workspace_access"], "immutable_current_generation")
        self.assertFalse(tool["config_schema"]["additionalProperties"])
        self.assertEqual(tool["config_schema"]["properties"], {})
        declarations = {entry["id"]: entry for entry in tool["external_content"]}
        self.assertEqual(set(declarations), {"platform"})
        for declaration in declarations.values():
            self.assertEqual(declaration["mode"], "pinned")
            self.assertEqual(declaration["mount_root"], "execution_runtime")
            self.assertRegex(declaration["digest"], r"^[0-9a-f]{64}$")
        producer = load(".ai/graphs/ryeos/development/cargo-vendor-production.yaml")
        self.assertEqual(producer["external_product_slots"], [{
            "id": "registry-inputs",
            "relationship_ref": "config:development/ryeos/registry-products",
            "relationship": "registry_inputs_to_cargo_vendor",
            "kind": "tree",
            "mount_root": "execution_runtime",
            "mount": "registry-inputs",
        }])
        args = tool["config"]["args"]
        for required in ("--locked", "--frozen", "--offline", "--respect-source-config", "--versioned-dirs"):
            self.assertIn(required, args)
        self.assertEqual(args[-1], "products/cargo-vendor")
        self.assertEqual(tool["env_config"]["env"]["PATH"], "")
        self.assertEqual(tool["config"]["env"]["CARGO_HOME"], "/tmp/cargo")
        execution = load(".ai/config/execution/execution.yaml")
        self.assertEqual(execution["items"]["tool"]["ryeos/development/cargo-vendor"]["timeout"],
                         tool["config"]["timeout_secs"])
        # Provisioning is operator-driven, not silently added to root worker grants.
        self.assertNotIn("tool:ryeos/development/cargo-vendor",
                         [route["item_ref"] for route in self.environment["workload_client"]["executions"]])
        products = load(".ai/config/development/ryeos/cargo-vendor-products.yaml")
        self.assertEqual(
            {relationship["consumer"]["declaration_id"]
             for relationship in products["product_relationships"]["relationships"]},
            {"vendor"},
        )

    def test_development_closure_policy_covers_observed_vendor_artifact(self):
        policy = load("bundles/.ai/node/init/profiles/development.yaml")["policies"]["object_closure"]
        # Observed complete vendor output, not an engine/compiler special case.
        # The registered Rust compiler separately validates protocol and encoded
        # response bounds when this signed policy is installed.
        self.assertGreaterEqual(policy["max_total_blob_bytes"], 575879606)
        self.assertGreaterEqual(policy["max_blobs"], 25804)

    def test_platform_reproduction_is_an_offline_named_product(self):
        producer = load(".ai/graphs/ryeos/development/platform-production.yaml")
        products = load(".ai/config/development/ryeos/platform-products.yaml")
        self.assertEqual(producer["product_recipe"],
                         "config:development/ryeos/platform-products")
        self.assertEqual(products["build_products"]["products"][0]["name"], "platform")
        tool_text = (ROOT / ".ai/tools/ryeos/development/platform-production/assemble.py").read_text()
        self.assertIn("#   network_authority: isolated", tool_text)
        self.assertIn("#   filesystem_authority: captured_execution", tool_text)
        self.assertIn("digest: 98bceddd5b4024d5963eeac8c579e6d4e79c24577980fa9f88bce9ae3151d316",
                      tool_text)

    def test_producer_timeouts_survive_project_execution_config_precedence(self):
        execution = load(".ai/config/execution/execution.yaml")["items"]["tool"]
        runtime = load(".ai/tools/ryeos/development/authoring-environment-production/runtime.yaml")
        for operation in ("prepare", "assemble", "verify"):
            self.assertEqual(execution["ryeos/development/authoring-environment-production/" + operation]["timeout"],
                             runtime["config"]["timeout_secs"])

    def test_cargo_operations_are_bounded_offline_children_not_ambient_builds(self):
        execution = load(".ai/config/execution/execution.yaml")["items"]["tool"]
        linker_flags = []
        for operation in ("check", "build", "test"):
            ref = "ryeos/development/cargo-" + operation
            tool = load(".ai/tools/" + ref + ".yaml")
            self.assertEqual(tool["executor_id"], "@subprocess")
            self.assertEqual(tool["execution_protocol"], "protocol:ryeos/core/opaque")
            self.assertEqual(tool["workspace_access"], "immutable_current_generation")
            self.assertEqual(tool["filesystem_authority"], "captured_execution")
            self.assertEqual(tool["network_authority"], "isolated")
            declarations = {entry["id"]: entry for entry in tool["external_content"]}
            self.assertEqual(set(declarations), {"platform", "vendor"})
            self.assertEqual(declarations["vendor"]["digest"],
                             "8dac785a06faad238b3210c79ba3ca7bee37dc211fe0e95acf80dff6500a75ac")
            self.assertTrue(all(entry["mount_root"] == "execution_runtime"
                                and entry["mode"] == "pinned" for entry in declarations.values()))
            args = tool["config"]["args"]
            for flag in (operation, "--locked", "--frozen", "--offline", "--lib"):
                self.assertIn(flag, args)
            self.assertNotIn("--target", args)
            self.assertEqual(args[args.index("--jobs") + 1], "2")
            self.assertEqual(args[args.index("--package") + 1], "${params.package}")
            self.assertEqual(tool["config_schema"]["properties"]["package"]["enum"],
                             ["lillux", "ryeos-isolation-protocol"])
            self.assertFalse(tool["config_schema"]["additionalProperties"])
            self.assertEqual(tool["config"]["env"]["CARGO_TARGET_DIR"], "/tmp/target")
            self.assertEqual(tool["env_config"]["env"]["PATH"], "")
            flags = tool["config"]["env"]["RUSTFLAGS"]
            self.assertIn("-C link-arg=/ryeos/realizations/platform/lib/libc_nonshared.a", flags)
            linker_flags.append(flags)
            self.assertEqual(execution[ref]["timeout"], tool["config"]["timeout_secs"])
            if operation == "test":
                self.assertIn("--exact", args)
                self.assertIn("${params.test}", args)
                self.assertEqual(tool["config_schema"]["properties"]["test"]["minLength"], 1)
        self.assertEqual(len(set(linker_flags)), 1)
        # The independent bootstrap probe must exercise the same link input;
        # merely having the archive in an inventory does not prove it is used.
        probe = (ROOT / "tests/e2e/development-toolchain-stage0/probe.sh").read_text()
        self.assertIn("-C link-arg=$platform/lib/libc_nonshared.a", probe)

    def test_complete_distribution_has_its_own_nonexecutable_retention_consumer(self):
        distribution = load(".ai/config/development/ryeos/authoring-distribution.yaml")
        self.assertNotIn("executor_id", distribution)
        artifact = distribution["external_content"][0]
        self.assertEqual(artifact["id"], "distribution")
        self.assertEqual(artifact["digest"],
                         "31852861af3ec0384240611b8675ea600ae7d93b76cf6c5c1451276e0f7bd49c")
        self.assertNotIn(artifact["digest"],
                         [entry["digest"] for entry in self.environment["external_content"]])

    def test_production_graph_is_closed_sequential_and_fail_fast(self):
        prefix = "ryeos/development/authoring-environment-production"
        graph = load(".ai/graphs/" + prefix + ".yaml")
        config = graph["config"]
        self.assertEqual(config["start"], "assemble")
        self.assertEqual(config["max_steps"], 3)
        self.assertEqual(config["on_error"], "fail")
        self.assertEqual(config["config_schema"], {
            "type": "object", "properties": {}, "additionalProperties": False})
        self.assertEqual(set(config["nodes"]), {"assemble", "verify", "done"})
        self.assertEqual(graph["requires"]["capabilities"]["declared"], [
            "ryeos.execute.config.development/ryeos/authoring-environment-products",
            *["ryeos.execute.tool." + prefix + "/" + operation
              for operation in ("assemble", "verify")],
        ])
        for operation, successor in (("assemble", "verify"), ("verify", "done")):
            node = config["nodes"][operation]
            self.assertEqual(node["node_type"], "action")
            self.assertEqual(node["action"], {
                "item_id": "tool:" + prefix + "/" + operation,
                "ref_bindings": {}, "params": {}, "thread": "inline"})
            self.assertEqual(node["effects"], "live")
            self.assertFalse(node["cache_result"])
            for separate_or_recovering_flow in ("follow", "detach", "parallel", "retry", "on_error"):
                self.assertNotIn(separate_or_recovering_flow, node)
            self.assertEqual(node["next"], {"type": "unconditional", "to": successor})
        self.assertEqual(config["nodes"]["done"], {
            "node_type": "return", "output": {
                "assembly": "${state.assembly}", "verification": "${state.verification}"}})
        execution = load(".ai/config/execution/execution.yaml")["items"]
        self.assertGreater(execution["graph"][prefix]["timeout"], sum(
            execution["tool"][prefix + "/" + operation]["timeout"]
            for operation in ("assemble", "verify")))
        for manifest in ("manifest.source.yaml", "manifest.yaml"):
            self.assertIn("graph", load(".ai/" + manifest)["requires_kinds"])

    def test_production_inputs_use_exact_product_relationships_and_graph_slots(self):
        cases = [
            {
                "recipe": "authoring-prepared-input-products",
                "relationship": "prepared_inputs_to_environment_production",
                "producer": "graph:ryeos/development/authoring-prepared-inputs-production",
                "product": "prepared_inputs",
                "consumer": "graph:ryeos/development/authoring-environment-production",
                "declaration": "assembly-inputs",
                "consumer_graph": "authoring-environment-production",
                "mount": "authoring-inputs",
            },
            {
                "recipe": "authoring-build-support-products",
                "relationship": "build_support_to_utility_production",
                "producer": "graph:ryeos/development/authoring-build-support-production",
                "product": "build_support",
                "consumer": "graph:ryeos/development/authoring-built-utilities-production",
                "declaration": "authoring-build-support",
                "consumer_graph": "authoring-built-utilities-production",
                "mount": "authoring-build-support",
            },
            {
                "recipe": "authoring-built-utilities-products",
                "relationship": "built_utilities_to_environment_production",
                "producer": "graph:ryeos/development/authoring-built-utilities-production",
                "product": "built_utilities",
                "consumer": "graph:ryeos/development/authoring-environment-production",
                "declaration": "built-utilities",
                "consumer_graph": "authoring-environment-production",
                "mount": "authoring-built-utilities",
            },
        ]
        for case in cases:
            with self.subTest(case=case["relationship"]):
                recipe = load(f".ai/config/development/ryeos/{case['recipe']}.yaml")
                relationships = recipe["product_relationships"]
                self.assertEqual(relationships["schema"], "ryeos.product_relationships.v1")
                relationship = next(entry for entry in relationships["relationships"]
                                    if entry["name"] == case["relationship"])
                self.assertEqual(relationship["producer"], {
                    "canonical_ref": case["producer"], "recipe_binding": "product_recipe",
                    "product_name": case["product"], "parameters": {}})
                self.assertEqual(relationship["consumer"], {
                    "canonical_ref": case["consumer"],
                    "declaration_id": case["declaration"]})
                self.assertEqual(relationship["required_product"]["shape"], "tree")
                self.assertEqual(relationship["required_product"]["storage"], "large_content")
                self.assertEqual(relationship["qualification"], {
                    "policy_ref": None, "required_claims": []})
                graph = load(f".ai/graphs/ryeos/development/{case['consumer_graph']}.yaml")
                slot = next(entry for entry in graph["external_product_slots"]
                            if entry["id"] == case["declaration"])
                self.assertEqual(slot, {
                    "id": case["declaration"],
                    "relationship_ref": f"config:development/ryeos/{case['recipe']}",
                    "relationship": case["relationship"], "kind": "tree",
                    "mount_root": "execution_runtime", "mount": case["mount"]})

    def test_selected_inputs_replace_only_the_corresponding_literal_leaf_pins(self):
        owner = ROOT / ".ai/tools/ryeos/development/authoring-environment-production"
        for operation in ("assemble.py", "verify.py"):
            source = (owner / operation).read_text()
            self.assertNotIn("digest: cc090b3d53dd41c0", source)
            self.assertIn("id: producer-python", source)
            self.assertIn("assembly-inputs is selected by the enclosing producer Graph", source)
            self.assertIn("built-utilities is selected by that same Graph", source)
        utility = (owner / "build-utilities.py").read_text()
        self.assertNotIn("digest: f6bcd9d28b9bb3da", utility)
        for retained_pin in ("id: producer-python", "id: platform", "id: source-inputs"):
            self.assertIn(retained_pin, utility)
        self.assertIn("authoring-build-support is selected by the enclosing producer Graph", utility)

    def test_recorded_wrappers_forward_only_explicit_authenticated_root_selections(self):
        cases = {
            "authoring-built-utilities-production": [
                ("build_support_selection", "authoring-build-support")],
            "authoring-environment-production": [
                ("assembly_inputs_selection", "assembly-inputs"),
                ("built_utilities_selection", "built-utilities")],
            "gnu-python-production": [
                ("production_support_selection", "production-support")],
        }
        for producer, expected in cases.items():
            with self.subTest(producer=producer):
                wrapper = load(
                    f".ai/graphs/ryeos/development/{producer}-recorded.yaml")
                schema = wrapper["config"]["config_schema"]
                self.assertEqual(schema["required"], [name for name, _ in expected])
                for parameter, declaration in expected:
                    value = schema["properties"][parameter]
                    self.assertEqual(value["required"], [
                        "declaration_id", "witness_hash", "qualification_hash",
                        "witness_source"])
                    self.assertEqual(value["properties"]["declaration_id"]["enum"],
                                     [declaration])
                    self.assertEqual(value["properties"]["qualification_hash"]["type"],
                                     "null")
                    source_schema = value["properties"]["witness_source"]
                    self.assertEqual(source_schema, {"oneOf": [
                        {
                            "type": "object",
                            "required": ["kind"],
                            "properties": {"kind": {"const": "local_capture"}},
                            "additionalProperties": False,
                        },
                        {
                            "type": "object",
                            "required": ["kind", "acceptance_hash"],
                            "properties": {
                                "kind": {"const": "received"},
                                "acceptance_hash": {
                                    "type": "string", "pattern": "^[0-9a-f]{64}$"},
                            },
                            "additionalProperties": False,
                        },
                    ]})
                    self.assertFalse(value["additionalProperties"])
                action = wrapper["config"]["nodes"]["produce"]["action"]
                self.assertEqual(action["product_selections"], [
                    {"target": {"kind": "root"}, "selection": "${inputs." + parameter + "}"}
                    for parameter, _ in expected])
                self.assertNotIn("manifest_hash", str(action))
                self.assertNotIn("latest", str(action))

    def test_production_graph_leaves_use_same_exact_inputs_and_opaque_protocol(self):
        leaves = []
        for operation in ("assemble", "verify"):
            source = (ROOT / ".ai/tools/ryeos/development/authoring-environment-production" /
                      (operation + ".py")).read_text()
            header = "\n".join(line[2:] for line in source.splitlines()
                               if line.startswith("# ") and not line.startswith("# ryeos:signed:"))
            tool = yaml.safe_load(header)["ryeos-tool"]
            self.assertEqual(tool["execution_protocol"], "protocol:ryeos/core/opaque")
            self.assertEqual(tool["filesystem_authority"], "captured_execution")
            self.assertEqual(tool["network_authority"], "isolated")
            self.assertEqual(tool["config_schema"], {
                "type": "object", "properties": {}, "additionalProperties": False})
            leaves.append(tool)
        for field in ("executor_id", "external_content", "config_resolve"):
            self.assertEqual(leaves[0][field], leaves[1][field])

    def test_python_runtime_uses_admitted_prefix_not_protected_environment(self):
        runtime = load(".ai/tools/ryeos/development/authoring-environment-production/runtime.yaml")
        for environment in (runtime["config"]["env"], runtime["env_config"]["env"],
                            runtime["env_config"]["env_paths"]):
            self.assertNotIn("PYTHONHOME", environment)
            self.assertNotIn("PYTHONPATH", environment)
            self.assertNotIn("LANG", environment)
            self.assertNotIn("LC_ALL", environment)
        args = runtime["config"]["args"]
        for flag in ("-P", "-S", "-B"):
            self.assertIn(flag, args)
        self.assertEqual(args[args.index("-X") + 1], "utf8")
        bootstrap = next(arg["literal"] for arg in args if isinstance(arg, dict))
        self.assertIn('Path(sys.prefix) != Path("/ryeos/realizations/producer-python/python")', bootstrap)


if __name__ == "__main__":
    unittest.main()
