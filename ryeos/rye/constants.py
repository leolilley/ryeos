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

    # Type directory mappings
    TYPE_DIRS = {
        DIRECTIVE: "directives",
        TOOL: "tools",
        KNOWLEDGE: "knowledge",
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


# Instruction injected into the thread runner's prompt when executing a directive.
# Used by thread_directive._build_prompt() to instruct the LLM.
DIRECTIVE_INSTRUCTION = "Execute the directive as specified now."
