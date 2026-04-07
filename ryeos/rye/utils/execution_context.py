"""Execution context — explicit, immutable container for per-execution paths.

All env-var reads happen at the process boundary (CLI entry, server request
handler) via ``from_env()``.  Every downstream component receives an explicit
``ExecutionContext`` — no fallbacks, no globals.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Optional, Tuple

if TYPE_CHECKING:
    from rye.utils.path_utils import BundleInfo


@dataclass(frozen=True)
class ExecutionContext:
    """Immutable per-execution path context.

    Attributes:
        project_path: Project root containing ``.ai/``.
        user_space: User-space base directory (e.g. ``~`` or a CAS cache dir).
        signing_key_dir: Directory holding the Ed25519 keypair for signing.
        system_spaces: Ordered bundle roots for system-space resolution.
    """

    project_path: Path
    user_space: Path
    signing_key_dir: Path
    system_spaces: Tuple["BundleInfo", ...]

    # ------------------------------------------------------------------
    # Factory — the ONLY place env vars are consulted
    # ------------------------------------------------------------------

    @classmethod
    def from_env(
        cls,
        project_path: Optional[Path] = None,
    ) -> "ExecutionContext":
        """Build a context from environment variables / defaults.

        This is the single boundary where ``os.environ`` is read.
        CLI entry points and local tooling should use this; server-side
        code should construct an ``ExecutionContext`` explicitly.
        """
        from rye.utils.path_utils import (
            get_signing_key_dir,
            get_system_spaces,
            get_user_space,
        )

        return cls(
            project_path=project_path or Path.cwd(),
            user_space=get_user_space(),
            signing_key_dir=get_signing_key_dir(),
            system_spaces=tuple(get_system_spaces()),
        )
