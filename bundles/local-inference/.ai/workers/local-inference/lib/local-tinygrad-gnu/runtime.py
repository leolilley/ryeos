"""GNU product startup over the existing private worker workspace.

Layout checks are defense in depth, not ELF/dlopen qualification. The executor
must retain qualified inputs and the original scratch owner before release.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Mapping

from bootstrap_support import _member, _environment, inspect_layout, install_exact_library_resolver
from driver_closure import TreeBounds, inspect_cuda_driver_closure


# Exact ceiling from gnu_runtime_to_private_worker in the unsigned product draft.
# Actual retained GNU bounds must be supplied separately and remain within it.
GNU_RUNTIME_BOUNDS_CEILING = TreeBounds(10000, 64, 134217728, 268435456)


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


def _retained_tree_bounds(
    gnu_bounds: TreeBounds | Mapping[str, object] | None,
    driver_bounds: TreeBounds | Mapping[str, object] | None,
) -> tuple[TreeBounds, TreeBounds]:
    if gnu_bounds is None or driver_bounds is None:
        raise ValueError('missing D-retained GNU or CUDA driver product bounds')
    selected_gnu = gnu_bounds if isinstance(gnu_bounds, TreeBounds) else TreeBounds.from_contract(gnu_bounds)
    selected_driver = driver_bounds if isinstance(driver_bounds, TreeBounds) else TreeBounds.from_contract(driver_bounds)
    if any(
        selected > maximum for selected, maximum in zip(
            (selected_gnu.maximum_entries, selected_gnu.maximum_depth,
             selected_gnu.maximum_file_bytes, selected_gnu.maximum_total_bytes),
            (GNU_RUNTIME_BOUNDS_CEILING.maximum_entries, GNU_RUNTIME_BOUNDS_CEILING.maximum_depth,
             GNU_RUNTIME_BOUNDS_CEILING.maximum_file_bytes, GNU_RUNTIME_BOUNDS_CEILING.maximum_total_bytes),
            strict=True,
        )
    ):
        raise ValueError('retained GNU product bounds exceed its authored relationship ceiling')
    return selected_gnu, selected_driver


def _retained_device_uuid(value: object) -> str:
    if (not isinstance(value, str) or len(value) != 32
            or any(character not in '0123456789abcdef' for character in value)
            or value == '0' * 32):
        raise ValueError('missing D-retained canonical UUID-v2 comparison')
    return value


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


def prepare_environment(
    *,
    gnu_bounds: TreeBounds | Mapping[str, object] | None = None,
    driver_bounds: TreeBounds | Mapping[str, object] | None = None,
) -> tuple[Path, Path, Path, Path]:
    retained_gnu_bounds, retained_driver_bounds = _retained_tree_bounds(gnu_bounds, driver_bounds)
    workspace = Path.cwd().resolve(strict=True)
    worker = Path(__file__).resolve(strict=True).parent
    layout = validate_paths(workspace, worker)
    closure = inspect_cuda_driver_closure(
        layout['gnu_lib'], layout['driver'] / 'lib',
        gnu_bounds=retained_gnu_bounds, driver_bounds=retained_driver_bounds,
    )
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
    install_exact_library_resolver(
        layout["tinygrad"], layout["libc"], layout["driver"],
        selected_libraries=closure.providers,
    )
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


def run_product(
    *, observe=None, bindings=None, before_shutdown=None,
    gnu_bounds: TreeBounds | Mapping[str, object] | None = None,
    driver_bounds: TreeBounds | Mapping[str, object] | None = None,
    expected_uuid_v2_hex: str | None = None,
):
    # A Python callable is only the consumer interface, not authentication.
    # D must supply it through the admitted same-process private channel contract.
    if not callable(observe):
        raise RuntimeError("missing D-owned admitted private cleanup observation binding")
    retained_gnu_bounds, retained_driver_bounds = _retained_tree_bounds(gnu_bounds, driver_bounds)
    retained_uuid_v2_hex = _retained_device_uuid(expected_uuid_v2_hex)
    workspace, gnu, worker, tinygrad = prepare_environment(
        gnu_bounds=retained_gnu_bounds, driver_bounds=retained_driver_bounds
    )
    import importlib
    from cuda_lifecycle import install
    if bindings is None:
        bindings = importlib.import_module("tinygrad.runtime.autogen.cuda")
    verify_origins((gnu / "python", worker, tinygrad))
    lifecycle = install(
        selected_device=0, observe=observe, bindings=bindings,
        expected_uuid_v2_hex=retained_uuid_v2_hex,
        require_single_visible_device=True,
    )

    def load_session():
        # Keep this selected-product gate before compiler/session/model imports
        # without changing ordinary run_with_lifecycle callers.
        lifecycle.qualify_single_visible_device()
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
    # CleanupChannel's strict v2 init is the sole input source. These fields are
    # comparison data from D's original owner, never self-authenticating claims.
    return run_product(
        observe=channel.observe, before_shutdown=channel.authorize_shutdown,
        gnu_bounds=getattr(channel, 'gnu_bounds', None),
        driver_bounds=getattr(channel, 'driver_bounds', None),
        expected_uuid_v2_hex=getattr(channel, 'uuid_v2_hex', None),
    )
