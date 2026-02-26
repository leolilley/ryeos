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
# This is the single most important string for cross-model directive compliance.
# It must be explicit enough that even weak models follow the body as instructions
# rather than summarizing, describing, or re-executing.
DIRECTIVE_INSTRUCTION = (
    "ZERO PREAMBLE. Your very first output token must be directive content — "
    "never narration. Do NOT say 'I need to follow', 'Let me start', "
    "'Here is the output', or ANY framing text.\n\n"
    "You are the executor of this directive. Follow the body step by step.\n\n"
    "<render> → output EXACTLY the text inside. Nothing before, nothing after.\n"
    "<instruction> → follow silently. Do NOT narrate.\n\n"
    "RULES:\n"
    "- Do NOT summarize or describe what you are about to do.\n"
    "- Do NOT re-call execute — you already have the instructions.\n"
    "- If a step says STOP and wait, you MUST stop and wait.\n\n"
    "Begin now with step 1."
)
