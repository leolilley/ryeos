"""Tests for CAS manifest diff and three-way merge."""

import pytest

from rye.primitives import cas
from rye.cas.merge import (
    ManifestDiff,
    MergeResult,
    manifest_diff,
    three_way_merge,
    merge3,
    _TEXT_MERGE_MAX_BYTES,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _manifest(items=None, files=None):
    """Build a SourceManifest-shaped dict."""
    return {"items": items or {}, "files": files or {}}


def _store_text(text: str, root) -> str:
    """Store a text blob in CAS, return hash."""
    return cas.store_blob(text.encode("utf-8"), root)


# ---------------------------------------------------------------------------
# manifest_diff tests
# ---------------------------------------------------------------------------


class TestManifestDiff:
    def test_identical_manifests(self):
        m = _manifest(items={"a": "h1"}, files={"b": "h2"})
        diff = manifest_diff(m, m)
        assert diff.added == {}
        assert diff.removed == {}
        assert diff.modified == {}
        assert diff.unchanged == {"a": "h1", "b": "h2"}

    def test_added_paths(self):
        base = _manifest(items={"a": "h1"})
        target = _manifest(items={"a": "h1", "b": "h2"})
        diff = manifest_diff(base, target)
        assert diff.added == {"b": "h2"}
        assert diff.removed == {}
        assert diff.modified == {}
        assert diff.unchanged == {"a": "h1"}

    def test_removed_paths(self):
        base = _manifest(items={"a": "h1", "b": "h2"})
        target = _manifest(items={"a": "h1"})
        diff = manifest_diff(base, target)
        assert diff.removed == {"b": "h2"}
        assert diff.added == {}

    def test_modified_paths(self):
        base = _manifest(items={"a": "h1"})
        target = _manifest(items={"a": "h2"})
        diff = manifest_diff(base, target)
        assert diff.modified == {"a": ("h1", "h2")}
        assert diff.unchanged == {}

    def test_path_types_tracked(self):
        base = _manifest(items={"a": "h1"}, files={"b": "h2"})
        target = _manifest(items={"a": "h1"}, files={"b": "h3", "c": "h4"})
        diff = manifest_diff(base, target)
        assert diff.path_types["a"] == "item"
        assert diff.path_types["b"] == "file"
        assert diff.path_types["c"] == "file"

    def test_empty_manifests(self):
        diff = manifest_diff(_manifest(), _manifest())
        assert diff.added == {}
        assert diff.removed == {}
        assert diff.modified == {}
        assert diff.unchanged == {}


# ---------------------------------------------------------------------------
# three_way_merge tests
# ---------------------------------------------------------------------------


class TestThreeWayMerge:
    def test_no_changes(self, tmp_path):
        m = _manifest(items={"a": "h1"}, files={"b": "h2"})
        result = three_way_merge(m, m, m, tmp_path)
        assert not result.has_conflicts
        assert result.merged_items == {"a": "h1"}
        assert result.merged_files == {"b": "h2"}

    def test_only_ours_changed(self, tmp_path):
        base = _manifest(items={"a": "h1"})
        ours = _manifest(items={"a": "h2"})
        theirs = _manifest(items={"a": "h1"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert not result.has_conflicts
        assert result.merged_items == {"a": "h2"}

    def test_only_theirs_changed(self, tmp_path):
        base = _manifest(items={"a": "h1"})
        ours = _manifest(items={"a": "h1"})
        theirs = _manifest(items={"a": "h2"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert not result.has_conflicts
        assert result.merged_items == {"a": "h2"}

    def test_both_same_change(self, tmp_path):
        base = _manifest(items={"a": "h1"})
        ours = _manifest(items={"a": "h2"})
        theirs = _manifest(items={"a": "h2"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert not result.has_conflicts
        assert result.merged_items == {"a": "h2"}

    def test_both_deleted(self, tmp_path):
        base = _manifest(items={"a": "h1"})
        ours = _manifest()
        theirs = _manifest()
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert not result.has_conflicts
        assert result.merged_items == {}

    def test_ours_added_theirs_unchanged(self, tmp_path):
        base = _manifest(items={"a": "h1"})
        ours = _manifest(items={"a": "h1", "b": "h2"})
        theirs = _manifest(items={"a": "h1"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert not result.has_conflicts
        assert result.merged_items == {"a": "h1", "b": "h2"}

    def test_both_add_different_paths(self, tmp_path):
        base = _manifest()
        ours = _manifest(items={"a": "h1"})
        theirs = _manifest(files={"b": "h2"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert not result.has_conflicts
        assert result.merged_items == {"a": "h1"}
        assert result.merged_files == {"b": "h2"}

    def test_delete_modify_conflict(self, tmp_path):
        base = _manifest(items={"a": "h1"})
        ours = _manifest()  # deleted
        theirs = _manifest(items={"a": "h2"})  # modified
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert result.has_conflicts
        assert "a" in result.conflicts
        assert result.conflicts["a"]["type"] == "delete/modify"

    def test_modify_delete_conflict(self, tmp_path):
        base = _manifest(items={"a": "h1"})
        ours = _manifest(items={"a": "h2"})  # modified
        theirs = _manifest()  # deleted
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert result.has_conflicts
        assert result.conflicts["a"]["type"] == "modify/delete"

    def test_add_add_conflict(self, tmp_path):
        base = _manifest()
        ours = _manifest(items={"a": "h1"})
        theirs = _manifest(items={"a": "h2"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert result.has_conflicts
        assert result.conflicts["a"]["type"] == "add/add"

    def test_content_conflict_binary(self, tmp_path):
        """Both modified differently, blob is binary → conflict."""
        root = tmp_path / "cas"
        root.mkdir()
        base_h = cas.store_blob(b"\x00\x01\x02", root)
        ours_h = cas.store_blob(b"\x00\x01\x03", root)
        theirs_h = cas.store_blob(b"\x00\x01\x04", root)

        base = _manifest(files={"bin": base_h})
        ours = _manifest(files={"bin": ours_h})
        theirs = _manifest(files={"bin": theirs_h})
        result = three_way_merge(base, ours, theirs, root)
        assert result.has_conflicts
        assert result.conflicts["bin"]["type"] == "content"

    def test_content_conflict_large_file(self, tmp_path):
        """Both modified, but file exceeds 1MB → conflict (skip merge)."""
        root = tmp_path / "cas"
        root.mkdir()
        big = b"x" * (_TEXT_MERGE_MAX_BYTES + 1)
        base_h = cas.store_blob(big, root)
        ours_h = cas.store_blob(big + b"a", root)
        theirs_h = cas.store_blob(big + b"b", root)

        base = _manifest(files={"big": base_h})
        ours = _manifest(files={"big": ours_h})
        theirs = _manifest(files={"big": theirs_h})
        result = three_way_merge(base, ours, theirs, root)
        assert result.has_conflicts
        assert result.conflicts["big"]["type"] == "content"

    def test_text_merge_non_overlapping(self, tmp_path):
        """Both modified different lines → auto-merge succeeds."""
        root = tmp_path / "cas"
        root.mkdir()
        base_h = _store_text("line1\nline2\nline3\n", root)
        ours_h = _store_text("LINE1\nline2\nline3\n", root)
        theirs_h = _store_text("line1\nline2\nLINE3\n", root)

        base = _manifest(items={"f": base_h})
        ours = _manifest(items={"f": ours_h})
        theirs = _manifest(items={"f": theirs_h})
        result = three_way_merge(base, ours, theirs, root)
        assert not result.has_conflicts
        merged_blob = cas.get_blob(result.merged_items["f"], root)
        assert merged_blob == b"LINE1\nline2\nLINE3\n"

    def test_text_merge_same_line_conflict(self, tmp_path):
        """Both modified the same line differently → conflict."""
        root = tmp_path / "cas"
        root.mkdir()
        base_h = _store_text("line1\nline2\nline3\n", root)
        ours_h = _store_text("line1\nOURS\nline3\n", root)
        theirs_h = _store_text("line1\nTHEIRS\nline3\n", root)

        base = _manifest(items={"f": base_h})
        ours = _manifest(items={"f": ours_h})
        theirs = _manifest(items={"f": theirs_h})
        result = three_way_merge(base, ours, theirs, root)
        assert result.has_conflicts
        assert result.conflicts["f"]["type"] == "content"

    def test_path_prefix_conflict(self, tmp_path):
        """File 'a' and file 'a/b.txt' can't coexist."""
        base = _manifest()
        ours = _manifest(files={"a": "h1"})
        theirs = _manifest(files={"a/b.txt": "h2"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert result.has_conflicts
        assert result.conflicts["a"]["type"] == "path_prefix"

    def test_item_file_type_conflict(self, tmp_path):
        """Same path as item on one side, file on other → conflict."""
        base = _manifest()
        ours = _manifest(items={"x": "h1"})
        theirs = _manifest(files={"x": "h1"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert result.has_conflicts
        assert result.conflicts["x"]["type"] == "item_file_type"

    def test_item_classification_preserved(self, tmp_path):
        """Items stay items, files stay files through merge."""
        base = _manifest(items={"a": "h1"}, files={"b": "h2"})
        ours = _manifest(items={"a": "h1", "c": "h3"}, files={"b": "h2"})
        theirs = _manifest(items={"a": "h1"}, files={"b": "h2", "d": "h4"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert not result.has_conflicts
        assert "a" in result.merged_items
        assert "c" in result.merged_items
        assert "b" in result.merged_files
        assert "d" in result.merged_files

    def test_noop_execution(self, tmp_path):
        """If theirs == base (no changes), merge is trivially ours."""
        base = _manifest(items={"a": "h1"})
        ours = _manifest(items={"a": "h2"})
        theirs = _manifest(items={"a": "h1"})
        result = three_way_merge(base, ours, theirs, tmp_path)
        assert not result.has_conflicts
        assert result.merged_items == {"a": "h2"}


# ---------------------------------------------------------------------------
# merge3 algorithm tests
# ---------------------------------------------------------------------------


class TestMerge3:
    def test_no_changes(self):
        base = ["a\n", "b\n", "c\n"]
        lines, conflicts = merge3(base, base, base)
        assert lines == base
        assert not conflicts

    def test_only_ours_changed(self):
        base = ["a\n", "b\n", "c\n"]
        ours = ["A\n", "b\n", "c\n"]
        lines, conflicts = merge3(base, ours, base)
        assert lines == ours
        assert not conflicts

    def test_only_theirs_changed(self):
        base = ["a\n", "b\n", "c\n"]
        theirs = ["a\n", "b\n", "C\n"]
        lines, conflicts = merge3(base, base, theirs)
        assert lines == theirs
        assert not conflicts

    def test_non_overlapping_changes(self):
        base = ["a\n", "b\n", "c\n"]
        ours = ["A\n", "b\n", "c\n"]
        theirs = ["a\n", "b\n", "C\n"]
        lines, conflicts = merge3(base, ours, theirs)
        assert lines == ["A\n", "b\n", "C\n"]
        assert not conflicts

    def test_same_change_both_sides(self):
        base = ["a\n", "b\n", "c\n"]
        both = ["a\n", "B\n", "c\n"]
        lines, conflicts = merge3(base, both, both)
        assert lines == both
        assert not conflicts

    def test_conflicting_changes(self):
        base = ["a\n", "b\n", "c\n"]
        ours = ["a\n", "OURS\n", "c\n"]
        theirs = ["a\n", "THEIRS\n", "c\n"]
        _, conflicts = merge3(base, ours, theirs)
        assert conflicts

    def test_ours_insert(self):
        base = ["a\n", "c\n"]
        ours = ["a\n", "b\n", "c\n"]
        lines, conflicts = merge3(base, ours, base)
        assert lines == ["a\n", "b\n", "c\n"]
        assert not conflicts

    def test_theirs_delete(self):
        base = ["a\n", "b\n", "c\n"]
        theirs = ["a\n", "c\n"]
        lines, conflicts = merge3(base, base, theirs)
        assert lines == ["a\n", "c\n"]
        assert not conflicts

    def test_both_insert_different_locations(self):
        base = ["a\n", "b\n", "c\n"]
        ours = ["X\n", "a\n", "b\n", "c\n"]
        theirs = ["a\n", "b\n", "c\n", "Y\n"]
        lines, conflicts = merge3(base, ours, theirs)
        assert lines == ["X\n", "a\n", "b\n", "c\n", "Y\n"]
        assert not conflicts

    def test_empty_base(self):
        lines, conflicts = merge3([], ["a\n"], ["a\n"])
        assert lines == ["a\n"]
        assert not conflicts

    def test_empty_base_different_additions(self):
        _, conflicts = merge3([], ["a\n"], ["b\n"])
        assert conflicts
