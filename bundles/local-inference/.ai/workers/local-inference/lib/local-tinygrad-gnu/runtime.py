"""GNU product startup over the existing private worker workspace.

Layout checks are defense in depth, not ELF/dlopen qualification. The executor
must retain qualified inputs and the original scratch owner before release.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path

from bootstrap_support import _member, _environment, inspect_layout, install_exact_library_resolver


def validate_paths(workspace: Path, worker: Path) -> dict[str, Path]:
    workspace = workspace.resolve(strict=True)
    worker = worker.resolve(strict=True)
    if workspace not in worker.parents:
        raise RuntimeError("GNU worker source escaped admitted workspace")
    roots = {name: _member(workspace, name, directory=True) for name in
             ("python-gnu", "runtime", "toolchain", "tinygrad", "model", "driver", "scratch")}
    layout = dict(inspect_layout(roots["python-gnu"], roots["runtime"], roots["toolchain"]))
    layout.update(roots)
    layout["worker"] = worker
    _member(roots["tinygrad"], "tinygrad/runtime/support/c.py")
    _member(roots["driver"], "lib/libcuda.so.1")
    _member(roots["python-gnu"], "python/lib/libgcc_s.so.1")
    _member(roots["python-gnu"], "python/lib/python3.14/lib-dynload", directory=True)
    for name in ("home", "cache", "tmp"):
        _member(roots["scratch"], name, directory=True)
    return layout


def closed_environment(layout: dict[str, Path], supplied: dict[str, str]) -> dict[str, str]:
    if supplied.get("DEV") != "CUDA:PTX" or supplied.get("JIT") != "0" or supplied.get("PROFILE") != "0":
        raise RuntimeError("GNU worker requires CUDA:PTX, JIT0 and PROFILE0")
    if supplied.get("RYEOS_LOCAL_MODEL_PROFILE") != "qwen3-0.6b":
        raise RuntimeError("GNU compatibility worker requires exact Qwen3-0.6B profile")
    channel = supplied.get("RYEOS_SESSION_FD", "")
    if not channel.isascii() or not channel.isdecimal() or str(int(channel)) != channel or not 0 <= int(channel) <= 2147483647:
        raise RuntimeError("persistent descriptor is not canonical")
    env = _environment(layout, layout["scratch"], profile="qwen3-0.6b", session_fd=int(channel))
    env.update(JIT="0", PROFILE="0")
    return env


def verify_origins(roots: tuple[Path, ...]) -> None:
    for name, module in list(sys.modules.items()):
        origin = getattr(module, "__file__", None)
        if origin is None:
            continue
        path = Path(origin).resolve(strict=True)
        if not any(path == root or root in path.parents for root in roots):
            raise RuntimeError(f"GNU module {name!r} escaped selected closure")


def prepare_environment() -> tuple[Path, Path, Path, Path]:
    workspace = Path.cwd().resolve(strict=True)
    worker = Path(__file__).resolve(strict=True).parent
    layout = validate_paths(workspace, worker)
    environment = closed_environment(layout, dict(os.environ))
    if Path(sys.prefix).resolve(strict=True) != layout["python_home"]:
        raise RuntimeError("GNU interpreter prefix differs from selected product")
    if not sys.flags.safe_path or sys.flags.hash_randomization != 0 or not sys.flags.no_site:
        raise RuntimeError("GNU interpreter startup controls were not honored")
    if hash("ryeos-local-worker") != -7902905501708591707:
        raise RuntimeError("GNU interpreter hash seed differs from selected launch")
    stdlib = layout["python_home"] / "lib/python3.14"
    sys.path[:] = [str(worker), str(layout["tinygrad"]), str(stdlib), str(stdlib / "lib-dynload")]
    os.environ.clear()
    os.environ.update(environment)
    verify_origins((worker, layout["python_home"]))
    install_exact_library_resolver(layout["tinygrad"], layout["libc"], layout["driver"])
    return workspace, layout["python-gnu"], worker, layout["tinygrad"]


def run_with_lifecycle(lifecycle, load_session, before_shutdown=None):
    """Connect the same session owner; cleanup also covers import/constructor failure."""
    try:
        with lifecycle.submission():
            session = load_session()
        try:
            return session.main(lifecycle)
        except getattr(session, "SessionInputClosed", ()):
            # D closes the request stream before authorized teardown. The
            # finally block must still authorize and prove cooperative close.
            return 0
    finally:
        try:
            if before_shutdown is not None:
                before_shutdown()
        finally:
            outcome = lifecycle.shutdown()
        if outcome.status != "closed" or not outcome.observation_delivered:
            raise RuntimeError("CUDA normal shutdown evidence unavailable; same owner remains uncertain")


def run_product(*, observe=None, bindings=None, before_shutdown=None):
    # A Python callable is only the consumer interface, not authentication.
    # D must supply it through the admitted same-process private channel contract.
    if not callable(observe):
        raise RuntimeError("missing D-owned admitted private cleanup observation binding")
    workspace, gnu, worker, tinygrad = prepare_environment()
    import importlib
    from cuda_lifecycle import install
    lifecycle = install(selected_device=0, observe=observe, bindings=bindings)

    def load_session():
        from compiler import install_admitted_compiler
        install_admitted_compiler(workspace)
        from cpu_support import install_cpu_support
        install_cpu_support(gnu, tinygrad)
        session = importlib.import_module("session")
        verify_origins((gnu / "python", worker, tinygrad))
        return session

    return run_with_lifecycle(lifecycle, load_session, before_shutdown)


def run_channel_product():
    from cleanup_channel import CleanupChannel, descriptor
    cleanup_fd = descriptor(os.environ.get("RYEOS_RESOURCE_CLEANUP_FD"))
    session_text = os.environ.get("RYEOS_SESSION_FD", "")
    if not session_text.isascii() or not session_text.isdecimal() or str(int(session_text)) != session_text or not 0 <= int(session_text) <= 2147483647 or int(session_text) in (1, 2):
        raise RuntimeError("persistent descriptor is not canonical")
    channel = CleanupChannel(cleanup_fd, int(session_text))
    return run_product(observe=channel.observe, before_shutdown=channel.authorize_shutdown)
