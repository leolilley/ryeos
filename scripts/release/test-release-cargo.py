#!/usr/bin/env python3
"""Focused declared release compiler environment checks; no compilation."""
import importlib.util
import os
import sys
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "release_cargo", ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib/release-cargo.py")
RECIPE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RECIPE)


class ReleaseCargoTests(unittest.TestCase):
    def test_static_sysroot_is_explicit_and_forwarded_to_linker(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "environment"
            mount = "/ryeos/realizations/static-link-inputs"
            env = RECIPE.build_environment(root, RECIPE.TARGET, mount)
            specs = (root / "linker/specs").read_text()
            self.assertIn("%{static|static-pie:--sysroot=" + mount + "}", specs)
            self.assertIn("--sysroot=%R", specs)
            self.assertIn(mount + "/usr/lib/x86_64-linux-gnu/", specs)
            self.assertEqual(env["LIBRARY_PATH"], RECIPE.PLATFORM + "/lib:" + mount + "/usr/lib/x86_64-linux-gnu")
            self.assertNotIn(mount, env["LD_LIBRARY_PATH"])
            self.assertEqual(env["RUSTFLAGS"], "")

    def test_rejects_ancestor_configuration(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "project"
            project.mkdir()
            RECIPE.validate_source_configuration(project)
            (root / ".cargo").mkdir()
            (root / ".cargo/config.toml").touch()
            with self.assertRaises(ValueError):
                RECIPE.validate_source_configuration(project)

    def test_private_scrubbed_environment_and_supported_specs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "environment"
            with patch.dict(os.environ, {"HOME": "/poison", "RUSTFLAGS": "poison",
                                         "LD_PRELOAD": "poison", "RUSTC_WRAPPER": "poison"}):
                env = RECIPE.build_environment(root, RECIPE.TARGET)
            self.assertFalse(any("poison" in value for value in env.values()))
            self.assertNotIn("LD_PRELOAD", env)
            self.assertNotIn("RUSTC_WRAPPER", env)
            for name in ("HOME", "CARGO_HOME", "TMPDIR", "ZIG_LOCAL_CACHE_DIR", "ZIG_GLOBAL_CACHE_DIR"):
                self.assertTrue(Path(env[name]).is_dir())
                self.assertTrue(Path(env[name]).is_relative_to(root))
            self.assertNotEqual(env["HOME"], env["CARGO_HOME"])
            self.assertEqual(env["RUSTFLAGS"], "")
            self.assertEqual(env["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER"],
                             RECIPE.PLATFORM + "/native/bin/gcc")
            self.assertEqual(env["CC"], RECIPE.PLATFORM + "/zig/zig cc -target x86_64-linux-gnu")
            self.assertEqual(env["CFLAGS"], "--target=x86_64-linux-gnu")
            self.assertEqual(env["CXXFLAGS"], "--target=x86_64-linux-gnu")
            self.assertNotIn("CRATE_CC_NO_DEFAULTS", env)
            specs = (root / "linker/specs").read_text()
            for part in ("*self_spec:", "*linker:", "*startfile_prefix_spec:",
                         "libc_nonshared.a", "%{!static:", "%{!static-pie:"):
                self.assertIn(part, specs)
            self.assertNotIn("rpath", specs)
            self.assertNotIn("nodefaultlib", specs)
            self.assertEqual((root / "linker/specs").stat().st_mode & 0o111, 0)

    def test_rejects_other_target_before_writing(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "environment"
            with self.assertRaises(ValueError):
                RECIPE.build_environment(root, "aarch64-unknown-linux-gnu")
            self.assertFalse(root.exists())

    def test_rejects_reused_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "environment"
            RECIPE.build_environment(root, RECIPE.TARGET)
            with self.assertRaises(FileExistsError):
                RECIPE.build_environment(root, RECIPE.TARGET)


if __name__ == "__main__":
    unittest.main()
