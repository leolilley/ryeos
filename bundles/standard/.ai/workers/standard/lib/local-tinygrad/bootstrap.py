"""Hermetic entry point for the admitted tinygrad worker realization."""

from __future__ import annotations

import sys
from pathlib import Path


workspace = Path.cwd()
worker_root = Path(__file__).resolve(strict=True).parent
tinygrad_root = (workspace / "tinygrad").resolve(strict=True)
if workspace not in tinygrad_root.parents:
    raise RuntimeError("tinygrad import root escaped the admitted workspace")
sys.path[:0] = [str(worker_root), str(tinygrad_root)]

from session import main


if __name__ == "__main__":
    main()
