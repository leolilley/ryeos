"""LockfileResolver - Resolves lockfile paths with 3-tier precedence.

Three-tier architecture:
    - System: site-packages/rye/.ai/lockfiles/ (bundled, read-only, lowest precedence)
    - User: {USER_SPACE}/.ai/lockfiles/ (default, read-write, medium precedence)
    - Project: {project}/.ai/lockfiles/ (opt-in, read-write, highest precedence)

The orchestrator resolves all paths and passes explicit paths to Lilux.
Lilux never does path discovery or precedence logic.
"""

from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

from lilux.primitives.lockfile import Lockfile, LockfileManager, LockfileRoot

from rye.constants import AI_DIR
from rye.utils.path_utils import (
    ensure_parent_directory,
    get_system_spaces,
    get_user_space,
)


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
    ):
        """Initialize lockfile resolver.

        Args:
            project_path: Project root directory
            user_space: User space base path (~ or $USER_SPACE)
            system_space: System space base path (site-packages/rye/) — legacy, ignored if None
        """
        self.project_path = Path(project_path) if project_path else None

        # Resolve user space
        if user_space:
            self.user_space = Path(user_space)
        else:
            self.user_space = get_user_space()

        # Resolve system spaces (all bundles)
        if system_space:
            from rye.utils.path_utils import BundleInfo
            self.system_spaces = [
                BundleInfo(
                    bundle_id="ryeos",
                    version="0.0.0",
                    root_path=Path(system_space),
                    manifest_path=None,
                    source="legacy",
                )
            ]
        else:
            self.system_spaces = get_system_spaces()

        # Lilux lockfile manager (pure I/O)
        self.manager = LockfileManager()

    @property
    def system_dirs(self) -> List[Path]:
        """Get system lockfile directories across all bundles (read-only)."""
        return [b.root_path / AI_DIR / "lockfiles" for b in self.system_spaces]

    @property
    def user_dir(self) -> Path:
        """Get user lockfile directory."""
        return self.user_space / AI_DIR / "lockfiles"

    @property
    def project_dir(self) -> Optional[Path]:
        """Get project lockfile directory (only if project_path set)."""
        if self.project_path:
            return self.project_path / AI_DIR / "lockfiles"
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

    def save_lockfile(self, lockfile: Lockfile, space: str = "project") -> Path:
        """Save lockfile to appropriate location based on resolved tool space.

        Args:
            lockfile: Lockfile to save
            space: Space where the root tool was resolved ("project", "user", "system")

        Returns:
            Path where lockfile was saved
        """
        path = self._resolve_write_path(
            lockfile.root.tool_id,
            lockfile.root.version,
            space,
        )
        ensure_parent_directory(path)
        return self.manager.save(lockfile, path)

    def create_lockfile(
        self,
        tool_id: str,
        version: str,
        integrity: str,
        resolved_chain: List[Dict[str, Any]],
        registry: Optional[Dict[str, Any]] = None,
        verified_deps: Optional[Dict[str, Any]] = None,
    ) -> Lockfile:
        """Create a new lockfile for a resolved chain.

        Args:
            tool_id: Root tool identifier
            version: Tool version
            integrity: Tool integrity hash
            resolved_chain: List of resolved chain elements
            registry: Optional registry metadata
            verified_deps: Optional dependency verification hashes

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
            verified_deps=verified_deps,
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
            for sd in self.system_dirs:
                dirs_to_check.append((sd, "system"))

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
        ]
        # System bundles (lowest precedence)
        candidates.extend(self.system_dirs)

        for dir_path in candidates:
            if dir_path and (dir_path / name).exists():
                return dir_path / name

        return None

    def _resolve_write_path(
        self, tool_id: str, version: str, space: str = "project"
    ) -> Path:
        """Determine write location from resolved space.

        If a project_path is configured, lockfiles always write to the project
        directory — the execution context determines where lockfiles live, not
        the tool's source space.  System space is read-only so system tools
        running inside a project should cache their lockfiles there.
        """
        name = self._lockfile_name(tool_id, version)
        if self.project_dir:
            return self.project_dir / name
        # No project context — fall back to user space
        return self.user_dir / name

    def _lockfile_name(self, tool_id: str, version: str) -> str:
        """Generate lockfile filename."""
        return f"{tool_id}@{version}.lock.json"
