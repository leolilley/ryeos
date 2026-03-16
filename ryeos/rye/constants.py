"""RYE Constants

Centralized constants for the AI directory name, item types, and tool actions.
"""

# The name of the working directory used in all three spaces.
# Every space follows: base_path / AI_DIR / {type_dir} / {item_id}
AI_DIR = ".ai"


class ItemType:
    """Item type constants."""

    DIRECTIVE = "directive"
    TOOL = "tool"
    KNOWLEDGE = "knowledge"

    ALL = [DIRECTIVE, TOOL, KNOWLEDGE]

    # Items that can be signed (ALL + config files)
    CONFIG = "config"
    SIGNABLE = [DIRECTIVE, TOOL, KNOWLEDGE, CONFIG]

    # Type directory mappings (execute/load/search — NO config)
    TYPE_DIRS = {
        DIRECTIVE: "directives",
        TOOL: "tools",
        KNOWLEDGE: "knowledge",
    }

    # Extended mapping for signing and integrity (includes config)
    SIGNABLE_DIRS = {
        **TYPE_DIRS,
        CONFIG: "config",
    }

    # File extensions to search per item type (tools use dynamic lookup)
    CONTENT_EXTENSIONS = {
        DIRECTIVE: [".md"],
        KNOWLEDGE: [".md", ".yaml", ".yml"],
    }


class Action:
    """Tool action constants."""

    SEARCH = "search"
    SIGN = "sign"
    LOAD = "load"
    EXECUTE = "execute"

    ALL = [SEARCH, SIGN, LOAD, EXECUTE]
