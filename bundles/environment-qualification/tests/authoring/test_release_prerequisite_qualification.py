# ryeos:signed:2026-10-16T09:33:55Z:9f0d399548c9d277dcf58f3c45aadbd2dad091dbeb16c3e546f80981b453017d:W4o6sbiV8f9szONLzmqDHmFG0sdYos84sLlc6zVuo4c1ote8U7SyWQS/dwO+AyHXMnsR+mf3cS7pgXaQaem6Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
from __future__ import annotations

import hashlib
import copy
import importlib.util
import json
import os
import sys
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import yaml

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[4]
TOOLS = ROOT / "bundles/environment-qualification/.ai/tools/ryeos/environments/qualification"
POLICIES = ROOT / "bundles/environment-qualification/.ai/config/ryeos/environments/qualification"
DEVELOPMENT = ROOT / ".ai/config/development/ryeos"
PRODUCER = "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf"
# Synthetic realization identity for verifier contract tests. Live platform
# identity is selected by the project product relationship, not this fixture.
VERIFIER_PLATFORM = "2" * 64


def load_tool(name):
    path = TOOLS / f"{name}.py"
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    header = []
    for line in path.read_text().splitlines():
        if not line.startswith("#"):
            break
        if line.startswith("# ryeos:signed:"):
            continue
        if line.startswith("# "):
            header.append(line[2:])
    return module, yaml.safe_load("\n".join(header))["ryeos-tool"]


def sealed(subject_hash="1" * 64):
    base = {"kind": "tree", "mode": "pinned", "entry_count": 1,
            "total_bytes": 1, "mount_root": "execution_runtime"}
    return json.dumps([
        {**base, "id": "producer-python", "manifest_hash": PRODUCER, "mount": "producer-python"},
        {**base, "id": "verifier-platform", "manifest_hash": VERIFIER_PLATFORM,
         "mount": "platform"},
        {**base, "id": "subject", "manifest_hash": subject_hash, "mount": "cargo-vendor"},
    ])


class ReleasePrerequisiteQualificationTests(unittest.TestCase):
    def test_platform_policy_and_relationships_require_typed_claims(self):
        module, tool = load_tool("development-platform")
        self.assertIn("native/bin/ar", module.REQUIRED_EXECUTABLES)
        self.assertIn("native/bin/collect2", module.REQUIRED_EXECUTABLES)
        self.assertIn("zig/zig", module.REQUIRED_EXECUTABLES)
        self.assertEqual(tool["executor_id"], "@subprocess")
        self.assertEqual(tool["env_config"]["interpreter"]["realization_id"],
                         "producer-python")
        self.assertEqual(tool["env_config"]["interpreter"]["relative_path"],
                         "lib/ld-musl-x86_64.so.1")
        self.assertEqual(tool["config"]["args"][:3], [
            "--library-path", "/ryeos/realizations/producer-python/lib",
            "/ryeos/realizations/producer-python/python/bin/python3.14",
        ])
        self.assertEqual(tool["external_content"][0]["digest"], PRODUCER)
        slot, = tool["external_product_slots"]
        self.assertEqual(slot["relationship"], "platform_to_qualification_verifier")
        policy = yaml.safe_load((POLICIES / "development-platform.yaml").read_text())[
            "product_qualification_policy"]
        claims = ["development_platform_profile_v2",
                  "development_platform_target_x86_64_linux_gnu_v1"]
        self.assertEqual(policy["allowed_claims"], claims)
        self.assertEqual(list(module.QUALIFICATION_CLAIMS), claims)
        products = yaml.safe_load((DEVELOPMENT / "platform-products.yaml").read_text())
        relationships = products["product_relationships"]["relationships"]
        verifier = next(item for item in relationships
                        if item["name"] == "platform_to_qualification_verifier")
        self.assertIsNone(verifier["qualification"]["policy_ref"])
        for relationship in relationships:
            if relationship is verifier:
                continue
            self.assertEqual(relationship["qualification"], {
                "policy_ref": "config:ryeos/environments/qualification/development-platform",
                "required_claims": claims,
            })
        with tempfile.NamedTemporaryFile() as stream:
            stream.write(b"\x7fELF\x02\x01\x01" + b"\0" * 11 + (62).to_bytes(2, "little") + b"\0" * 44)
            stream.flush()
            self.assertEqual(module.elf_identity(Path(stream.name))["machine"], "x86_64")

    def test_platform_gcc_identity_matches_retained_invocation_exactly(self):
        module, _ = load_tool("development-platform")
        expected = "gcc (Debian 14.2.0-19) 14.2.0"
        for value in (expected, "gcc (Debian 14.2.0-20) 14.2.0",
                      "gcc (Debian 14.2.0-19) 14.3.0",
                      "x86_64-linux-gnu-gcc-14 (Debian 14.2.0-19) 14.2.0"):
            with self.subTest(identity=value), mock.patch.object(
                    module, "run_program", return_value=value + "\nCopyright\n") as probe:
                if value == expected:
                    self.assertEqual(module.gcc_identity(expected), expected)
                else:
                    with self.assertRaisesRegex(ValueError, "native compiler identity changed"):
                        module.gcc_identity(expected)
                probe.assert_called_once_with(module.ROOT / "native/bin/gcc", "--version")

    def test_platform_collect2_probe_selects_only_retained_linker(self):
        module, _ = load_tool("development-platform")
        for exit_code in (0, 1):
            def run(command, **kwargs):
                self.assertEqual(command, [str(module.ROOT / "lib/ld-linux-x86-64.so.2"),
                    "--library-path", str(module.ROOT / "lib"),
                    str(module.ROOT / "native/bin/collect2"), "-fuse-ld=lld", "--version"])
                self.assertEqual(kwargs["env"], {"LANG": "C", "LC_ALL": "C", "PATH": "",
                    "COMPILER_PATH": str(module.ROOT / "native/bin")})
                return module.subprocess.CompletedProcess(command, exit_code,
                    b"collect2 version 14.2.0\nretained linker version\n")

            with mock.patch.dict(os.environ, {"COMPILER_PATH": "/poison/compiler",
                                             "PATH": "/poison/path", "LD_PRELOAD": "/poison/library"}), \
                    mock.patch.object(module.subprocess, "run", side_effect=run) as probe:
                if exit_code:
                    with self.assertRaisesRegex(ValueError, "identity probe failed"):
                        module.collect2_identity()
                else:
                    self.assertEqual(module.collect2_identity(), "collect2 version 14.2.0")
                self.assertEqual(probe.call_count, 1)
        self.assertEqual(module.PROBE_ENVIRONMENT, {"LANG": "C", "LC_ALL": "C", "PATH": ""})

    def test_platform_cargo_probe_owns_state_and_cleans_up_on_success_or_failure(self):
        module, _ = load_tool("development-platform")
        roots = []
        for exit_code in (0, 101):
            def run(command, **kwargs):
                scratch = Path(kwargs["cwd"])
                roots.append(scratch)
                environment = kwargs["env"]
                for variable in ("HOME", "CARGO_HOME"):
                    state = Path(environment[variable])
                    self.assertEqual(state.parent, scratch)
                    self.assertTrue(state.is_dir())
                    self.assertEqual(list(state.iterdir()), [])
                self.assertNotEqual(environment["HOME"], environment["CARGO_HOME"])
                self.assertEqual(environment["PATH"], "")
                self.assertEqual(environment["CARGO_NET_OFFLINE"], "true")
                self.assertNotIn("RUSTC_WRAPPER", environment)
                self.assertNotIn("LD_PRELOAD", environment)
                self.assertEqual(command[-2:], [str(module.ROOT / "rust/bin/cargo"), "--version"])
                return module.subprocess.CompletedProcess(command, exit_code, b"cargo probe\n")

            with mock.patch.dict(os.environ, {"HOME": "/poison/home",
                                             "CARGO_HOME": "/poison/cargo",
                                             "RUSTC_WRAPPER": "/poison/wrapper",
                                             "LD_PRELOAD": "/poison/library"}), \
                    mock.patch.object(module.subprocess, "run", side_effect=run):
                if exit_code:
                    with self.assertRaisesRegex(ValueError, "failed \\(101\\)"):
                        module.cargo_identity()
                else:
                    self.assertEqual(module.cargo_identity(), "cargo probe")
            self.assertFalse(roots[-1].exists())
        self.assertNotEqual(roots[0], roots[1])
        with mock.patch.object(module.subprocess, "run", return_value=
                               module.subprocess.CompletedProcess([], 0, b"identity")) as run:
            module.run_program(module.ROOT / "rust/bin/rustc", "-vV")
            self.assertNotIn("HOME", run.call_args.kwargs["env"])
            self.assertNotIn("CARGO_HOME", run.call_args.kwargs["env"])

    def test_platform_dynamic_closure_rejects_ambient_and_escaping_search(self):
        module, _ = load_tool("development-platform")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            library = root / "lib"
            library.mkdir()
            (library / "libc.so.6").write_bytes(b"libc")
            base = {"interpreter": str(library / "ld-linux-x86-64.so.2"),
                    "needed": ["libc.so.6"], "search": [str(library)]}
            module.validate_dynamic_closure(root, "bin/tool", base, [library])
            for search in ("/tmp", "$ORIGIN/..", str(root / "lib/../..")):
                bad = {**base, "search": [search]}
                with self.subTest(search=search), self.assertRaises(ValueError):
                    module.validate_dynamic_closure(root, "bin/tool", bad, [library])
            missing = {**base, "needed": ["libmissing.so"]}
            with self.assertRaisesRegex(ValueError, "absent or ambiguous"):
                module.validate_dynamic_closure(root, "bin/tool", missing, [library])

    def test_platform_inventory_uses_manifest_entry_and_byte_semantics(self):
        module, _ = load_tool("development-platform")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in module.EXPECTED_TOP_LEVEL:
                path = root / name
                if name in {"lib", "native", "rust", "share", "sysroot", "zig"}:
                    path.mkdir()
                else:
                    path.write_bytes(name.encode())
            payload = root / "rust/tool"
            payload.write_bytes(b"payload")
            payload.chmod(0o500)
            entries = len(module.EXPECTED_TOP_LEVEL) + 1
            total = sum(path.stat().st_size for path in root.rglob("*") if path.is_file())
            result = module.inventory(root, {"entry_count": entries, "total_bytes": total})
            self.assertEqual(len(result), entries)
            self.assertEqual(sum(item["bytes"] for item in result), total)
            self.assertEqual(next(item for item in result if item["path"] == "rust")["kind"], "dir")
            tool = next(item for item in result if item["path"] == "rust/tool")
            self.assertEqual(tool["kind"], "file")
            self.assertEqual(tool["mode"], 0o755)
            with self.assertRaisesRegex(ValueError, "metrics contradict"):
                module.inventory(root, {"entry_count": entries - 1, "total_bytes": total})

    def test_platform_non_elf_probe_does_not_read_the_whole_member(self):
        module, _ = load_tool("development-platform")
        with tempfile.NamedTemporaryFile() as stream:
            stream.write(b"ordinary data" * 1024)
            stream.flush()
            with mock.patch.object(module, "regular", side_effect=AssertionError("full read")):
                self.assertIsNone(module.elf_dynamic_contract(Path(stream.name)))

    def test_platform_elf_parser_accepts_section_only_objects(self):
        module, _ = load_tool("development-platform")
        header = bytearray(64)
        header[:7] = b"\x7fELF\x02\x01\x01"
        header[18:20] = (62).to_bytes(2, "little")
        with tempfile.NamedTemporaryFile() as stream:
            stream.write(header)
            stream.flush()
            with mock.patch.object(module, "ROOT", Path(stream.name).parent):
                self.assertEqual(module.elf_dynamic_contract(Path(stream.name)), {
                    "path": Path(stream.name).name,
                    "interpreter": None,
                    "needed": [],
                    "search": [],
                })

    def test_platform_retained_testimony_must_match_observed_closure(self):
        module, _ = load_tool("development-platform")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            expected = yaml.safe_load((POLICIES / "development-platform.yaml").read_text())[
                "product_qualification_policy"]["verifier_parameters"]["bootstrap"]
            bootstrap = "".join(f"{key}={value}\n" for key, value in expected.items())
            (root / "RYEOS-BOOTSTRAP").write_text(bootstrap)
            entries = [
                # The CAS materializer may normalize directory permissions;
                # source testimony still has to record the safe canonical mode.
                {"path": "native", "kind": "dir", "bytes": 0, "mode": 0o700},
                {"path": "native/tool", "kind": "file", "bytes": 1, "mode": 0o755,
                 "sha256": "a" * 64},
                {"path": "RYEOS-TREE-SHA256", "kind": "file", "bytes": 1, "mode": 0o644,
                 "sha256": "b" * 64},
            ]
            (root / "RYEOS-TREE-SHA256").write_text(
                f'd\t755\t-\tnative\nf\t755\t{"a" * 64}\tnative/tool\n')
            (root / "RYEOS-RUNTIME-DEPENDENCIES").write_text(
                f'{"a" * 64}\tnative/tool\telf\t-\tlibc.so.6\n')
            contracts = [{"path": "native/tool", "interpreter": None,
                          "needed": ["libc.so.6"], "search": []}]
            with mock.patch.object(module, "ROOT", root):
                module.validate_bootstrap_testimony(expected)
                module.validate_tree_testimony(entries)
                module.validate_runtime_testimony(entries, contracts)
                (root / "RYEOS-BOOTSTRAP").write_text(bootstrap.replace("zig_version=0.15.2", "zig_version=0"))
                with self.assertRaisesRegex(ValueError, "bootstrap testimony changed"):
                    module.validate_bootstrap_testimony(expected)

    def test_platform_signed_provenance_profile_rejects_swaps(self):
        module, tool = load_tool("development-platform")
        profile = yaml.safe_load((POLICIES / "development-platform.yaml").read_text())[
            "product_qualification_policy"]["verifier_parameters"]
        original = module.expected_bootstrap(profile)
        changed = copy.deepcopy(profile)
        changed["bootstrap"]["producer_sha256"] = "a" * 64
        alternate = module.expected_bootstrap(changed)
        self.assertEqual(tool["external_content"][0]["digest"], PRODUCER)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            testimony = root / "RYEOS-BOOTSTRAP"
            testimony.write_text("".join(f"{key}={value}\n" for key, value in original.items()))
            with mock.patch.object(module, "ROOT", root):
                module.validate_bootstrap_testimony(original)
                with self.assertRaisesRegex(ValueError, "bootstrap testimony changed"):
                    module.validate_bootstrap_testimony(alternate)
                testimony.write_text("".join(f"{key}={value}\n" for key, value in alternate.items()))
                module.validate_bootstrap_testimony(alternate)
                with self.assertRaisesRegex(ValueError, "bootstrap testimony changed"):
                    module.validate_bootstrap_testimony(original)
        unsupported = copy.deepcopy(profile)
        unsupported["bootstrap"]["rust_version"] = "1.96.0-beta"
        with self.assertRaisesRegex(ValueError, "unsupported"):
            module.expected_bootstrap(unsupported)

    def test_platform_version_profiles_drive_real_probe_processes_and_reject_swaps(self):
        module, _ = load_tool("development-platform")
        original = yaml.safe_load((POLICIES / "development-platform.yaml").read_text())[
            "product_qualification_policy"]["verifier_parameters"]
        alternate = copy.deepcopy(original)
        alternate["bootstrap"].update(rust_version="1.96.0", zig_version="0.16.0")
        alternate["tool_versions"] = {
            "cargo": "cargo 1.96.0 (123456789 2026-05-01)",
            "gcc": "gcc (Debian 14.2.0-20) 14.2.0",
            "collect2": "collect2 version 14.3.0",
            "linker": "LLD 22.1.3 (compatible with GNU linkers)",
            "archiver": "GNU ar (GNU Binutils for Debian) 2.45",
        }
        # Synthetic executable probes exercise the actual subprocess path. They
        # establish profile interpretation, not qualification of real toolchains.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def script(relative, body):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(f"#!{sys.executable}\n" + body)
                path.chmod(0o755)

            script("lib/ld-linux-x86-64.so.2", "import os, sys\nos.execv(sys.argv[3], sys.argv[3:])\n")

            def install(profile):
                bootstrap, versions = profile["bootstrap"], profile["tool_versions"]
                outputs = {
                    "rust/bin/rustc": (f'release: {bootstrap["rust_version"]}\n'
                                       f'host: {bootstrap["rust_host"]}\ncommit-hash: synthetic'),
                    "rust/bin/cargo": versions["cargo"],
                    "native/bin/gcc": versions["gcc"],
                    "native/bin/collect2": versions["collect2"],
                    "native/bin/ld.lld": versions["linker"],
                    "native/bin/ar": versions["archiver"],
                    "zig/zig": bootstrap["zig_version"],
                }
                for relative, output in outputs.items():
                    script(relative, f"print({output!r})\n")

            def probe(profile):
                bootstrap = module.expected_bootstrap(profile)
                return module.probe_compilers(bootstrap, profile["tool_versions"])[1]

            with mock.patch.object(module, "ROOT", root):
                for selected, rejected in ((original, alternate), (alternate, original)):
                    install(selected)
                    observed = probe(selected)
                    self.assertEqual(observed["release"], selected["bootstrap"]["rust_version"])
                    self.assertEqual(observed["zig"], selected["bootstrap"]["zig_version"])
                    for field, value in selected["tool_versions"].items():
                        self.assertEqual(observed[field], value)
                    with self.assertRaisesRegex(ValueError, "target or release changed"):
                        probe(rejected)
                    # Every output remains independently exact, including
                    # patch/build changes that share the same release prefix.
                    for field in selected["tool_versions"]:
                        changed = copy.deepcopy(selected)
                        changed["tool_versions"][field] += "-different"
                        with self.subTest(field=field), self.assertRaisesRegex(ValueError, "identity changed"):
                            probe(changed)
                    changed = copy.deepcopy(selected)
                    changed["bootstrap"]["zig_version"] = "0.99.0"
                    with self.assertRaisesRegex(ValueError, "Zig identity changed"):
                        probe(changed)

    def test_platform_profile_refuses_unbounded_or_incoherent_expectations(self):
        module, _ = load_tool("development-platform")
        original = yaml.safe_load((POLICIES / "development-platform.yaml").read_text())[
            "product_qualification_policy"]["verifier_parameters"]
        for mutate in (
                lambda value: value.update(schema="ryeos.development_platform_profile.v1"),
                lambda value: value["tool_versions"].pop("gcc"),
                lambda value: value["tool_versions"].update(extra="unexpected"),
                lambda value: value["tool_versions"].update(gcc="gcc\nextra"),
                lambda value: value["tool_versions"].update(gcc="x" * 513),
                lambda value: value["tool_versions"].update(cargo="cargo 1.95.00 (different)"),
                lambda value: value["bootstrap"].update(target="aarch64-unknown-linux-gnu"),
        ):
            profile = copy.deepcopy(original)
            mutate(profile)
            with self.assertRaises(ValueError):
                module.expected_bootstrap(profile)

    def test_gnu_python_fixed_zlib_proposition_is_not_profile_selected(self):
        policy = yaml.safe_load((POLICIES / "gnu-python.yaml").read_text())[
            "product_qualification_policy"]
        tool = yaml.safe_load((TOOLS / "gnu-python.yaml").read_text())
        scripts = [arg["literal"] for arg in tool["config"]["args"] if isinstance(arg, dict)]
        self.assertEqual(policy["verifier_parameters"], {})
        self.assertIn("gnu_python_zlib_1_3_2", policy["allowed_claims"])
        self.assertIn('zlib.ZLIB_RUNTIME_VERSION != "1.3.2"', scripts[1])
        self.assertIn('"gnu_python_zlib_1_3_2"', scripts[0])
        self.assertIn('"gnu_python_zlib_1_3_2"', scripts[1])
        self.assertNotIn("${params_json}", str(tool["config"]))

    def test_vendor_verifier_proves_lock_checksum_and_file_closure(self):
        module, tool = load_tool("cargo-vendor")
        self.assertEqual(tool["external_content"][0]["digest"], PRODUCER)
        self.assertEqual(tool["env_config"]["interpreter"]["relative_path"],
                         "lib/ld-musl-x86_64.so.1")
        self.assertEqual(tool["config"]["args"][:3], [
            "--library-path", "/ryeos/realizations/producer-python/lib",
            "/ryeos/realizations/producer-python/python/bin/python3.14",
        ])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = root / "demo-1.2.3"
            package.mkdir()
            content = b"exact source\n"
            (package / "src.rs").write_bytes(content)
            package_checksum = "2" * 64
            (package / ".cargo-checksum.json").write_text(json.dumps({
                "package": package_checksum,
                "files": {"src.rs": hashlib.sha256(content).hexdigest()},
            }))
            (root / "Cargo.lock").write_text(
                'version = 4\n\n[[package]]\nname = "demo"\nversion = "1.2.3"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                f'checksum = "{package_checksum}"\n')
            with mock.patch.object(module, "ROOT", root), \
                    mock.patch.object(module, "probe_offline_cargo", return_value="3" * 64), \
                    mock.patch.dict(os.environ, {"RYEOS_EXTERNAL_REALIZATIONS": sealed()}, clear=True):
                result = module.execute({})
            self.assertEqual(result["claims"], ["cargo_vendor_lock_closure_v1",
                                                 "cargo_vendor_offline_checksums_v1"])
            self.assertTrue(result["probe_evidence"]["offline_complete"])
            (package / "src.rs").write_bytes(b"moved")
            with mock.patch.object(module, "ROOT", root), \
                    mock.patch.object(module, "probe_offline_cargo", return_value="3" * 64), \
                    mock.patch.dict(os.environ, {"RYEOS_EXTERNAL_REALIZATIONS": sealed()}, clear=True):
                with self.assertRaisesRegex(ValueError, "differ from .cargo-checksum"):
                    module.execute({})

    def test_vendor_policy_relationships_and_self_verification_are_closed(self):
        module, tool = load_tool("cargo-vendor")
        policy = yaml.safe_load((POLICIES / "cargo-vendor.yaml").read_text())[
            "product_qualification_policy"]
        claims = ["cargo_vendor_lock_closure_v1", "cargo_vendor_offline_checksums_v1"]
        self.assertEqual(policy["allowed_claims"], claims)
        products = yaml.safe_load((DEVELOPMENT / "cargo-vendor-products.yaml").read_text())
        relationships = products["product_relationships"]["relationships"]
        verifier = next(item for item in relationships
                        if item["name"] == "cargo_vendor_to_qualification_verifier")
        self.assertIsNone(verifier["qualification"]["policy_ref"])
        for relationship in relationships:
            if relationship is verifier:
                continue
            self.assertEqual(relationship["qualification"]["required_claims"], claims)
        value = json.loads(sealed(PRODUCER))
        with mock.patch.dict(os.environ, {"RYEOS_EXTERNAL_REALIZATIONS": json.dumps(value)}, clear=True):
            with self.assertRaisesRegex(ValueError, "may not qualify itself"):
                module.realizations()


if __name__ == "__main__":
    unittest.main()
