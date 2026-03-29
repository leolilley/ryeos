"""Ref file primitives — low-level mutable pointer operations.

Pure file operations for reading and writing ref files that store
a single hash string.  Unlike store.write_ref / store.read_ref (which
use a JSON-wrapped format), these primitives store the raw hash as
plain text.

No locking — concurrency control is the caller's responsibility.
"""

from __future__ import annotations

import logging
import os
import tempfile
from pathlib import Path
from typing import Optional

logger = logging.getLogger(__name__)


def read_ref(ref_path: Path) -> Optional[str]:
    """Read a ref file, returning the current hash or None if not found."""
    try:
        return ref_path.read_text("utf-8").strip()
    except FileNotFoundError:
        return None


def write_ref_atomic(ref_path: Path, new_hash: str) -> None:
    """Atomically write a hash to a ref file.

    Creates parent directories if needed.  Uses a temp file +
    ``os.replace()`` + fsync on the parent directory for durability.
    """
    ref_path.parent.mkdir(parents=True, exist_ok=True)

    fd, tmp_path = tempfile.mkstemp(dir=ref_path.parent)
    closed = False
    try:
        os.write(fd, new_hash.encode("utf-8"))
        os.fsync(fd)
        os.close(fd)
        closed = True
        os.replace(tmp_path, ref_path)
    except BaseException:
        if not closed:
            os.close(fd)
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise

    # fsync parent directory so the rename is durable
    dir_fd = os.open(str(ref_path.parent), os.O_RDONLY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)


def init_ref(ref_path: Path, hash_value: str) -> bool:
    """Create an initial ref file.  Returns True if created, False if it already exists."""
    if ref_path.exists():
        return False
    write_ref_atomic(ref_path, hash_value)
    return True
