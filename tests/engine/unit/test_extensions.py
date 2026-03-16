"""Tests for rye.utils.extensions error handling."""

import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

import rye.utils.extensions as ext_mod
from rye.utils.extensions import (
    clear_extensions_cache,
    get_item_extensions,
    get_tool_extensions,
)


@pytest.fixture(autouse=True)
def _clear_cache():
    """Clear extension caches before and after each test."""
    clear_extensions_cache()
    yield
    clear_extensions_cache()


def _isolated_search_paths(project_root):
    """Return a mock for get_extractor_search_paths that only sees project_root."""
    extractors_dir = project_root / ".ai" / "tools" / "rye" / "core" / "extractors"
    return lambda _project_path=None: [extractors_dir]


@pytest.fixture
def empty_project():
    """Project with no extractors at all."""
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        (project_root / ".ai").mkdir()
        with patch.object(
            ext_mod,
            "get_extractor_search_paths",
            _isolated_search_paths(project_root),
        ):
            yield project_root


@pytest.fixture
def project_with_extractor():
    """Project with a working tool extractor."""
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        extractors_dir = (
            project_root / ".ai" / "tools" / "rye" / "core" / "extractors"
        )

        tool_dir = extractors_dir / "tool"
        tool_dir.mkdir(parents=True)
        (tool_dir / "python_extractor.yaml").write_text(
            "extensions:\n  - .py\n"
        )

        with patch.object(
            ext_mod,
            "get_extractor_search_paths",
            _isolated_search_paths(project_root),
        ):
            yield project_root


@pytest.fixture
def project_with_type_extractors():
    """Project with extractors for multiple item types."""
    with tempfile.TemporaryDirectory() as tmpdir:
        project_root = Path(tmpdir)
        extractors_dir = (
            project_root / ".ai" / "tools" / "rye" / "core" / "extractors"
        )

        for sub, filename, content in [
            ("tool", "python_extractor.yaml", "extensions:\n  - .py\n"),
            ("directive", "md_extractor.yaml", "extensions:\n  - .md\n"),
            ("knowledge", "md_extractor.yaml", "extensions:\n  - .md\n"),
            ("config", "config_extractor.yaml", "extensions:\n  - .yaml\n  - .yml\n"),
        ]:
            d = extractors_dir / sub
            d.mkdir(parents=True)
            (d / filename).write_text(content)

        with patch.object(
            ext_mod,
            "get_extractor_search_paths",
            _isolated_search_paths(project_root),
        ):
            yield project_root


# -- get_tool_extensions -----------------------------------------------------


class TestGetToolExtensions:
    def test_raises_when_no_extractors_found(self, empty_project):
        with pytest.raises(ValueError, match="No tool extensions found"):
            get_tool_extensions(project_path=empty_project, force_reload=True)

    def test_error_includes_search_paths(self, empty_project):
        with pytest.raises(ValueError, match="Expected \\*_extractor"):
            get_tool_extensions(project_path=empty_project, force_reload=True)

    def test_returns_extensions_from_extractor(self, project_with_extractor):
        result = get_tool_extensions(
            project_path=project_with_extractor, force_reload=True
        )
        assert ".py" in result

    def test_caches_result(self, project_with_extractor):
        first = get_tool_extensions(
            project_path=project_with_extractor, force_reload=True
        )
        second = get_tool_extensions(project_path=project_with_extractor)
        assert first == second


# -- get_item_extensions ------------------------------------------------------


class TestGetItemExtensions:
    def test_raises_for_unknown_item_type(self, empty_project):
        with pytest.raises(ValueError, match="No extractor glob pattern configured"):
            get_item_extensions("bogus", project_path=empty_project, force_reload=True)

    def test_error_lists_known_types(self, empty_project):
        with pytest.raises(ValueError, match="Known types:"):
            get_item_extensions("bogus", project_path=empty_project, force_reload=True)

    def test_raises_when_no_extractors_for_type(self, empty_project):
        with pytest.raises(ValueError, match="No extensions found for item type 'tool'"):
            get_item_extensions("tool", project_path=empty_project, force_reload=True)

    def test_error_includes_glob_and_paths(self, empty_project):
        with pytest.raises(ValueError, match="Expected extractors matching"):
            get_item_extensions(
                "directive", project_path=empty_project, force_reload=True
            )

    @pytest.mark.parametrize(
        "item_type,expected_ext",
        [
            ("tool", ".py"),
            ("directive", ".md"),
            ("knowledge", ".md"),
            ("config", ".yaml"),
        ],
    )
    def test_returns_extensions_for_each_type(
        self, project_with_type_extractors, item_type, expected_ext
    ):
        result = get_item_extensions(
            item_type,
            project_path=project_with_type_extractors,
            force_reload=True,
        )
        assert expected_ext in result

    def test_caches_result(self, project_with_type_extractors):
        first = get_item_extensions(
            "tool", project_path=project_with_type_extractors, force_reload=True
        )
        second = get_item_extensions(
            "tool", project_path=project_with_type_extractors
        )
        assert first == second
