#!/usr/bin/env python3
"""Free disk space eaten by cargo build artifacts, safely.

Deletes only regenerable caches from the shared target dir:
  - target/debug/incremental   (the big one; 10-15G when it grows back)
  - target/nextest             (old run archives)
  - target/tmp                 (stray temp dirs)

Never touches target/release (bundle-owned binaries live there) or
target/debug/deps (current build cache - deleting it forces full rebuilds).

Usage: python3 scripts/dev/free-build-space.py [target-dir]
Default target dir: /home/leo/projects/ryeos-next/target
"""

import shutil
import subprocess
import sys
from pathlib import Path

target = Path(sys.argv[1] if len(sys.argv) > 1 else "/home/leo/projects/ryeos-next/target")

PRUNE = ["debug/incremental", "nextest", "tmp"]


def usage_gb(path: Path) -> float:
    try:
        out = subprocess.run(
            ["du", "-sb", str(path)], capture_output=True, text=True, check=True
        ).stdout
        return int(out.split()[0]) / 1e9
    except Exception:
        return 0.0


freed = 0.0
for rel in PRUNE:
    path = target / rel
    if path.exists():
        size = usage_gb(path)
        shutil.rmtree(path, ignore_errors=True)
        freed += size
        print(f"removed {path} ({size:.1f}G)")

df = subprocess.run(["df", "-h", str(target)], capture_output=True, text=True).stdout
print(f"freed ~{freed:.1f}G")
print(df.strip().splitlines()[-1])
