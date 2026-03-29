"""Nonce-based replay protection for signed requests.

Local minute-bucket append-only journal files + in-memory LRU.

Layout:
    <cas_base>/runtime/replay/
      2026-03-26T12-30.journal
      2026-03-26T12-31.journal

Entry format: <timestamp>,<key_fingerprint>,<nonce_hash>\n
"""

import hashlib
import logging
import time
from collections import OrderedDict
from datetime import datetime, timezone
from pathlib import Path
from threading import Lock

logger = logging.getLogger(__name__)

# Nonce window matches request freshness (5 min) + margin
REPLAY_WINDOW_SECONDS = 360  # 6 minutes
LRU_MAX_SIZE = 50_000


class ReplayGuard:
    """Thread-safe nonce replay detector using minute-bucket journals."""

    def __init__(self, replay_dir: Path) -> None:
        self._dir = replay_dir
        self._dir.mkdir(parents=True, exist_ok=True)
        self._lru: OrderedDict[str, bool] = OrderedDict()
        self._lock = Lock()
        self._load_recent_buckets()

    def _bucket_name(self, ts: int | None = None) -> str:
        if ts is None:
            ts = int(time.time())
        dt = datetime.fromtimestamp(ts, tz=timezone.utc)
        return dt.strftime("%Y-%m-%dT%H-%M") + ".journal"

    def _entry_key(self, fingerprint: str, nonce: str) -> str:
        nonce_hash = hashlib.sha256(nonce.encode()).hexdigest()[:32]
        return f"{fingerprint}:{nonce_hash}"

    def _load_recent_buckets(self) -> None:
        """Load recent bucket files into the LRU on startup."""
        cutoff = int(time.time()) - REPLAY_WINDOW_SECONDS
        for path in sorted(self._dir.glob("*.journal")):
            try:
                # Parse bucket timestamp from filename
                stem = path.stem  # e.g. 2026-03-26T12-30
                bucket_dt = datetime.strptime(stem, "%Y-%m-%dT%H-%M").replace(
                    tzinfo=timezone.utc
                )
                bucket_ts = int(bucket_dt.timestamp())
                # Skip buckets older than the replay window
                if bucket_ts < cutoff - 60:
                    continue
                for line in path.read_text().splitlines():
                    parts = line.strip().split(",", 2)
                    if len(parts) == 3:
                        _, fp, nonce_hash = parts
                        key = f"{fp}:{nonce_hash}"
                        self._lru[key] = True
            except Exception:
                logger.warning("Failed to load replay bucket %s", path, exc_info=True)

        # Trim if we loaded too many
        while len(self._lru) > LRU_MAX_SIZE:
            self._lru.popitem(last=False)

    def check_and_record(self, fingerprint: str, nonce: str) -> bool:
        """Check if nonce is replayed; if not, record it.

        Returns True if the nonce is NEW (not a replay).
        Returns False if the nonce is a REPLAY.
        """
        key = self._entry_key(fingerprint, nonce)

        with self._lock:
            # Fast path: LRU hit
            if key in self._lru:
                return False

            # Check bucket files for the current window
            now = int(time.time())
            bucket_path = self._dir / self._bucket_name(now)
            if bucket_path.exists():
                try:
                    content = bucket_path.read_text()
                    nonce_hash = key.split(":", 1)[1]
                    if f",{fingerprint},{nonce_hash}\n" in content:
                        self._lru[key] = True
                        return False
                except Exception:
                    pass

            # Not found — record it
            self._lru[key] = True
            self._lru.move_to_end(key)

            # Trim LRU
            while len(self._lru) > LRU_MAX_SIZE:
                self._lru.popitem(last=False)

            # Append to bucket file
            nonce_hash = key.split(":", 1)[1]
            entry = f"{now},{fingerprint},{nonce_hash}\n"
            try:
                with open(bucket_path, "a") as f:
                    f.write(entry)
            except Exception:
                logger.warning("Failed to write replay entry", exc_info=True)

            return True

    def cleanup_expired(self) -> int:
        """Delete bucket files older than the replay window. Returns count deleted."""
        cutoff = int(time.time()) - REPLAY_WINDOW_SECONDS
        deleted = 0
        for path in self._dir.glob("*.journal"):
            try:
                stem = path.stem
                bucket_dt = datetime.strptime(stem, "%Y-%m-%dT%H-%M").replace(
                    tzinfo=timezone.utc
                )
                bucket_ts = int(bucket_dt.timestamp())
                if bucket_ts < cutoff - 60:
                    path.unlink()
                    deleted += 1
            except Exception:
                logger.warning("Failed to clean replay bucket %s", path, exc_info=True)
        return deleted


# Module-level singleton, initialized lazily
_guard: ReplayGuard | None = None
_guard_lock = Lock()


def get_replay_guard(cas_base_path: str) -> ReplayGuard:
    """Get or create the module-level ReplayGuard singleton."""
    global _guard
    if _guard is not None:
        return _guard
    with _guard_lock:
        if _guard is not None:
            return _guard
        replay_dir = Path(cas_base_path) / "runtime" / "replay"
        _guard = ReplayGuard(replay_dir)
        return _guard


def reset_guard() -> None:
    """Reset the singleton (for tests)."""
    global _guard
    _guard = None
