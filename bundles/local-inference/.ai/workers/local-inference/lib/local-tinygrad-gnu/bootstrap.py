"""Selected GNU worker entry; session/lifecycle integration is source-owned."""
from pathlib import Path
import sys

worker_root = Path(__file__).resolve(strict=True).parent
workspace = Path.cwd().resolve(strict=True)
if workspace not in worker_root.parents:
    raise RuntimeError("GNU source is outside admitted workspace")
sys.path.insert(0, str(worker_root))

if __name__ == "__main__":
    from runtime import run_channel_product
    raise SystemExit(run_channel_product())
