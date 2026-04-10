"""Tests for _resolve_directive_chain context composition.

Covers:
- Empty chain (no extends)
- Single extends (parent → child) with root-first composition
- 3-level chain (root → parent → leaf) context composition
- Deduplication of knowledge IDs across chain
- Circular extends detection
- Suppress composition through chain
"""

import asyncio
import importlib.util
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from conftest import get_bundle_path

# Import _resolve_directive_chain via importlib (same pattern as test_thread_context_injection.py)
TD_PATH = get_bundle_path("standard", "tools/rye/agent/threads/thread_directive.py")
_td_spec = importlib.util.spec_from_file_location("thread_directive_test", TD_PATH)
_td_mod = importlib.util.module_from_spec(_td_spec)
_td_spec.loader.exec_module(_td_mod)
_resolve_directive_chain = _td_mod._resolve_directive_chain


# ── Helpers ───────────────────────────────────────────────────────────


def _mock_load_and_parser(*parent_dicts):
    """Create mocked resolve_item and ParserRouter that return parent directives in order.

    parent_dicts: sequence of parsed directive dicts returned for each
    parent load (in chain-walk order: first parent, then grandparent, etc.)
    """
    mock_resolve = AsyncMock(return_value={"status": "success", "content": "fake"})

    call_idx = {"i": 0}

    def _parse_side_effect(_fmt, _content):
        idx = call_idx["i"]
        call_idx["i"] += 1
        return parent_dicts[idx]

    mock_parser = MagicMock()
    mock_parser.parse = MagicMock(side_effect=_parse_side_effect)

    return mock_resolve, mock_parser


async def _patch_and_run(coro, mock_resolve, mock_parser):
    """Run an async coroutine with resolve_item, ParserRouter, and get_user_space patched."""
    with (
        patch("rye.actions._resolve.resolve_item", mock_resolve),
        patch("rye.utils.parser_router.ParserRouter", return_value=mock_parser),
        patch("rye.utils.resolvers.get_user_space", return_value=Path("/tmp/user")),
    ):
        return await coro


# ── No extends ────────────────────────────────────────────────────────


class TestNoExtends:
    """Chain with no extends attribute."""

    async def test_no_extends_no_context(self):
        directive = {"name": "leaf"}
        result = await _resolve_directive_chain("leaf", directive, "/tmp/test")
        assert result["chain"] == ["leaf"]
        assert result["context"]["system"] == []
        assert result["context"]["before"] == []
        assert result["context"]["after"] == []
        assert result["context"]["suppress"] == []

    async def test_no_extends_with_context(self):
        directive = {
            "name": "leaf",
            "context": {
                "system": ["my/identity"],
                "before": ["my/rules"],
                "after": ["my/checklist"],
            },
        }
        result = await _resolve_directive_chain("leaf", directive, "/tmp/test")
        assert result["chain"] == ["leaf"]
        assert result["context"]["system"] == ["my/identity"]
        assert result["context"]["before"] == ["my/rules"]
        assert result["context"]["after"] == ["my/checklist"]

    async def test_no_extends_context_as_string(self):
        """Context items can be a single string instead of a list."""
        directive = {
            "name": "leaf",
            "context": {"system": "single/item"},
        }
        result = await _resolve_directive_chain("leaf", directive, "/tmp/test")
        assert result["context"]["system"] == ["single/item"]


# ── Single extends ────────────────────────────────────────────────────


class TestSingleExtends:
    """Parent → child chain."""

    async def test_composes_root_first(self):
        parent = {
            "name": "parent",
            "context": {
                "system": ["parent/identity"],
                "before": ["parent/rules"],
            },
        }
        child = {
            "name": "child",
            "extends": "parent",
            "context": {
                "system": ["child/identity"],
                "after": ["child/checklist"],
            },
        }

        mock_lt, mock_pr = _mock_load_and_parser(parent)
        result = await _patch_and_run(
            _resolve_directive_chain("child", child, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        assert result["chain"] == ["parent", "child"]
        assert result["context"]["system"] == ["parent/identity", "child/identity"]
        assert result["context"]["before"] == ["parent/rules"]
        assert result["context"]["after"] == ["child/checklist"]

    async def test_child_inherits_empty_positions(self):
        """Parent with before, child with nothing — only parent's before appears."""
        parent = {
            "name": "parent",
            "context": {"before": ["parent/rules"]},
        }
        child = {"name": "child", "extends": "parent"}

        mock_lt, mock_pr = _mock_load_and_parser(parent)
        result = await _patch_and_run(
            _resolve_directive_chain("child", child, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        assert result["context"]["before"] == ["parent/rules"]
        assert result["context"]["system"] == []
        assert result["context"]["after"] == []


# ── Three-level chain ─────────────────────────────────────────────────


class TestThreeLevelChain:
    """Root → parent → leaf chain."""

    async def test_three_level_composes_root_first(self):
        root = {"name": "root", "context": {"system": ["root/identity"]}}
        parent = {
            "name": "parent",
            "extends": "root",
            "context": {"before": ["parent/rules"]},
        }
        leaf = {
            "name": "leaf",
            "extends": "parent",
            "context": {"after": ["leaf/checklist"]},
        }

        # Walk order: leaf loads parent first, then parent loads root
        mock_lt, mock_pr = _mock_load_and_parser(parent, root)
        result = await _patch_and_run(
            _resolve_directive_chain("leaf", leaf, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        assert result["chain"] == ["root", "parent", "leaf"]
        assert result["context"]["system"] == ["root/identity"]
        assert result["context"]["before"] == ["parent/rules"]
        assert result["context"]["after"] == ["leaf/checklist"]

    async def test_three_level_same_position(self):
        """All three levels add to 'system' — root-first order."""
        root = {"name": "root", "context": {"system": ["root/sys"]}}
        parent = {
            "name": "parent",
            "extends": "root",
            "context": {"system": ["parent/sys"]},
        }
        leaf = {
            "name": "leaf",
            "extends": "parent",
            "context": {"system": ["leaf/sys"]},
        }

        mock_lt, mock_pr = _mock_load_and_parser(parent, root)
        result = await _patch_and_run(
            _resolve_directive_chain("leaf", leaf, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        assert result["context"]["system"] == ["root/sys", "parent/sys", "leaf/sys"]


# ── Deduplication ─────────────────────────────────────────────────────


class TestDeduplication:
    """Same knowledge ID in multiple chain levels appears only once."""

    async def test_dedup_same_id_across_positions(self):
        parent = {"name": "parent", "context": {"system": ["shared/id", "parent/only"]}}
        child = {
            "name": "child",
            "extends": "parent",
            "context": {"system": ["shared/id", "child/only"]},
        }

        mock_lt, mock_pr = _mock_load_and_parser(parent)
        result = await _patch_and_run(
            _resolve_directive_chain("child", child, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        # shared/id from parent (first occurrence) wins, child duplicate dropped
        assert result["context"]["system"] == ["shared/id", "parent/only", "child/only"]

    async def test_dedup_in_before(self):
        parent = {"name": "parent", "context": {"before": ["shared/rules"]}}
        child = {
            "name": "child",
            "extends": "parent",
            "context": {"before": ["shared/rules", "child/rules"]},
        }

        mock_lt, mock_pr = _mock_load_and_parser(parent)
        result = await _patch_and_run(
            _resolve_directive_chain("child", child, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        assert result["context"]["before"] == ["shared/rules", "child/rules"]


# ── Circular detection ────────────────────────────────────────────────


class TestCircularDetection:
    """Circular extends chains are rejected."""

    async def test_circular_raises(self):
        directive_a = {"name": "a", "extends": "b"}

        # When we load "b", it extends "a" → circular
        directive_b = {"name": "b", "extends": "a"}

        mock_lt, mock_pr = _mock_load_and_parser(directive_b)
        with pytest.raises(ValueError, match="Circular extends chain"):
            await _patch_and_run(
                _resolve_directive_chain("a", directive_a, "/tmp/test"),
                mock_lt,
                mock_pr,
            )

    async def test_self_extends_raises(self):
        directive = {"name": "self-ref", "extends": "self-ref"}

        with pytest.raises(ValueError, match="Circular extends chain"):
            await _resolve_directive_chain("self-ref", directive, "/tmp/test")


# ── Suppress composition ─────────────────────────────────────────────


class TestSuppressComposition:
    """Suppress items collected from all chain levels."""

    async def test_suppress_from_single(self):
        directive = {
            "name": "leaf",
            "context": {"suppress": ["system_tool_protocol"]},
        }
        result = await _resolve_directive_chain("leaf", directive, "/tmp/test")
        assert result["context"]["suppress"] == ["system_tool_protocol"]

    async def test_suppress_composed_from_chain(self):
        parent = {"name": "parent", "context": {"suppress": ["system_tool_protocol"]}}
        child = {
            "name": "child",
            "extends": "parent",
            "context": {"suppress": ["system_identity"]},
        }

        mock_lt, mock_pr = _mock_load_and_parser(parent)
        result = await _patch_and_run(
            _resolve_directive_chain("child", child, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        assert "system_tool_protocol" in result["context"]["suppress"]
        assert "system_identity" in result["context"]["suppress"]

    async def test_suppress_deduplication(self):
        parent = {"name": "parent", "context": {"suppress": ["system_identity"]}}
        child = {
            "name": "child",
            "extends": "parent",
            "context": {"suppress": ["system_identity"]},
        }

        mock_lt, mock_pr = _mock_load_and_parser(parent)
        result = await _patch_and_run(
            _resolve_directive_chain("child", child, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        assert result["context"]["suppress"].count("system_identity") == 1


# ── Chain metadata ────────────────────────────────────────────────────


class TestChainMetadata:
    """chain_directives contains the full parsed directive dicts."""

    async def test_chain_directives_returned(self):
        parent = {"name": "parent", "context": {"system": ["p/id"]}}
        child = {"name": "child", "extends": "parent", "context": {"after": ["c/ck"]}}

        mock_lt, mock_pr = _mock_load_and_parser(parent)
        result = await _patch_and_run(
            _resolve_directive_chain("child", child, "/tmp/test"),
            mock_lt,
            mock_pr,
        )

        assert len(result["chain_directives"]) == 2
        assert result["chain_directives"][0]["name"] == "parent"  # root first
        assert result["chain_directives"][1]["name"] == "child"  # leaf last

    async def test_load_failure_raises(self):
        """Parent directive that fails to load raises ValueError."""
        child = {"name": "child", "extends": "nonexistent"}

        mock_resolve = AsyncMock(
            return_value={
                "status": "error",
                "error": "Directive not found: nonexistent",
            }
        )
        mock_parser = MagicMock()

        with pytest.raises(ValueError, match="Failed to load parent directive"):
            await _patch_and_run(
                _resolve_directive_chain("child", child, "/tmp/test"),
                mock_resolve,
                mock_parser,
            )


# ── Knowledge content extraction ─────────────────────────────────────
#
# These test the knowledge execution + content extraction path in
# thread_directive.execute() step 5, which assembles the system_prompt
# from the resolved extends chain's context knowledge items.
# ──────────────────────────────────────────────────────────────────────


async def _build_context_from_chain(
    directive: dict,
    directive_name: str,
    knowledge_contents: dict,
    project_path: str = "/tmp/test",
):
    """Helper: resolve extends chain + execute knowledge items, return (system_prompt, directive_context).

    Mirrors the logic in thread_directive.execute() step 5 but isolated
    from the full execute() flow (no LLM, no harness, no registry).

    Args:
        directive: parsed directive dict (with extends/context)
        directive_name: bare ID
        knowledge_contents: mapping of knowledge ID → body text returned by ExecuteTool
        project_path: fake project path
    """
    from rye.actions.execute import ExecuteTool

    chain_result = await _resolve_directive_chain(
        directive_name, directive, project_path
    )

    # Mock ExecuteTool.handle to return knowledge bodies
    async def _mock_exec_handle(**kwargs):
        item_id = kwargs.get("item_id", "")
        # Strip "knowledge:" prefix
        bare = item_id.replace("knowledge:", "")
        if bare in knowledge_contents:
            return {"status": "success", "content": knowledge_contents[bare]}
        return {"status": "error", "error": f"Knowledge not found: {bare}"}

    system_prompt = ""
    directive_context = {"before": "", "after": "", "suppress": []}
    suppressed = set(chain_result["context"].get("suppress", []))

    with patch.object(ExecuteTool, "handle", side_effect=_mock_exec_handle):
        exec_tool = ExecuteTool(user_space="/tmp/user")
        for position in ("system", "before", "after"):
            parts = []
            for kid in chain_result["context"].get(position, []):
                if kid in suppressed:
                    continue
                kr = await exec_tool.handle(
                    item_id=f"knowledge:{kid}", project_path=project_path,
                )
                if kr.get("status") != "success":
                    continue
                content = kr.get("content", "")
                if content:
                    parts.append(content.strip())
            if parts:
                if position == "system":
                    system_prompt = "\n\n".join(parts)
                else:
                    directive_context[position] = "\n\n".join(parts)
        directive_context["suppress"] = list(suppressed)

    return system_prompt, directive_context


class TestKnowledgeContentExtraction:
    """Knowledge items executed via ExecuteTool produce system_prompt content."""

    async def test_system_prompt_from_single_knowledge(self):
        """Directive with context.system knowledge gets content as system_prompt."""
        directive = {
            "name": "leaf",
            "context": {"system": ["my/identity"]},
        }
        knowledge = {"my/identity": "You are a helpful agent."}

        system_prompt, ctx = await _build_context_from_chain(
            directive, "leaf", knowledge
        )
        assert system_prompt == "You are a helpful agent."

    async def test_system_prompt_from_extends_chain(self):
        """Extends chain composes system_prompt root-first."""
        parent = {
            "name": "parent",
            "context": {"system": ["base/identity"]},
        }
        child = {
            "name": "child",
            "extends": "parent",
            "context": {"system": ["child/identity"]},
        }
        knowledge = {
            "base/identity": "You are Rye.",
            "child/identity": "You are a Track Blox agent.",
        }

        mock_lt, mock_pr = _mock_load_and_parser(parent)
        with (
            patch("rye.actions._resolve.resolve_item", mock_lt),
            patch("rye.utils.parser_router.ParserRouter", return_value=mock_pr),
            patch("rye.utils.resolvers.get_user_space", return_value=Path("/tmp/user")),
        ):
            system_prompt, ctx = await _build_context_from_chain(
                child, "child", knowledge
            )

        assert "You are Rye." in system_prompt
        assert "You are a Track Blox agent." in system_prompt
        # Root-first order
        assert system_prompt.index("You are Rye.") < system_prompt.index("You are a Track Blox agent.")

    async def test_before_after_context(self):
        """before/after context positions populate directive_context."""
        directive = {
            "name": "leaf",
            "context": {
                "before": ["my/rules"],
                "after": ["my/checklist"],
            },
        }
        knowledge = {
            "my/rules": "Always validate inputs.",
            "my/checklist": "Verify output format.",
        }

        system_prompt, ctx = await _build_context_from_chain(
            directive, "leaf", knowledge
        )
        assert system_prompt == ""
        assert ctx["before"] == "Always validate inputs."
        assert ctx["after"] == "Verify output format."

    async def test_suppressed_knowledge_excluded(self):
        """Suppressed knowledge IDs are not executed or included."""
        directive = {
            "name": "leaf",
            "context": {
                "system": ["keep/this", "drop/this"],
                "suppress": ["drop/this"],
            },
        }
        knowledge = {
            "keep/this": "Kept content.",
            "drop/this": "THIS SHOULD NOT APPEAR",
        }

        system_prompt, ctx = await _build_context_from_chain(
            directive, "leaf", knowledge
        )
        assert system_prompt == "Kept content."
        assert "SHOULD NOT APPEAR" not in system_prompt

    async def test_missing_knowledge_skipped(self):
        """Missing knowledge items are silently skipped (non-integrity errors)."""
        directive = {
            "name": "leaf",
            "context": {"system": ["exists/item", "missing/item"]},
        }
        knowledge = {"exists/item": "Present content."}

        system_prompt, ctx = await _build_context_from_chain(
            directive, "leaf", knowledge
        )
        assert system_prompt == "Present content."

    async def test_empty_content_skipped(self):
        """Knowledge items returning empty content are not included."""
        directive = {
            "name": "leaf",
            "context": {"system": ["real/item", "empty/item"]},
        }
        knowledge = {
            "real/item": "Real content.",
            "empty/item": "",
        }

        system_prompt, ctx = await _build_context_from_chain(
            directive, "leaf", knowledge
        )
        assert system_prompt == "Real content."

    async def test_content_stripped(self):
        """Leading/trailing whitespace in knowledge content is stripped."""
        directive = {
            "name": "leaf",
            "context": {"system": ["padded/item"]},
        }
        knowledge = {"padded/item": "\n\n  Agent identity text.  \n\n"}

        system_prompt, ctx = await _build_context_from_chain(
            directive, "leaf", knowledge
        )
        assert system_prompt == "Agent identity text."

    async def test_execute_returns_content_key_not_data_body(self):
        """Regression: ExecuteTool returns content at top level, not under data.body.

        The knowledge executor returns {status, content, item_id, metadata}.
        The content key holds the body text (frontmatter stripped).
        Previous bug: code accessed kr['data']['body'] which was always empty.
        """
        directive = {
            "name": "leaf",
            "context": {"system": ["test/knowledge"]},
        }

        # Simulate what ExecuteTool.handle actually returns for knowledge items
        mock_handle = AsyncMock(return_value={
            "status": "success",
            "content": "Behavioral rules here.",
            "item_id": "test/knowledge",
            "metadata": {},
            "type": "knowledge",
        })

        with patch.object(
            __import__("rye.actions.execute", fromlist=["ExecuteTool"]).ExecuteTool,
            "handle",
            mock_handle,
        ):
            chain_result = await _resolve_directive_chain("leaf", directive, "/tmp/test")

            from rye.actions.execute import ExecuteTool
            exec_tool = ExecuteTool(user_space="/tmp/user")
            parts = []
            for kid in chain_result["context"]["system"]:
                kr = await exec_tool.handle(
                    item_id=f"knowledge:{kid}", project_path="/tmp/test",
                )
                # This is the exact line from thread_directive.py — must work
                content = kr.get("content", "")
                if content:
                    parts.append(content.strip())

            system_prompt = "\n\n".join(parts)

        assert system_prompt == "Behavioral rules here."
        # Verify the old broken access pattern would fail
        assert mock_handle.return_value.get("data") is None
