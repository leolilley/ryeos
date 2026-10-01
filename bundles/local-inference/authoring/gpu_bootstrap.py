"""Finite GNU / CUDA:PTX bootstrap preparation, outside admitted worker source.

These helpers neither select products nor grant devices or execution. A future
separate GPU worker must receive qualified, immutable roots from RyeOS, preserve
the GNU product's fixed namespace, and qualify ELF providers and actual loading.
The descriptor-loader probe below investigates a separate consumer relationship;
it cannot inherit the fixed-namespace runtime's qualification. The existing
signed CPU worker and its musl compiler implementation are unchanged.
"""

from __future__ import annotations

import importlib
import os
from pathlib import Path
from types import MappingProxyType
from typing import Callable, Mapping


GNU_PRODUCT_ROOT = Path("/ryeos/realizations/python-gnu")
BACKEND = "CUDA:PTX"
TINYGRAD_REVISION = "4c206a52b1a72a98db8c97576959b54fa2a38232"
UNUSED_CUDA_PTX_LIBRARIES = frozenset({"nvrtc", "nvjitlink"})


def _member(root: Path, relative: str, *, directory: bool = False) -> Path:
    root = root.resolve(strict=True)
    path = (root / relative).resolve(strict=True)
    if root not in path.parents:
        raise ValueError(f"bootstrap member escaped its selected root: {relative}")
    if not (path.is_dir() if directory else path.is_file()):
        raise ValueError(f"bootstrap member has the wrong type: {relative}")
    return path


def inspect_layout(
    gnu_product: Path, compiler_runtime: Path, toolchain: Path
) -> Mapping[str, Path]:
    """Check staged member layout only; do not execute or claim ABI compatibility."""
    gnu_product = gnu_product.resolve(strict=True)
    compiler_runtime = compiler_runtime.resolve(strict=True)
    toolchain = toolchain.resolve(strict=True)
    roots = (gnu_product, compiler_runtime, toolchain)
    if any(a == b or a in b.parents or b in a.parents
           for i, a in enumerate(roots) for b in roots[i + 1:]):
        raise ValueError("GNU, compiler runtime and toolchain require separate roots")
    return MappingProxyType({
        "gnu_product": gnu_product,
        "python_home": _member(gnu_product, "python", directory=True),
        "python": _member(gnu_product, "python/bin/python3.14"),
        "gnu_loader": _member(gnu_product, "python/lib/ld-linux-x86-64.so.2"),
        "gnu_lib": _member(gnu_product, "python/lib", directory=True),
        "libc": _member(gnu_product, "python/lib/libc.so.6"),
        "compiler_loader": _member(compiler_runtime, "lib/ld-musl-x86_64.so.1"),
        "compiler_lib": _member(compiler_runtime, "lib", directory=True),
        "clang": _member(toolchain, "bin/clang"),
        "linker": _member(toolchain, "bin/ld.lld"),
        "toolchain_lib": _member(toolchain, "lib", directory=True),
        "builtins": _member(toolchain, "lib/clang/20/lib/x86_64-alpine-linux-musl/"
                                       "libclang_rt.builtins-x86_64.a"),
    })


def exact_library_resolver(
    libc: Path, driver_root: Path
) -> Callable[..., str | None]:
    """Bind only the selected GNU libc and driver entry; never scan host paths.

    CUDA:PTX uses driver PTX JIT. Imported but unused NVRTC/nvJitLink DLLs
    remain unbound, so attempting their functions fails rather than finding
    host software. Driver transitive libraries still need ELF qualification.
    """
    libc = libc.resolve(strict=True)
    if not libc.is_file():
        raise ValueError("selected GNU libc is not a regular file")
    driver = _member(driver_root, "lib/libcuda.so.1")
    selected = MappingProxyType({"libc": str(libc), "cuda": str(driver)})

    def resolve(name: str, _paths: object, _extra_paths: object = ()) -> str | None:
        if name in selected:
            return selected[name]
        if name in UNUSED_CUDA_PTX_LIBRARIES:
            return None
        raise RuntimeError(f"CUDA:PTX requested an unselected library: {name}")

    return resolve


def install_exact_library_resolver(
    tinygrad_root: Path, libc: Path, driver_root: Path
) -> None:
    """Fence tinygrad DLL discovery before any generated bindings are imported."""
    import sys

    forbidden = (
        "tinygrad.runtime.autogen.libc", "tinygrad.runtime.autogen.cuda",
        "tinygrad.runtime.autogen.nvrtc", "tinygrad.runtime.autogen.nvjitlink",
    )
    if any(name in sys.modules for name in forbidden):
        raise RuntimeError("tinygrad library binding occurred before the bootstrap fence")
    expected = _member(tinygrad_root, "tinygrad/runtime/support/c.py")
    resolver = exact_library_resolver(libc, driver_root)
    support = importlib.import_module("tinygrad.runtime.support.c")
    if Path(support.__file__).resolve(strict=True) != expected:
        raise RuntimeError("tinygrad library support escaped selected source")
    if support.DLL._loaded_:
        raise RuntimeError("tinygrad already loaded libraries before the bootstrap fence")
    support.DLL.findlib = staticmethod(resolver)


def launch_environment(
    layout: Mapping[str, Path], scratch: Path, *, profile: str, session_fd: int
) -> dict[str, str]:
    """Construct a closed proposal, never merge ambient loader/backend controls.

    The fixed namespace check deliberately refuses staged project-relative GNU
    copies. This is preparation, not proof that the selected session can mount
    this namespace or that the returned environment is admitted.
    """
    if layout["gnu_product"] != GNU_PRODUCT_ROOT:
        raise ValueError("GNU product requires its qualified fixed namespace")
    for name, member in (
        ("python_home", "python"), ("python", "python/bin/python3.14"),
        ("gnu_loader", "python/lib/ld-linux-x86-64.so.2"),
        ("gnu_lib", "python/lib"), ("libc", "python/lib/libc.so.6"),
    ):
        if layout[name] != GNU_PRODUCT_ROOT / member:
            raise ValueError("GNU launch members differ from the qualified fixed namespace")
    return _environment(layout, scratch, profile=profile, session_fd=session_fd)


def _environment(
    layout: Mapping[str, Path], scratch: Path, *, profile: str, session_fd: int
) -> dict[str, str]:
    if profile not in {"qwen3-0.6b", "qwen3-4b"}:
        raise ValueError("unselected Qwen compatibility profile")
    if (isinstance(session_fd, bool) or not isinstance(session_fd, int)
            or session_fd < 0 or session_fd in (1, 2)):
        raise ValueError("persistent channel descriptor is invalid")
    scratch = scratch.resolve(strict=True)
    private = {name: _member(scratch, name, directory=True)
               for name in ("home", "cache", "tmp")}
    return {
        "HOME": str(private["home"]), "XDG_CACHE_HOME": str(private["cache"]),
        "TMPDIR": str(private["tmp"]), "PYTHONHOME": str(layout["python_home"]),
        "LIBC_PATH": str(layout["libc"]), "PATH": "", "DEV": BACKEND,
        "RYEOS_LOCAL_MODEL_PROFILE": profile, "RYEOS_SESSION_FD": str(session_fd),
        "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONHASHSEED": "0", "PYTHONSAFEPATH": "1", "PYTHONUNBUFFERED": "1",
        "CACHELEVEL": "0", "CCACHE": "0", "LANG": "C", "LC_ALL": "C",
    }


def descriptor_loader_probe(
    layout: Mapping[str, Path], source_root: Path, probe_member: str,
    scratch: Path, *, profile: str, session_fd: int,
) -> tuple[list[str], dict[str, str]]:
    """Assemble a feasibility probe; do not launch, admit or claim closed loading.

    RyeOS must first supply exact retained immutable descriptor roots. Explicit
    loader invocation bypasses PT_INTERP for this process only. --library-path
    and --inhibit-cache alone do not suppress every object's RUNPATH or dlopen
    dependency. A new independent consumer qualifier must check mapped bytes,
    required imports, missing-provider refusal and actual compiler children.
    This helper neither verifies an ELF inventory nor enables a GPU worker.
    """
    for name in ("gnu_product", "compiler_lib", "toolchain_lib"):
        path = layout[name]
        if not path.is_absolute() or any(character in str(path) for character in (":", ";", "$", "\x00", "\n")):
            raise ValueError("descriptor root is ambiguous to the loader")
    for name, member in (
        ("python_home", "python"), ("python", "python/bin/python3.14"),
        ("gnu_loader", "python/lib/ld-linux-x86-64.so.2"),
        ("gnu_lib", "python/lib"), ("libc", "python/lib/libc.so.6"),
    ):
        expected = _member(layout["gnu_product"], member, directory=name in ("python_home", "gnu_lib"))
        if layout[name] != expected:
            raise ValueError("descriptor launch member differs from its selected root")
    if (not isinstance(probe_member, str) or not probe_member
            or Path(probe_member).is_absolute() or ".." in Path(probe_member).parts):
        raise ValueError("probe requires a selected source-relative member")
    probe = _member(source_root, probe_member)
    environment = _environment(layout, scratch, profile=profile, session_fd=session_fd)
    return [
        str(layout["gnu_loader"]), "--inhibit-cache", "--library-path",
        str(layout["gnu_lib"]), str(layout["python"]), "-P", "-S", "-B",
        "-X", "utf8", str(probe),
    ], environment


def compiler_prefix(layout: Mapping[str, Path], *, linker: bool = False) -> tuple[str, ...]:
    """Use the separately selected musl loader for either exact compiler tool."""
    return (
        str(layout["compiler_loader"]), "--library-path",
        os.pathsep.join((str(layout["toolchain_lib"]), str(layout["compiler_lib"]))),
        str(layout["linker" if linker else "clang"]),
    )
