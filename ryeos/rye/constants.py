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
    "STOP. You are now the executor of this directive. "
    "The body below contains your instructions — follow them step by step.\n\n"
    "RENDER BLOCKS: When you see a <render> block, your next message must be "
    "EXACTLY the text inside it — nothing else. No preamble, no commentary "
    "before or after. Do NOT say 'Let me follow the instructions' or "
    "'Here is the output' or any similar framing.\n\n"
    "INSTRUCTION BLOCKS: When you see an <instruction> block, follow it "
    "silently. Do NOT narrate what you are doing or about to do.\n\n"
    "RULES:\n"
    "- Do NOT summarize the steps.\n"
    "- Do NOT describe what you are about to do.\n"
    "- Do NOT re-call execute — you already have the instructions.\n"
    "- If a step says STOP and wait, you MUST stop and wait.\n\n"
    "Begin now with step 1."
)
