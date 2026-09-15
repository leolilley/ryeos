"""Synthetic boundary tests, not a utility build or support qualification."""

import io
import json
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[3]
LIB = ROOT / ".ai/tools/ryeos/development/authoring-environment-production/lib"
sys.path.insert(0, str(LIB))
import utilities


def source_config():
    text = (ROOT / ".ai/config/development/ryeos/authoring-utility-sources.yaml").read_text()
    # The signed config's body is JSON, a YAML subset; no host YAML dependency.
    return json.loads("\n".join(line for line in text.splitlines() if not line.startswith("#")))


class UtilityTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="ryeos-utility-contract-test-")
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)

    def support(self, name="support"):
        root = self.root / name
        root.mkdir()
        commands = {name: f"bin/{name}" for name in utilities.REQUIRED_SUPPORT_COMMANDS}
        for member in set(commands.values()) | set(utilities.ELF_TOOLS.values()):
            path = root / member
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"\x7fELFsynthetic; must never execute")
            path.chmod(0o755)
        return root, {"inputs": utilities.input_inventory(root), "commands": commands, "notices": {}}

    def archive(self, members):
        path = self.root / "source.tar"
        with tarfile.open(path, "w") as output:
            for name, data in members:
                if isinstance(data, tarfile.TarInfo):
                    item, content = data, None
                    item.name = name
                else:
                    item, content = tarfile.TarInfo(name), io.BytesIO(data)
                    item.size = len(data)
                output.addfile(item, content)
        return path

    def platform(self):
        root = self.root / "platform"
        for member in ("zig/zig", "native/bin/ld.lld"):
            path = root / member
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"synthetic admitted compiler; never executed")
            path.chmod(0o755)
        return root

    def test_source_contract_preserves_finite_inputs_not_image_authority(self):
        config = source_config()
        self.assertEqual(set(utilities.validate_sources(config)), set(utilities.BUILD_ORDER) | {"zig"})
        self.assertNotIn("publisher_image", config)
        self.assertNotIn("artifact", config)
        config["input_image"] = "rust:latest"
        with self.assertRaisesRegex(ValueError, "immutable coordinate"):
            utilities.validate_sources(config)

    def test_malformed_source_and_missing_command_refuse(self):
        config = source_config()
        config["sources"][0] = None
        with self.assertRaisesRegex(ValueError, "source selection"):
            utilities.validate_sources(config)
        config = source_config()
        config["sources"][-1]["programs"] = {}
        with self.assertRaisesRegex(ValueError, "exact supported commands"):
            utilities.validate_sources(config)

    def test_shell_support_requires_exact_bytes_modes_and_complete_inventory(self):
        root, config = self.support("changed-bytes")
        utilities.checked_support(root, config)
        del config["commands"]["make"]
        with self.assertRaisesRegex(ValueError, "incomplete"):
            utilities.checked_support(root, config)
        config["commands"]["make"] = "bin/make"
        (root / "bin/sh").write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "member 'bin/sh' differs: bytes expected"):
            utilities.checked_support(root, config)

    def test_shell_support_inventory_refusal_identifies_first_exact_difference(self):
        root, config = self.support("inventory-changed-bytes")
        expected = config["inputs"]["bin/sh"]
        (root / "bin/sh").write_bytes(b"\x7fELFchanged")
        with self.assertRaisesRegex(
                ValueError,
                rf"member 'bin/sh' differs: bytes expected {expected['bytes']!r}, observed 11; "
                rf"sha256 expected '{expected['sha256']}', observed '[0-9a-f]{{64}}'"):
            utilities.checked_support(root, config)

        root, config = self.support("inventory-changed-mode")
        (root / "bin/sh").chmod(0o644)
        with self.assertRaisesRegex(
                ValueError, "member 'bin/sh' differs: mode expected 493, observed 420"):
            utilities.checked_support(root, config)

        root, config = self.support("inventory-missing-member")
        (root / "bin/sh").unlink()
        with self.assertRaisesRegex(ValueError, "missing exact member 'bin/sh'"):
            utilities.checked_support(root, config)

        root, config = self.support("inventory-unexpected-member")
        extra = root / "unexpected"
        extra.write_bytes(b"not admitted")
        extra.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "has unexpected member 'unexpected'"):
            utilities.checked_support(root, config)

    def test_shell_support_refuses_scripts_even_when_inventory_matches(self):
        root, config = self.support()
        (root / "bin/sh").write_bytes(b"#!/bin/sh\necho ambient\n")
        config["inputs"] = utilities.input_inventory(root)
        with self.assertRaisesRegex(ValueError, "never an ambient shebang"):
            utilities.checked_support(root, config)

    def test_extra_path_member_or_missing_elf_helper_refuses(self):
        root, config = self.support()
        (root / "bin/unselected").write_bytes(b"\x7fELFnot selected")
        config["inputs"] = utilities.input_inventory(root)
        with self.assertRaisesRegex(ValueError, "unselected"):
            utilities.checked_support(root, config)
        (root / "bin/unselected").unlink()
        (root / utilities.ELF_TOOLS["readelf"]).unlink()
        config["inputs"] = utilities.input_inventory(root)
        with self.assertRaisesRegex(ValueError, "ELF inspection"):
            utilities.checked_support(root, config)

    def test_environment_reuses_stage0_and_never_host_path(self):
        root, config = self.support()
        commands = utilities.checked_support(root, config)
        platform = self.platform()
        with patch.dict("os.environ", {"PATH": "/ambient/bin", "CC": "/host/cc", "MAKEFLAGS": "-j999"}):
            env = utilities.build_environment(self.root / "work", commands, platform)
        self.assertEqual(env["PATH"], str(root / "bin"))
        self.assertEqual(env["CONFIG_SHELL"], str(root / "bin/sh"))
        self.assertEqual(env["SHELL"], env["CONFIG_SHELL"])
        self.assertEqual(env["PKG_CONFIG"], str(root / "bin/false"))
        self.assertEqual(env["CC"], f"{platform}/zig/zig cc -target x86_64-linux-musl -static")
        self.assertEqual(env["LD"], f"{platform}/native/bin/ld.lld")
        self.assertNotIn("MAKEFLAGS", env)
        self.assertNotIn("RYEOS_AUTHORING_PUBLISHER_IMAGE", env)

    def test_readonly_support_matches_manifest_without_chmod_or_executable_relaxation(self):
        root, config = self.support()
        notice = root / "NOTICE"
        notice.write_bytes(b"support notice")
        notice.chmod(0o644)
        config["inputs"] = utilities.input_inventory(root)
        for member, identity in config["inputs"].items():
            (root / member).chmod(identity["mode"] & ~0o222)
        before = {member: (root / member).lstat().st_mode for member in config["inputs"]}
        utilities.checked_support(root, config)
        self.assertEqual(before, {member: (root / member).lstat().st_mode for member in config["inputs"]})
        (root / "bin/sh").chmod(0o444)
        with self.assertRaisesRegex(ValueError, "member 'bin/sh' differs: mode"):
            utilities.checked_support(root, config)

    def test_special_support_entry_refuses_before_reading_or_executing_it(self):
        root, config = self.support()
        member = root / "bin/sh"
        member.unlink()
        os.mkfifo(member)
        with self.assertRaisesRegex(ValueError, "link or special"):
            utilities.checked_support(root, config)

    def test_readonly_stage0_executable_is_not_relabelled(self):
        root, config = self.support()
        commands = utilities.checked_support(root, config)
        platform = self.platform()
        zig = platform / "zig/zig"
        zig.write_bytes(b"synthetic compiler; never executed")
        zig.chmod(0o555)
        before = zig.lstat().st_mode
        env = utilities.build_environment(self.root / "work", commands, platform)
        self.assertTrue(env["CC"].startswith(str(zig) + " cc "))
        self.assertEqual(zig.lstat().st_mode, before)
        zig.chmod(0o444)
        with self.assertRaisesRegex(ValueError, "not executable"):
            utilities.build_environment(self.root / "work", commands, platform)

    def test_missing_stage0_linker_refuses_instead_of_host_discovery(self):
        root, config = self.support()
        commands = utilities.checked_support(root, config)
        platform = self.platform()
        linker = platform / "native/bin/ld.lld"
        linker.chmod(0o444)
        with self.assertRaisesRegex(ValueError, "not executable"):
            utilities.build_environment(self.root / "work", commands, platform)
        linker.unlink()
        with self.assertRaises(FileNotFoundError):
            utilities.build_environment(self.root / "work", commands, platform)

    def test_all_build_recipes_select_shell_and_make_explicitly(self):
        sources = utilities.validate_sources(source_config())
        env = {"CONFIG_SHELL": "/selected/bin/sh", "MAKE": "/selected/bin/make",
               "CC": "/platform/zig/zig cc", "AR": "/platform/zig/zig ar",
               "CFLAGS": "-Os", "LDFLAGS": "-static"}
        for name in utilities.BUILD_ORDER:
            with self.subTest(name=name):
                commands = utilities.build_commands(name, sources[name], Path("/private/source"),
                                                     Path("/private/zlib"), env)
                self.assertTrue(all(command[0] in (env["CONFIG_SHELL"], env["MAKE"]) for command in commands))
                if name == "git":
                    self.assertIn("SHELL_PATH=/selected/bin/sh", commands[0])
                    self.assertIn('SHELL_PATH_CQ="/ryeos/realizations/authoring-tools/bin/zsh"', commands[0])
                for command in commands:
                    if command[0] == env["MAKE"]:
                        self.assertIn("SHELL=/selected/bin/sh", command)
                    else:
                        self.assertEqual(command[1], "/private/source/configure")

    def test_safe_internal_links_survive_source_extraction(self):
        link = tarfile.TarInfo()
        link.type, link.linkname = tarfile.SYMTYPE, "COPYING"
        path = self.archive([("pkg/COPYING", b"notice"), ("pkg/link", link)])
        extracted = utilities.extract_source(path, self.root / "extracted", "pkg")
        self.assertTrue((extracted / "link").is_symlink())
        self.assertEqual((extracted / "link").read_bytes(), b"notice")

    def test_archive_escape_and_expanded_budget_refuse(self):
        path = self.archive([("../outside", b"bad")])
        with self.assertRaisesRegex(ValueError, "extraction contract"):
            utilities.extract_source(path, self.root / "escape", "pkg")
        self.assertFalse((self.root / "outside").exists())
        path.unlink()
        path = self.archive([("pkg/file", b"too much")])
        with patch.object(utilities, "MAX_EXTRACTED_BYTES", 2):
            with self.assertRaisesRegex(ValueError, "expansion bound"):
                utilities.extract_source(path, self.root / "bounded", "pkg")

    def test_out_of_root_symlink_refuses(self):
        link = tarfile.TarInfo()
        link.type, link.linkname = tarfile.SYMTYPE, "../../outside"
        path = self.archive([("pkg/link", link)])
        with self.assertRaises(tarfile.FilterError):
            utilities.extract_source(path, self.root / "escape", "pkg")

    def test_log_limit_refuses_without_running_a_command(self):
        log = self.root / "build.log"
        log.write_bytes(b"x" * 16)
        with patch.object(utilities, "MAX_LOG_BYTES", 16), patch.object(utilities.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(ValueError, "diagnostic exceeds"):
                utilities.run(["/not/executed"], self.root, {}, log)
            spawn.assert_not_called()
        with self.assertRaisesRegex(ValueError, "exact executable"):
            utilities.run(["make"], self.root, {}, log)

    def test_running_diagnostic_limit_kills_only_the_owned_process(self):
        class Process:
            def __init__(self):
                self.stdout = io.BytesIO(b"x" * 128)
                self.killed = False

            def __enter__(self):
                return self

            def __exit__(self, *_):
                pass

            def poll(self):
                return -9 if self.killed else None

            def kill(self):
                self.killed = True

            def wait(self):
                return -9 if self.killed else 0

        process = Process()
        with patch.object(utilities.subprocess, "Popen", return_value=process) as spawn:
            with patch.object(utilities, "MAX_LOG_BYTES", 64):
                with self.assertRaisesRegex(ValueError, "diagnostic exceeds"):
                    utilities.run(["/selected/bin/make"], self.root, {"PATH": "/selected/bin"},
                                  self.root / "bounded.log")
            self.assertTrue(process.killed)
            self.assertNotIn("start_new_session", spawn.call_args.kwargs)
            self.assertNotIn("process_group", spawn.call_args.kwargs)
        self.assertLessEqual((self.root / "bounded.log").stat().st_size, 64)

    def test_failed_configure_retains_bounded_regular_config_log_head_and_tail(self):
        class FailedProcess:
            def __init__(self):
                self.stdout = io.BytesIO(b"compiler failed\n")

            def __enter__(self):
                return self

            def __exit__(self, *_):
                pass

            def poll(self):
                return 1

            def wait(self):
                return 1

        config_log = self.root / "config.log"
        config_log.write_bytes(b"exact-compiler-head" + b"x" * 256 + b"exact-linker-tail")
        retained = self.root / "retained.log"
        with patch.object(utilities.subprocess, "Popen", return_value=FailedProcess()), \
                patch.object(utilities, "MAX_LOG_BYTES", 256), \
                patch.object(utilities, "MAX_FAILURE_CONFIG_LOG_BYTES", 160):
            with self.assertRaisesRegex(ValueError, "utility build command failed"):
                utilities.run(["/selected/bin/configure"], self.root, {}, retained)
        evidence = retained.read_bytes()
        self.assertLessEqual(len(evidence), 256)
        self.assertIn(b"--- config.log bounded evidence ---", evidence)
        self.assertIn(b"exact-compiler-head", evidence)
        self.assertIn(b"--- config.log omitted middle ---", evidence)
        self.assertTrue(evidence.endswith(b"exact-linker-tail"))

        config_log.write_bytes(b"short complete diagnostic")
        retained = self.root / "short.log"
        with patch.object(utilities.subprocess, "Popen", return_value=FailedProcess()):
            with self.assertRaisesRegex(ValueError, "utility build command failed"):
                utilities.run(["/selected/bin/configure"], self.root, {}, retained)
        self.assertIn(b"short complete diagnostic", retained.read_bytes())

        config_log.unlink()
        secret = self.root / "outside"
        secret.write_bytes(b"must-not-be-retained")
        config_log.symlink_to(secret)
        retained = self.root / "symlink-refused.log"
        with patch.object(utilities.subprocess, "Popen", return_value=FailedProcess()):
            with self.assertRaisesRegex(ValueError, "utility build command failed"):
                utilities.run(["/selected/bin/configure"], self.root, {}, retained)
        self.assertNotIn(b"must-not-be-retained", retained.read_bytes())
        self.assertNotIn(b"config.log bounded evidence", retained.read_bytes())

    def test_compiler_or_support_mount_cannot_be_replaced_by_caller(self):
        with patch.object(utilities.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(ValueError, "exact admitted runtime roots"):
                utilities.build_utilities(source_config(), {}, self.root, self.root / "work",
                                          self.root / "output", platform=self.root / "host-platform")
            spawn.assert_not_called()
        self.assertFalse((self.root / "work").exists())
        self.assertFalse((self.root / "output").exists())

    def test_no_executable_tool_is_claimed_by_this_library(self):
        source = (LIB / "utilities.py").read_text()
        self.assertNotIn("# ryeos-tool:", source)
        self.assertNotIn('if __name__ == "__main__"', source)
        self.assertNotIn("scripts/release/authoring-baseline", source)
        self.assertNotIn('subprocess.run(["docker"', source)


if __name__ == "__main__":
    unittest.main()
