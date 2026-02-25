"""Lockfile I/O operations (Phase 2.1).

Pure lockfile I/O with explicit paths only. No path resolution or creation logic.
"""

import json
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Dict, Any, Optional, List

from lilux.primitives.errors import LockfileError


@dataclass
class LockfileRoot:
    """Root metadata for a lockfile.
    
    Attributes:
        tool_id: The ID of the tool being locked.
        version: Version of the tool (semver).
        integrity: Hash of the tool for integrity verification.
    """

    tool_id: str
    version: str
    integrity: str


@dataclass
class Lockfile:
    """Complete lockfile structure.
    
    Attributes:
        lockfile_version: Version of the lockfile format (integer).
        generated_at: ISO timestamp when lockfile was generated.
        root: LockfileRoot with tool metadata.
        resolved_chain: List of resolved dependencies.
        registry: Optional registry metadata.
    """

    lockfile_version: int
    generated_at: str
    root: LockfileRoot
    resolved_chain: List[Any]
    registry: Optional[Dict[str, Any]] = None
    verified_deps: Optional[Dict[str, Any]] = None


class LockfileManager:
    """Manages lockfile I/O operations.
    
    Pure I/O operations with explicit paths only. No validation, creation,
    or path resolution logic.
    """

    def load(self, path: Path) -> Lockfile:
        """Load lockfile from JSON file.
        
        Args:
            path: Path to lockfile JSON.
            
        Returns:
            Loaded Lockfile object.
            
        Raises:
            FileNotFoundError: If file doesn't exist.
            LockfileError: If JSON is invalid or missing required fields.
        """
        path = Path(path)

        try:
            content = path.read_text()
        except FileNotFoundError:
            raise

        try:
            data = json.loads(content)
        except json.JSONDecodeError as e:
            raise LockfileError(f"Invalid JSON in lockfile: {e}", path=str(path))

        # Validate required fields
        required = ["lockfile_version", "generated_at", "root", "resolved_chain"]
        for field_name in required:
            if field_name not in data:
                raise LockfileError(
                    f"Missing required field: {field_name}",
                    path=str(path),
                )

        try:
            root_data = data["root"]
            root = LockfileRoot(
                tool_id=root_data["tool_id"],
                version=root_data["version"],
                integrity=root_data["integrity"],
            )

            lockfile = Lockfile(
                lockfile_version=data["lockfile_version"],
                generated_at=data["generated_at"],
                root=root,
                resolved_chain=data["resolved_chain"],
                registry=data.get("registry"),
                verified_deps=data.get("verified_deps"),
            )
            return lockfile
        except (KeyError, TypeError) as e:
            raise LockfileError(
                f"Invalid lockfile structure: {e}",
                path=str(path),
            )

    def save(self, lockfile: Lockfile, path: Path) -> Path:
        """Save lockfile to JSON file.
        
        Args:
            lockfile: Lockfile object to save.
            path: Path where to save lockfile.
            
        Returns:
            The path where lockfile was saved.
            
        Raises:
            FileNotFoundError: If parent directory doesn't exist.
        """
        path = Path(path)

        # Convert to dict
        data = {
            "lockfile_version": lockfile.lockfile_version,
            "generated_at": lockfile.generated_at,
            "root": asdict(lockfile.root),
            "resolved_chain": lockfile.resolved_chain,
            "registry": lockfile.registry,
            "verified_deps": lockfile.verified_deps,
        }

        # This will raise FileNotFoundError if parent doesn't exist
        content = json.dumps(data, indent=2)
        path.write_text(content)

        return path

    def exists(self, path: Path) -> bool:
        """Check if lockfile exists.
        
        Args:
            path: Path to check.
            
        Returns:
            True if file exists, False otherwise.
        """
        return Path(path).exists()
