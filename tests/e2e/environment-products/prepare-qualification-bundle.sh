#!/usr/bin/env bash
set -euo pipefail

fixture_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
overlay=$fixture_root/qualification-bundle-overlay
expected_manifest=94c66e97825a25091abe924ae3de4715d26469332aa59f31cfb39854bdb59881

if [[ $# -ne 2 ]]; then
  printf 'usage: %s <runtime-manifest-hash> <new-bundle-overlay-root>\n' "$0" >&2
  exit 64
fi

manifest_hash=$1
destination=$2
if [[ ! $manifest_hash =~ ^[0-9a-f]{64}$ ]]; then
  printf '%s\n' 'runtime manifest hash must be exactly 64 lowercase hexadecimal characters' >&2
  exit 64
fi
if [[ $manifest_hash != "$expected_manifest" ]]; then
  printf 'runtime manifest differs from the signed fixture expectation: %s\n' "$manifest_hash" >&2
  exit 65
fi
if [[ -e $destination ]]; then
  printf 'qualification bundle destination already exists: %s\n' "$destination" >&2
  exit 73
fi

/usr/bin/mkdir -p -- "$destination"
/usr/bin/cp -a -- "$overlay/." "$destination/"

# A Worker source digest commits every byte below its declared logical source
# root. Populate the fixture placeholder before the ordinary Bundle publisher
# signs either Worker definition; no live worker or installed source is read.
source_digest=$(python3 - \
  "$destination/codex/.ai/workers/fixture/lib/hosted" \
  "$destination/codex/.ai/workers/fixture/enrollment.yaml" \
  "$destination/codex/.ai/workers/fixture/hosted.yaml" <<'PY'
import hashlib
import json
from pathlib import Path
import stat
import sys

source_root = Path(sys.argv[1])
workers = [Path(path) for path in sys.argv[2:]]
entries = []
total_bytes = 0
for path in sorted(source_root.rglob("*")):
    relative = path.relative_to(source_root).as_posix()
    metadata = path.lstat()
    if stat.S_ISDIR(metadata.st_mode):
        continue
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"fixture Worker source contains a non-regular entry: {relative}")
    content = path.read_bytes()
    total_bytes += len(content)
    entries.append({
        "root": "source",
        "path": relative,
        "blob_hash": hashlib.sha256(content).hexdigest(),
        "size": len(content),
        "mode": "read_only",
    })
if not entries:
    raise SystemExit("fixture Worker source closure is empty")
manifest = {
    "schema": 1,
    "kind": "ryeos.source_closure_manifest",
    "roots": [{"id": "source"}],
    "entries": entries,
    "totals": {"file_count": len(entries), "total_bytes": total_bytes},
}
canonical = json.dumps(manifest, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
digest = hashlib.sha256(canonical.encode()).hexdigest()
replacements = 0
for worker in workers:
    text = worker.read_text(encoding="utf-8")
    count = text.count("FIXTURE_SOURCE_DIGEST")
    if count != 1:
        raise SystemExit(f"fixture Worker must contain exactly one source digest placeholder: {worker}")
    worker.write_text(text.replace("FIXTURE_SOURCE_DIGEST", digest), encoding="utf-8")
    replacements += count
if replacements != len(workers):
    raise SystemExit("fixture Worker source digest replacement was incomplete")
print(digest)
PY
)

printf '%s\n' \
  "prepared unsigned standard + codex qualification Bundle overlays at $destination" \
  "fixed subject manifest: $manifest_hash" \
  "fixed fixture Worker source: $source_digest" \
  'publisher signing, Bundle publication, installation, content binding, and execution were not run'
