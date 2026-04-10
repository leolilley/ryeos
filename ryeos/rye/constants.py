"""RYE Constants

Centralized constants for the AI directory name, item types, and tool actions.
"""

from typing import Optional

# The name of the working directory used in all three spaces.
# Every space follows: base_path / AI_DIR / {type_dir} / {item_id}
AI_DIR = ".ai"

# Runtime state directory (engine-managed, gitignored)
STATE_DIR = "state"
STATE_THREADS = "threads"
STATE_GRAPHS = "graphs"
STATE_OBJECTS = "objects"
STATE_CACHE = "cache"

# Derived path segments for thread persistence (use with AI_DIR / ...)
# State: .ai/state/threads/{thread_id}/  (JSONL, thread.json, capabilities, etc.)
STATE_THREADS_REL = f"{STATE_DIR}/{STATE_THREADS}"
# Knowledge: .ai/knowledge/agent/threads/  (rendered markdown transcripts)
KNOWLEDGE_THREADS_REL = "knowledge/agent/threads"


class ItemType:
    """Item type constants."""

    DIRECTIVE = "directive"
    TOOL = "tool"
    KNOWLEDGE = "knowledge"

    ALL = [DIRECTIVE, TOOL, KNOWLEDGE]

    # Items that can be signed (ALL + config files)
    CONFIG = "config"
    SIGNABLE = [DIRECTIVE, TOOL, KNOWLEDGE, CONFIG]

    # Kind directory mappings (execute/fetch/sign — NO config)
    KIND_DIRS = {
        DIRECTIVE: "directives",
        TOOL: "tools",
        KNOWLEDGE: "knowledge",
    }

    # Extended mapping for signing and integrity (includes config)
    SIGNABLE_KINDS = {
        **KIND_DIRS,
        CONFIG: "config",
    }

    # File extensions to search per item type (tools use dynamic lookup)
    CONTENT_EXTENSIONS = {
        DIRECTIVE: [".md"],
        KNOWLEDGE: [".md", ".yaml", ".yml"],
    }

    # Canonical ref format: kind:item_id  (e.g. "tool:rye/bash/bash")
    _CANONICAL_PREFIXES = {
        "tool:": TOOL,
        "directive:": DIRECTIVE,
        "knowledge:": KNOWLEDGE,
        "config:": CONFIG,
    }

    @staticmethod
    def make_canonical_ref(kind: str, bare_id: str) -> str:
        """Build a canonical ref from kind + bare_id.

        Validates kind against _CANONICAL_PREFIXES. Raises ValueError on
        unknown kind or empty bare_id.

        >>> ItemType.make_canonical_ref("tool", "rye/bash/bash")
        'tool:rye/bash/bash'
        """
        valid_kinds = {v for v in ItemType._CANONICAL_PREFIXES.values()}
        if kind not in valid_kinds:
            raise ValueError(
                f"Unknown kind {kind!r}. Must be one of: {sorted(valid_kinds)}"
            )
        if not bare_id:
            raise ValueError(
                f"bare_id must not be empty (kind={kind!r})"
            )
        return f"{kind}:{bare_id}"

    @staticmethod
    def parse_canonical_ref(item_ref: str) -> tuple[Optional[str], str]:
        """Parse a canonical ref into (kind, bare_id).

        Returns (None, item_ref) when no prefix is present.
        Raises ValueError when a prefix is present but bare_id is empty.

        >>> ItemType.parse_canonical_ref("tool:rye/bash/bash")
        ('tool', 'rye/bash/bash')
        >>> ItemType.parse_canonical_ref("my/workflow")
        (None, 'my/workflow')
        """
        for prefix, kind in ItemType._CANONICAL_PREFIXES.items():
            if item_ref.startswith(prefix):
                bare_id = item_ref[len(prefix):]
                if not bare_id:
                    raise ValueError(
                        f"Canonical ref must include an ID after '{kind}:' "
                        f"(e.g. '{kind}:my/item')"
                    )
                return kind, bare_id
        return None, item_ref

    @staticmethod
    def require_canonical_ref(item_ref: str) -> tuple[str, str]:
        """Parse a canonical ref, raising if no kind prefix is present.

        >>> ItemType.require_canonical_ref("tool:rye/bash/bash")
        ('tool', 'rye/bash/bash')
        >>> ItemType.require_canonical_ref("my/workflow")
        Traceback (most recent call last):
            ...
        ValueError: ...
        """
        kind, bare_id = ItemType.parse_canonical_ref(item_ref)
        if kind is None:
            raise ValueError(
                f"Canonical ref required (e.g. 'tool:my/item'), got bare ID: {item_ref!r}"
            )
        return kind, bare_id


class NodeDir:
    """Node state directory constants.

    Node state lives at ~/.ai/node/ only — never in project space.
    These map node domain names to their subdirectory names.
    """

    DIR = "node"

    IDENTITY = "identity"
    ATTESTATION = "attestation"
    AUTHORIZED_KEYS = "authorized-keys"
    VAULT = "vault"
    EXECUTIONS = "executions"
    LOGS = "logs"

    # All valid node subdirectories
    ALL = [IDENTITY, ATTESTATION, AUTHORIZED_KEYS, VAULT, EXECUTIONS, LOGS]


class Action:
    """Tool action constants."""

    FETCH = "fetch"
    SIGN = "sign"
    EXECUTE = "execute"

    ALL = [FETCH, EXECUTE, SIGN]
