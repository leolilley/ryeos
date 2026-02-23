# rye:signed:2026-02-23T00:43:13Z:42fc89569e4472755c503e196900248c50ce6a933582c9d3244ba1468255f0c0:8RZcHHtdSBqNslOGLPYuBOZgv5F0i6UeTPZHeDPRw7vvIbjzW8rqYNFP-8CTat5lGpLmbZBKuThvfP5xNwxcDg==:9fbfabe975fa5a7f
"""Runtime library — shared module loader for Python-based tools.

Handles the Python-specific concern of registering packages in
sys.modules so `from .config_loader import X` works across
subdirectories. Injected into PYTHONPATH by the anchor system
in PrimitiveExecutor.

Signature verification is handled upstream by PrimitiveExecutor via
the runtime YAML verify_deps config (language-agnostic, data-driven).
"""

__tool_type__ = "python"
__version__ = "1.0.0"
__category__ = "rye/core/runtimes/python/lib"
__tool_description__ = "Python module loader for anchor imports"

import importlib.util
import logging
import sys
from pathlib import Path
from typing import Optional, Union

logger = logging.getLogger(__name__)


def load_module(
    relative_path: Union[str, Path],
    *,
    anchor: Optional[Path] = None,
):
    """Load a Python module with proper package context for relative imports.

    Dependency verification is handled upstream by PrimitiveExecutor via
    the runtime YAML's verify_deps config — not here. This function is
    purely a module loader with sys.modules registration.

    Args:
        relative_path: Path relative to anchor, or absolute path.
                       Extension is optional (defaults to .py).
        anchor: Directory to resolve relative paths from.
                Required if relative_path is not absolute.

    Returns:
        Loaded Python module object.

    Raises:
        FileNotFoundError: If module file doesn't exist.
    """
    # Resolve path
    if isinstance(relative_path, str):
        relative_path = Path(relative_path)

    if relative_path.is_absolute():
        module_path = relative_path
    else:
        if anchor is None:
            raise ValueError("anchor is required for relative paths")
        module_path = anchor / relative_path

    if not module_path.suffix:
        module_path = module_path.with_suffix(".py")

    module_path = module_path.resolve()

    if not module_path.exists():
        raise FileNotFoundError(f"Module not found: {module_path}")

    # Build a qualified module name that preserves package context
    # so relative imports (from .config_loader import X) work inside
    # the loaded module.
    resolved_anchor = anchor.resolve() if anchor else module_path.parent.resolve()

    # Determine relative position from anchor
    try:
        rel = module_path.relative_to(resolved_anchor)
    except ValueError:
        rel = Path(module_path.name)

    parts = list(rel.with_suffix("").parts)

    # Build package name prefix based on anchor directory name
    anchor_name = resolved_anchor.name
    pkg_prefix = f"_rye_{anchor_name}"

    if len(parts) > 1:
        # Module is in a subdirectory — need to register parent packages
        pkg_parts = parts[:-1]
        module_base = parts[-1]

        for i in range(len(pkg_parts)):
            pkg_name = ".".join([pkg_prefix] + pkg_parts[: i + 1])
            if pkg_name not in sys.modules:
                pkg_dir = resolved_anchor / Path(*pkg_parts[: i + 1])
                init_path = pkg_dir / "__init__.py"
                if init_path.exists():
                    pkg_spec = importlib.util.spec_from_file_location(
                        pkg_name, init_path,
                        submodule_search_locations=[str(pkg_dir)],
                    )
                    pkg_mod = importlib.util.module_from_spec(pkg_spec)
                    pkg_mod.__path__ = [str(pkg_dir)]
                    pkg_mod.__package__ = pkg_name
                    sys.modules[pkg_name] = pkg_mod
                    try:
                        pkg_spec.loader.exec_module(pkg_mod)
                    except Exception:
                        pass  # Empty __init__.py is fine
                else:
                    # Create a namespace package
                    pkg_mod = type(sys)(pkg_name)
                    pkg_mod.__path__ = [str(pkg_dir)]
                    pkg_mod.__package__ = pkg_name
                    sys.modules[pkg_name] = pkg_mod

        # Register the anchor as root package
        if pkg_prefix not in sys.modules:
            root_init = resolved_anchor / "__init__.py"
            root_mod = type(sys)(pkg_prefix)
            root_mod.__path__ = [str(resolved_anchor)]
            root_mod.__package__ = pkg_prefix
            sys.modules[pkg_prefix] = root_mod
            if root_init.exists():
                try:
                    root_spec = importlib.util.spec_from_file_location(
                        pkg_prefix, root_init,
                        submodule_search_locations=[str(resolved_anchor)],
                    )
                    root_mod_real = importlib.util.module_from_spec(root_spec)
                    root_mod_real.__path__ = [str(resolved_anchor)]
                    root_mod_real.__package__ = pkg_prefix
                    sys.modules[pkg_prefix] = root_mod_real
                    root_spec.loader.exec_module(root_mod_real)
                except Exception:
                    pass

        full_module_name = ".".join([pkg_prefix] + pkg_parts + [module_base])
        package_name = ".".join([pkg_prefix] + pkg_parts)
    else:
        full_module_name = f"{pkg_prefix}.{parts[0]}"
        package_name = pkg_prefix

        # Register anchor as root package
        if pkg_prefix not in sys.modules:
            root_mod = type(sys)(pkg_prefix)
            root_mod.__path__ = [str(resolved_anchor)]
            root_mod.__package__ = pkg_prefix
            sys.modules[pkg_prefix] = root_mod

    # Check if already loaded
    if full_module_name in sys.modules:
        return sys.modules[full_module_name]

    spec = importlib.util.spec_from_file_location(
        full_module_name, module_path,
        submodule_search_locations=None,
    )
    if spec is None:
        raise ImportError(f"Cannot create module spec for: {module_path}")
    module = importlib.util.module_from_spec(spec)
    module.__package__ = package_name
    sys.modules[full_module_name] = module
    spec.loader.exec_module(module)
    return module
