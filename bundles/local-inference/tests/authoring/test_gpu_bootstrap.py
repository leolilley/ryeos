"""Small synthetic GNU/driver bootstrap fixtures; no library or device loading."""

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[2] / "authoring/gpu_bootstrap.py"
SPEC = importlib.util.spec_from_file_location("gpu_bootstrap", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
GPU = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GPU)


class GpuBootstrapTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.gnu, self.musl, self.tools, self.driver = [self.root / name for name in ("gnu", "musl", "tools", "driver")]
        for root, members in (
            (self.gnu, ("python/bin/python3.14", "python/lib/ld-linux-x86-64.so.2", "python/lib/libc.so.6")),
            (self.musl, ("lib/ld-musl-x86_64.so.1",)),
            (self.tools, ("bin/clang", "bin/ld.lld", "lib/clang/20/lib/x86_64-alpine-linux-musl/libclang_rt.builtins-x86_64.a")),
            (self.driver, ("lib/libcuda.so.1",)),
        ):
            for member in members:
                path = root / member
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"synthetic member; never execute")
        self.layout = GPU.inspect_layout(self.gnu, self.musl, self.tools)

    def test_distinct_interpreter_and_compiler_loaders(self) -> None:
        self.assertEqual(self.layout["libc"], self.gnu / "python/lib/libc.so.6")
        for linker in (False, True):
            argv = GPU.compiler_prefix(self.layout, linker=linker)
            self.assertEqual(argv[0], str(self.musl / "lib/ld-musl-x86_64.so.1"))
            self.assertNotIn(str(self.layout["gnu_loader"]), argv)

    def test_missing_gnu_member_does_not_fall_back_to_musl(self) -> None:
        (self.gnu / "python/lib/libc.so.6").unlink()
        with self.assertRaises(FileNotFoundError):
            GPU.inspect_layout(self.gnu, self.musl, self.tools)

    def test_gnu_cannot_double_as_compiler_runtime(self) -> None:
        with self.assertRaisesRegex(ValueError, "separate roots"):
            GPU.inspect_layout(self.gnu, self.gnu, self.tools)

    def test_symlink_escape_and_wrong_member_type_refuse(self) -> None:
        member = self.driver / "lib/libcuda.so.1"
        member.unlink()
        member.symlink_to(self.gnu / "python/lib/libc.so.6")
        with self.assertRaisesRegex(ValueError, "escaped"):
            GPU.exact_library_resolver(self.layout["libc"], self.driver)
        member.unlink()
        member.mkdir()
        with self.assertRaisesRegex(ValueError, "wrong type"):
            GPU.exact_library_resolver(self.layout["libc"], self.driver)

    def test_resolver_ignores_ambient_library_and_backend_controls(self) -> None:
        resolver = GPU.exact_library_resolver(self.layout["libc"], self.driver)
        with mock.patch.dict(os.environ, {"CUDA_PATH": "/host/cuda", "LIBC_PATH": "/host/libc", "LD_LIBRARY_PATH": "/host", "DEV": "NV", "REGEN": "1"}):
            self.assertEqual(resolver("cuda", ["/host"], ["/host"]), str(self.driver / "lib/libcuda.so.1"))
            self.assertEqual(resolver("libc", ["/host"]), str(self.layout["libc"]))
            self.assertIsNone(resolver("nvrtc", ["/host"]))
            self.assertIsNone(resolver("nvjitlink", ["/host"]))
            with self.assertRaisesRegex(RuntimeError, "unselected library"):
                resolver("nvml", ["/host"])

    def test_project_relative_gnu_copy_cannot_be_launched(self) -> None:
        with self.assertRaisesRegex(ValueError, "fixed namespace"):
            GPU.launch_environment(self.layout, self.root, profile="qwen3-0.6b", session_fd=3)

    def test_environment_contains_only_proposed_controls(self) -> None:
        # Pure assembly fixture, not a real fixed-namespace mount or launch.
        layout = dict(self.layout, gnu_product=GPU.GNU_PRODUCT_ROOT)
        for name, member in (
            ("python_home", "python"), ("python", "python/bin/python3.14"),
            ("gnu_loader", "python/lib/ld-linux-x86-64.so.2"),
            ("gnu_lib", "python/lib"), ("libc", "python/lib/libc.so.6"),
        ):
            layout[name] = GPU.GNU_PRODUCT_ROOT / member
        for name in ("home", "cache", "tmp"):
            (self.root / name).mkdir()
        with mock.patch.dict(os.environ, {"LD_PRELOAD": "/host/evil", "MOCKGPU": "1", "DEVICE": "CPU", "REGEN": "1", "CUDA_PATH": "/host"}):
            env = GPU.launch_environment(layout, self.root, profile="qwen3-0.6b", session_fd=0)
        self.assertEqual(env["DEV"], "CUDA:PTX")
        self.assertEqual(env["RYEOS_SESSION_FD"], "0")
        self.assertEqual(env["PATH"], "")
        for name in ("LD_PRELOAD", "LD_LIBRARY_PATH", "MOCKGPU", "DEVICE", "REGEN", "CUDA_PATH"):
            self.assertNotIn(name, env)
        for descriptor in (True, -1, 1, 2, "3"):
            with self.assertRaisesRegex(ValueError, "descriptor"):
                GPU.launch_environment(layout, self.root, profile="qwen3-0.6b", session_fd=descriptor)
        layout["libc"] = self.layout["libc"]
        with self.assertRaisesRegex(ValueError, "launch members differ"):
            GPU.launch_environment(layout, self.root, profile="qwen3-0.6b", session_fd=3)

    def test_missing_driver_refuses_before_import(self) -> None:
        (self.driver / "lib/libcuda.so.1").unlink()
        with self.assertRaises(FileNotFoundError):
            GPU.exact_library_resolver(self.layout["libc"], self.driver)

    def _descriptor_probe(self, layout=None, member="probe.py"):
        source = self.root / "source"
        source.mkdir(exist_ok=True)
        (source / "probe.py").write_text("# selected source fixture; never run\n")
        for name in ("home", "cache", "tmp"):
            (self.root / name).mkdir(exist_ok=True)
        return GPU.descriptor_loader_probe(
            self.layout if layout is None else layout, source, member,
            self.root, profile="qwen3-0.6b", session_fd=3,
        )

    def test_descriptor_probe_uses_exact_loader_and_python_home(self) -> None:
        with mock.patch.dict(os.environ, {"LD_PRELOAD": "/ambient/evil", "PYTHONPATH": "/ambient"}):
            command, environment = self._descriptor_probe()
        self.assertEqual(command[:5], [str(self.layout["gnu_loader"]), "--inhibit-cache", "--library-path", str(self.layout["gnu_lib"]), str(self.layout["python"])])
        self.assertEqual(environment["PYTHONHOME"], str(self.gnu / "python"))
        self.assertNotIn("LD_PRELOAD", environment)
        self.assertNotIn("PYTHONPATH", environment)
        self.assertNotIn("-I", command)  # -I would ignore explicit PYTHONHOME.

    def test_descriptor_probe_refuses_missing_selected_loader(self) -> None:
        self.layout["gnu_loader"].unlink()
        with self.assertRaises(FileNotFoundError):
            self._descriptor_probe()

    def test_descriptor_probe_refuses_substituted_member(self) -> None:
        altered = dict(self.layout, python=self.layout["gnu_loader"])
        with self.assertRaisesRegex(ValueError, "member differs"):
            self._descriptor_probe(altered)

    def test_descriptor_probe_refuses_loader_path_expansion(self) -> None:
        for name in ("gnu_product", "compiler_lib", "toolchain_lib"):
            for character in (":", ";", "$", "\n"):
                altered = dict(self.layout)
                altered[name] = Path(str(altered[name]) + character + "ambient")
                with self.assertRaisesRegex(ValueError, "ambiguous"):
                    self._descriptor_probe(altered)

    def test_descriptor_probe_refuses_source_escape(self) -> None:
        for member in ("../probe.py", "/ambient/probe.py", ""):
            with self.assertRaisesRegex(ValueError, "source-relative"):
                self._descriptor_probe(member=member)

    def test_library_fence_rejects_preloaded_generated_bindings(self) -> None:
        with mock.patch.dict(sys.modules, {"tinygrad.runtime.autogen.cuda": object()}):
            with self.assertRaisesRegex(RuntimeError, "before the bootstrap fence"):
                GPU.install_exact_library_resolver(self.root, self.layout["libc"], self.driver)

    def test_library_fence_rejects_wrong_origin_and_prior_load(self) -> None:
        source = self.root / "tinygrad/runtime/support/c.py"
        source.parent.mkdir(parents=True)
        source.write_text("# synthetic module; not imported\n")
        support = SimpleNamespace(__file__=str(SCRIPT), DLL=SimpleNamespace(_loaded_=set()))
        with mock.patch.object(GPU.importlib, "import_module", return_value=support):
            with self.assertRaisesRegex(RuntimeError, "escaped selected source"):
                GPU.install_exact_library_resolver(self.root, self.layout["libc"], self.driver)
            support.__file__ = str(source)
            support.DLL._loaded_ = {"cuda"}
            with self.assertRaisesRegex(RuntimeError, "already loaded"):
                GPU.install_exact_library_resolver(self.root, self.layout["libc"], self.driver)


if __name__ == "__main__":
    unittest.main()
