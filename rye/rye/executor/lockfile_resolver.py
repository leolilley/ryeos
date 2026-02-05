"""LockfileResolver - Resolves lockfile paths with 3-tier precedence.

Three-tier architecture:
    - System: site-packages/rye/.ai/lockfiles/ (bundled, read-only, lowest precedence)
    - User: ~/.ai/lockfiles/ (default, read-write, medium precedence)
    - Project: {project}/lockfiles/ (opt-in, read-write, highest precedence)

The orchestrator resolves all paths and passes explicit paths to Lilux.
Lilux never does path discovery or precedence logic.
"""

import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

from lilux.primitives.lockfile import Lockfile, LockfileRoot, LockfileManager
from lilux.primitives.integrity import compute_tool_integrity

from rye.utils.path_utils import get_user_space, get_system_space, ensure_parent_directory


class LockfileResolver:
    """Resolves lockfile paths with 3-tier precedence.

    Read precedence: project → user → system (first match wins)
    Write location: depends on configured scope (user or project)
    """

    def __init__(
        self,
        project_path: Optional[Path] = None,
        user_space: Optional[Path] = None,
        system_space: Optional[Path] = None,
        scope: str = "user",
    ):
        """Initialize lockfile resolver.

        Args:
            project_path: Project root directory
            user_space: User space directory (~/.ai/)
            system_space: System space (site-packages/rye/.ai/)
            scope: Write scope - "user" (default) or "project"
        """
        self.project_path = Path(project_path) if project_path else None
        self.scope = scope

        # Resolve user space
        if user_space:
            self.user_space = Path(user_space)
        else:
            self.user_space = get_user_space()

        # Resolve system space (bundled with package)
        if system_space:
            self.system_space = Path(system_space)
        else:
            self.system_space = get_system_space()

        # Lilux lockfile manager (pure I/O)
        self.manager = LockfileManager()

    @property
    def system_dir(self) -> Path:
        """Get system lockfile directory (bundled, read-only)."""
        return self.system_space / "lockfiles"

    @property
    def user_dir(self) -> Path:
        """Get user lockfile directory."""
        return self.user_space / "lockfiles"

    @property
    def project_dir(self) -> Optional[Path]:
        """Get project lockfile directory (only if project_path set)."""
        if self.project_path:
            return self.project_path / "lockfiles"
        return None

    def get_lockfile(self, tool_id: str, version: str) -> Optional[Lockfile]:
        """Find and load lockfile using precedence.

        Checks: project → user → system
        Returns first match, or None if not found.

        Args:
            tool_id: Tool identifier
            version: Tool version (semver)

        Returns:
            Loaded Lockfile or None
        """
        path = self._resolve_read_path(tool_id, version)
        if path:
            try:
                return self.manager.load(path)
            except Exception:
                return None
        return None

    def save_lockfile(self, lockfile: Lockfile) -> Path:
        """Save lockfile to appropriate location based on scope.

        Args:
            lockfile: Lockfile to save

        Returns:
            Path where lockfile was saved
        """
        path = self._resolve_write_path(
            lockfile.root.tool_id,
            lockfile.root.version,
        )

        # Ensure parent directory exists
        ensure_parent_directory(path)

        return self.manager.save(lockfile, path)

    def create_lockfile(
        self,
        tool_id: str,
        version: str,
        integrity: str,
        resolved_chain: List[Dict[str, Any]],
        registry: Optional[Dict[str, Any]] = None,
    ) -> Lockfile:
        """Create a new lockfile for a resolved chain.

        Args:
            tool_id: Root tool identifier
            version: Tool version
            integrity: Tool integrity hash
            resolved_chain: List of resolved chain elements
            registry: Optional registry metadata

        Returns:
            New Lockfile object (not yet saved)
        """
        root = LockfileRoot(
            tool_id=tool_id,
            version=version,
            integrity=integrity,
        )

        return Lockfile(
            lockfile_version=1,
            generated_at=datetime.now(timezone.utc).isoformat(),
            root=root,
            resolved_chain=resolved_chain,
            registry=registry,
        )

    def exists(self, tool_id: str, version: str) -> bool:
        """Check if lockfile exists in any tier.

        Args:
            tool_id: Tool identifier
            version: Tool version

        Returns:
            True if lockfile exists
        """
        return self._resolve_read_path(tool_id, version) is not None

    def delete_lockfile(self, tool_id: str, version: str) -> bool:
        """Delete lockfile from writable locations.

        Only deletes from project or user space (not system).

        Args:
            tool_id: Tool identifier
            version: Tool version

        Returns:
            True if deleted, False if not found
        """
        name = self._lockfile_name(tool_id, version)

        # Check project first
        if self.project_dir:
            path = self.project_dir / name
            if path.exists():
                path.unlink()
                return True

        # Check user
        path = self.user_dir / name
        if path.exists():
            path.unlink()
            return True

        return False

    def list_lockfiles(self, space: str = "all") -> List[Dict[str, Any]]:
        """List available lockfiles.

        Args:
            space: "project", "user", "system", or "all"

        Returns:
            List of lockfile metadata dicts
        """
        results = []

        dirs_to_check = []
        if space in ("all", "project") and self.project_dir:
            dirs_to_check.append((self.project_dir, "project"))
        if space in ("all", "user"):
            dirs_to_check.append((self.user_dir, "user"))
        if space in ("all", "system"):
            dirs_to_check.append((self.system_dir, "system"))

        for dir_path, space_name in dirs_to_check:
            if not dir_path.exists():
                continue

            for lock_file in dir_path.glob("*.lock.json"):
                # Parse tool_id@version from filename
                name = lock_file.stem.replace(".lock", "")
                if "@" in name:
                    tool_id, version = name.rsplit("@", 1)
                else:
                    tool_id = name
                    version = "unknown"

                results.append(
                    {
                        "tool_id": tool_id,
                        "version": version,
                        "space": space_name,
                        "path": str(lock_file),
                    }
                )

        return results

    def _resolve_read_path(self, tool_id: str, version: str) -> Optional[Path]:
        """Apply precedence: project → user → system."""
        name = self._lockfile_name(tool_id, version)

        candidates = [
            self.project_dir,  # Highest precedence
            self.user_dir,
            self.system_dir,  # Lowest precedence
        ]

        for dir_path in candidates:
            if dir_path and (dir_path / name).exists():
                return dir_path / name

        return None

    def _resolve_write_path(self, tool_id: str, version: str) -> Path:
        """Determine write location from scope."""
        name = self._lockfile_name(tool_id, version)

        if self.scope == "project" and self.project_dir:
            return self.project_dir / name

        return self.user_dir / name

    def _lockfile_name(self, tool_id: str, version: str) -> str:
        """Generate lockfile filename."""
        return f"{tool_id}@{version}.lock.json"
