"""Hermetic entry point for the admitted tinygrad worker realization."""

from __future__ import annotations

import sys
from pathlib import Path


workspace = Path.cwd()
worker_root = (workspace / "worker").resolve(strict=True)
tinygrad_root = (workspace / "tinygrad").resolve(strict=True)
for root in (worker_root, tinygrad_root):
    if workspace not in root.parents:
        raise RuntimeError("worker import root escaped the admitted workspace")
sys.path[:0] = [str(worker_root), str(tinygrad_root)]

from session import main


if __name__ == "__main__":
    main()
