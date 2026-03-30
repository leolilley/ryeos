"""Manifest diff and three-way merge for CAS snapshots.

Tree-level diff compares two SourceManifest dicts path by path.
Three-way merge resolves concurrent changes with conflict detection.
Text-level merge uses a diff3 algorithm for non-overlapping line edits.
"""

from __future__ import annotations

import difflib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from rye.primitives import cas


# ---------------------------------------------------------------------------
# Tree-level diff
# ---------------------------------------------------------------------------


@dataclass
class ManifestDiff:
    """Diff between two SourceManifest dicts."""

    added: Dict[str, str]  # path → hash (in target, not in base)
    removed: Dict[str, str]  # path → hash (in base, not in target)
    modified: Dict[str, tuple]  # path → (base_hash, target_hash)
    unchanged: Dict[str, str]  # path → hash
    path_types: Dict[str, str]  # path → "item" | "file"


def manifest_diff(base: dict, target: dict) -> ManifestDiff:
    """Diff two SourceManifest dicts. Handles both items and files."""
    base_items = base.get("items", {})
    base_files = base.get("files", {})
    target_items = target.get("items", {})
    target_files = target.get("files", {})

    base_paths = {**base_items, **base_files}
    target_paths = {**target_items, **target_files}

    # Track item vs file classification
    path_types: Dict[str, str] = {}
    for p in target_items:
        path_types[p] = "item"
    for p in target_files:
        path_types[p] = "file"
    for p in base_items:
        if p not in path_types:
            path_types[p] = "item"
    for p in base_files:
        if p not in path_types:
            path_types[p] = "file"

    all_paths = set(base_paths) | set(target_paths)
    added: Dict[str, str] = {}
    removed: Dict[str, str] = {}
    modified: Dict[str, tuple] = {}
    unchanged: Dict[str, str] = {}

    for path in all_paths:
        b, t = base_paths.get(path), target_paths.get(path)
        if b is None:
            added[path] = t
        elif t is None:
            removed[path] = b
        elif b != t:
            modified[path] = (b, t)
        else:
            unchanged[path] = b

    return ManifestDiff(added, removed, modified, unchanged, path_types)


# ---------------------------------------------------------------------------
# Three-way tree merge
# ---------------------------------------------------------------------------


@dataclass
class MergeResult:
    """Result of a three-way manifest merge."""

    merged_items: Dict[str, str]  # path → hash (items)
    merged_files: Dict[str, str]  # path → hash (files)
    conflicts: Dict[str, dict]  # path → {base, ours, theirs, type}

    @property
    def has_conflicts(self) -> bool:
        return len(self.conflicts) > 0


def three_way_merge(
    base: dict,
    ours: dict,
    theirs: dict,
    cas_root: Path,
) -> MergeResult:
    """Three-way merge of SourceManifest dicts.

    Resolution rules:
    - Both sides agree → take it
    - Only one side changed → take the change
    - Both deleted → accept deletion
    - One deleted, other modified → conflict
    - Both modified differently → attempt text merge via diff3
      - Text merge only for UTF-8 decodable blobs under 1MB
      - Binary/non-decodable → conflict
    - Both added same path differently → conflict
    - Path prefix conflict (file vs directory at same path) → conflict
    - Type conflict (same path in items on one side, files on other) → conflict
    """
    base_paths = {**base.get("items", {}), **base.get("files", {})}
    ours_paths = {**ours.get("items", {}), **ours.get("files", {})}
    theirs_paths = {**theirs.get("items", {}), **theirs.get("files", {})}

    # Track item/file classification per-side
    ours_items = set(ours.get("items", {}).keys())
    theirs_items = set(theirs.get("items", {}).keys())
    base_items_set = set(base.get("items", {}).keys())
    ours_files = set(ours.get("files", {}).keys())
    theirs_files = set(theirs.get("files", {}).keys())

    all_paths = set(base_paths) | set(ours_paths) | set(theirs_paths)
    merged: Dict[str, str] = {}
    conflicts: Dict[str, dict] = {}

    for path in all_paths:
        b = base_paths.get(path)
        o = ours_paths.get(path)
        t = theirs_paths.get(path)

        if o == t:
            # Both sides agree (includes both unchanged, both same change, both deleted)
            if o is not None:
                merged[path] = o
        elif b == o:
            # Only theirs changed
            if t is not None:
                merged[path] = t
        elif b == t:
            # Only ours changed
            if o is not None:
                merged[path] = o
        elif b is None:
            # Both added same path with different content
            conflicts[path] = {"base": None, "ours": o, "theirs": t, "type": "add/add"}
        elif o is None or t is None:
            # One side deleted, other side modified
            conflicts[path] = {
                "base": b,
                "ours": o,
                "theirs": t,
                "type": "delete/modify" if o is None else "modify/delete",
            }
        else:
            # Both modified differently — attempt text merge
            resolved = _try_text_merge(b, o, t, cas_root)
            if resolved is not None:
                merged[path] = resolved
            else:
                conflicts[path] = {"base": b, "ours": o, "theirs": t, "type": "content"}

    # Detect path prefix conflicts
    merged_set = set(merged.keys())
    prefix_conflicts: set[str] = set()
    for path in merged_set:
        prefix = path + "/"
        for other in merged_set:
            if other.startswith(prefix):
                prefix_conflicts.add(path)
                break

    for path in prefix_conflicts:
        conflicts[path] = {
            "base": base_paths.get(path),
            "ours": ours_paths.get(path),
            "theirs": theirs_paths.get(path),
            "type": "path_prefix",
        }
        merged.pop(path, None)

    # Detect type conflicts: same path as item on one side, file on other
    type_conflicts: set[str] = set()
    for path in list(merged.keys()):
        sides_item = (path in ours_items, path in theirs_items)
        sides_file = (path in ours_files, path in theirs_files)
        if any(sides_item) and any(sides_file):
            type_conflicts.add(path)

    for path in type_conflicts:
        conflicts[path] = {
            "base": base_paths.get(path),
            "ours": ours_paths.get(path),
            "theirs": theirs_paths.get(path),
            "type": "item_file_type",
        }
        merged.pop(path, None)

    # Separate merged paths back into items and files
    merged_items: Dict[str, str] = {}
    merged_files: Dict[str, str] = {}
    for path, h in merged.items():
        if path in ours_items or path in theirs_items or path in base_items_set:
            merged_items[path] = h
        else:
            merged_files[path] = h

    return MergeResult(merged_items, merged_files, conflicts)


# ---------------------------------------------------------------------------
# Three-way text merge (diff3)
# ---------------------------------------------------------------------------

_TEXT_MERGE_MAX_BYTES = 1_000_000  # 1MB


def _try_text_merge(
    base_hash: str,
    ours_hash: str,
    theirs_hash: str,
    cas_root: Path,
) -> Optional[str]:
    """Attempt line-level three-way merge. Returns merged blob hash or None.

    Guards: UTF-8 only, under _TEXT_MERGE_MAX_BYTES. Binary, non-decodable,
    or oversized blobs return None (conflict).
    """
    base_raw = cas.get_blob(base_hash, cas_root)
    ours_raw = cas.get_blob(ours_hash, cas_root)
    theirs_raw = cas.get_blob(theirs_hash, cas_root)

    if base_raw is None or ours_raw is None or theirs_raw is None:
        return None

    # Size guard
    if max(len(base_raw), len(ours_raw), len(theirs_raw)) > _TEXT_MERGE_MAX_BYTES:
        return None

    try:
        base_text = base_raw.decode("utf-8")
        ours_text = ours_raw.decode("utf-8")
        theirs_text = theirs_raw.decode("utf-8")
    except UnicodeDecodeError:
        return None

    merged_lines, has_conflicts = merge3(
        base_text.splitlines(True),
        ours_text.splitlines(True),
        theirs_text.splitlines(True),
    )

    if has_conflicts:
        return None

    merged_bytes = "".join(merged_lines).encode("utf-8")
    return cas.store_blob(merged_bytes, cas_root)


# ---------------------------------------------------------------------------
# merge3 algorithm
# ---------------------------------------------------------------------------


def merge3(
    base: List[str],
    ours: List[str],
    theirs: List[str],
) -> Tuple[List[str], bool]:
    """Three-way line-level merge using diff3.

    Returns (merged_lines, has_conflicts).

    Algorithm:
    1. Compute matching blocks from base→ours and base→theirs
    2. Walk both sets of blocks to identify change regions
    3. Non-overlapping changes: apply from whichever side changed
    4. Overlapping changes with identical result: apply
    5. Overlapping changes with different results: conflict
    """
    # Get change hunks for each side relative to base
    ours_hunks = _diff_hunks(base, ours)
    theirs_hunks = _diff_hunks(base, theirs)

    result: List[str] = []
    has_conflicts = False
    base_pos = 0

    # Merge the two hunk streams ordered by base position
    oi, ti = 0, 0
    while oi < len(ours_hunks) or ti < len(theirs_hunks):
        o_hunk = ours_hunks[oi] if oi < len(ours_hunks) else None
        t_hunk = theirs_hunks[ti] if ti < len(theirs_hunks) else None

        if o_hunk is not None and t_hunk is not None:
            # Both sides have remaining hunks — pick by base position
            # Two hunks overlap when their base ranges intersect:
            # [o_start, o_end) ∩ [t_start, t_end) is non-empty
            # i.e. o_start < t_end AND t_start < o_end
            overlaps = o_hunk[0] < t_hunk[1] and t_hunk[0] < o_hunk[1]
            # Edge case: zero-width hunks (inserts) at same position also overlap
            if o_hunk[0] == o_hunk[1] == t_hunk[0] == t_hunk[1]:
                overlaps = True

            if not overlaps and o_hunk[0] <= t_hunk[0]:
                # Ours finishes before theirs starts (no overlap)
                result.extend(base[base_pos : o_hunk[0]])
                result.extend(o_hunk[2])
                base_pos = o_hunk[1]
                oi += 1
            elif not overlaps and t_hunk[0] < o_hunk[0]:
                # Theirs finishes before ours starts (no overlap)
                result.extend(base[base_pos : t_hunk[0]])
                result.extend(t_hunk[2])
                base_pos = t_hunk[1]
                ti += 1
            else:
                # Overlapping regions
                if o_hunk[2] == t_hunk[2]:
                    # Both sides made the same change
                    merge_start = min(o_hunk[0], t_hunk[0])
                    merge_end = max(o_hunk[1], t_hunk[1])
                    result.extend(base[base_pos:merge_start])
                    result.extend(o_hunk[2])
                    base_pos = merge_end
                    oi += 1
                    ti += 1
                else:
                    # Conflict
                    has_conflicts = True
                    merge_start = min(o_hunk[0], t_hunk[0])
                    merge_end = max(o_hunk[1], t_hunk[1])
                    result.extend(base[base_pos:merge_start])
                    base_pos = merge_end
                    oi += 1
                    ti += 1

        elif o_hunk is not None:
            result.extend(base[base_pos : o_hunk[0]])
            result.extend(o_hunk[2])
            base_pos = o_hunk[1]
            oi += 1
        elif t_hunk is not None:
            result.extend(base[base_pos : t_hunk[0]])
            result.extend(t_hunk[2])
            base_pos = t_hunk[1]
            ti += 1

    # Remaining base lines after all hunks
    result.extend(base[base_pos:])

    return result, has_conflicts


def _diff_hunks(
    base: List[str], target: List[str]
) -> List[Tuple[int, int, List[str]]]:
    """Compute change hunks between base and target.

    Returns list of (base_start, base_end, replacement_lines) tuples.
    Each hunk means: replace base[base_start:base_end] with replacement_lines.
    """
    matcher = difflib.SequenceMatcher(None, base, target, autojunk=False)
    hunks: List[Tuple[int, int, List[str]]] = []

    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "equal":
            continue
        # replace, insert, or delete
        hunks.append((i1, i2, target[j1:j2]))

    return hunks
