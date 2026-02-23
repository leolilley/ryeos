```yaml
id: quickstart
title: Quickstart
description: Create your first directive, tool, and knowledge entry in under 5 minutes
category: getting-started
tags: [quickstart, tutorial, first-steps]
version: "1.0.0"
```

# Quickstart

This guide walks you through creating one of each item type — a directive, a tool, and a knowledge entry — in your project's `.ai/` directory.

**Prerequisites:** Rye OS is installed and configured as an MCP server ([Installation](installation.md)).

## 1. Initialize the `.ai/` directory

Create the directory structure in your project root:

```bash
mkdir -p .ai/directives .ai/tools .ai/knowledge
```

This gives you:

```
your-project/
└── .ai/
    ├── directives/
    ├── tools/
    └── knowledge/
```

## 2. Create a directive

Directives are Markdown files with embedded XML metadata that define multi-step workflows for your agent.

Create `.ai/directives/greet_user.md`:

````markdown
<!-- rye:signed:placeholder:unsigned:unsigned -->

# Greet User

Greet the user by name and offer to help with their project.

```xml
<directive name="greet_user" version="1.0.0">
<metadata>
<description>Greet the user by name and offer assistance.</description>
<category></category>
<author>my-project</author>
<model tier="haiku" />
<limits max_turns="3" max_tokens="1024" />
<permissions>
<search>
<knowledge>\*</knowledge>
</search>
</permissions>
</metadata>

  <inputs>
    <input name="user_name" type="string" required="true">
      The name of the user to greet
    </input>
  </inputs>

  <outputs>
    <output name="greeting">The greeting message delivered to the user</output>
  </outputs>
</directive>
```

<process>
  <step name="greet">
    Say hello to {input:user_name} and ask what they'd like help with today.
  </step>

  <step name="suggest">
    Based on the project context, suggest 2-3 things you could help with.
  </step>
</process>
````

**Key parts of a directive:**

- **Signature comment** — the first line (`<!-- rye:signed:placeholder:unsigned:unsigned -->`) is a signing placeholder.
- **Markdown title and description** — human-readable context above the XML fence.
- **XML metadata block** — inside a fenced code block, declares name, version, model tier, resource limits, and permissions.
- **Process steps** — after the XML fence, `<process>` defines the steps the agent follows.

**Outside the XML fence is free form. We choose to express directive process steps as psudo xml**

## 3. Create a tool

Tools are executable scripts with metadata that declare which **runtime executor** to use. The Rye OS uses a **multi-layer execution chain**:

1. **Tool** (your script) — declares `__executor_id__` pointing to a runtime
2. **Runtime** (YAML config) — defines environment, interpreter, and command templates
3. **Primitive** (built-in) — maps to system-level execution (e.g., subprocess)

This chain allows tools to be language-agnostic while sharing infrastructure.

### Python Tool Example

Create `.ai/tools/word_count.py`:

```python
"""Count words in a file or string."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = ""
__tool_description__ = "Count words in a given text or file"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "text": {
            "type": "string",
            "description": "Text to count words in",
        },
        "file_path": {
            "type": "string",
            "description": "Path to a file to count words in (relative to project root)",
        },
    },
}


def execute(params: dict, project_path: str) -> dict:
    """Execute the word count tool.

    The executor runs: python word_count.py --params '{"text":"..."}' --project-path /path
    """
    text = params.get("text")
    file_path = params.get("file_path")

    if file_path:
        from pathlib import Path
        full_path = Path(project_path) / file_path
        if not full_path.exists():
            return {"error": f"File not found: {file_path}"}
        text = full_path.read_text()

    if not text:
        return {"error": "Provide either 'text' or 'file_path'"}

    words = text.split()
    lines = text.splitlines()

    return {
        "word_count": len(words),
        "line_count": len(lines),
        "char_count": len(text),
    }
```

**Key parts of a Python tool:**

- **`__executor_id__`** — points to `rye/core/runtimes/python/script`, which resolves `.venv`, sets `PYTHONPATH`, and runs the script.
- **`CONFIG_SCHEMA`** — JSON Schema declaring input parameters.
- **`execute(params, project_path)`** — entry point called by the runtime. Receives validated params and project root.

### JavaScript / Node Tool Example

Create `.ai/tools/text_stats.js`:

```javascript
/**
 * Analyze text for length, reading time, and complexity metrics.
 */

const __version__ = "1.0.0";
const __tool_type__ = "javascript";
const __executor_id__ = "rye/core/runtimes/node/node";
const __category__ = "";
const __tool_description__ = "Analyze text statistics and reading metrics";

const CONFIG_SCHEMA = {
  type: "object",
  properties: {
    text: {
      type: "string",
      description: "Text to analyze",
    },
    file_path: {
      type: "string",
      description: "Path to file to analyze (relative to project root)",
    },
  },
};

async function execute(params, projectPath) {
  /**
   * The executor runs: node text_stats.js
   * (with params passed via stdin or env, depending on runtime config)
   */
  const text = params.text;
  const filePath = params.file_path;

  let content = text;
  if (filePath) {
    const fs = require("fs");
    const path = require("path");
    const fullPath = path.join(projectPath, filePath);
    try {
      content = fs.readFileSync(fullPath, "utf-8");
    } catch (err) {
      return { error: `File not found: ${filePath}` };
    }
  }

  if (!content) {
    return { error: "Provide either 'text' or 'file_path'" };
  }

  const words = content.trim().split(/\s+/).length;
  const sentences = content
    .split(/[.!?]+/)
    .filter((s) => s.trim().length > 0).length;
  const paragraphs = content
    .split(/\n\n+/)
    .filter((p) => p.trim().length > 0).length;
  const readingTimeMinutes = Math.ceil(words / 200); // avg 200 words per minute

  return {
    word_count: words,
    sentence_count: sentences,
    paragraph_count: paragraphs,
    reading_time_minutes: readingTimeMinutes,
    character_count: content.length,
  };
}

// Export for the runtime executor
if (typeof module !== "undefined" && module.exports) {
  module.exports = { execute, CONFIG_SCHEMA };
}
```

**Key parts of a JavaScript tool:**

- **`__executor_id__`** — points to `rye/core/runtimes/node/node`, which finds `node` or uses `.bin/node`, and runs the script.
- **`CONFIG_SCHEMA`** — same JSON Schema format as Python.
- **`execute(params, projectPath)`** — async function receiving validated params and project root.

### How Tool Execution Works

When you call `rye_execute(item_type="tool", item_id="word_count", ...)`:

1. **Resolution** — Rye OS loads the tool script and reads `__executor_id__`.
2. **Runtime Lookup** — Finds the runtime config (e.g., `python/script.yaml`).
3. **Environment Setup** — Resolves interpreter (`.venv/bin/python` or `node`), sets env vars, prepends `PYTHONPATH` or `NODE_PATH`.
4. **Execution** — Runs the command template with your params: `python word_count.py --params '{"text":"..."}' --project-path /path`.
5. **Result** — Collects stdout/stderr, parses the returned dict, and returns to the agent.

## 4. Create a knowledge entry

Knowledge entries are Markdown files with YAML frontmatter. They store domain information, patterns, and learnings that agents can search and reference.

Create `.ai/knowledge/project_conventions.md`:

```markdown
---
id: project_conventions
title: Project Conventions
description: Coding conventions and standards for this project
category: ""
tags:
  - conventions
  - standards
  - style
entry_type: reference
version: "1.0.0"
---

# Project Conventions

## File naming

- Python files: `snake_case.py`
- Markdown docs: `kebab-case.md`
- Test files: `test_<module>.py`

## Code style

- Use type hints on all public functions
- Docstrings on all public classes and functions
- Maximum line length: 100 characters

## Git workflow

- Branch names: `feature/<description>`, `fix/<description>`
- Commit messages: imperative mood, 72-char subject line
```

**Key parts of a knowledge entry:**

- **YAML frontmatter** — `id`, `title`, `description`, `category`, `tags`, `entry_type`, and `version`.
- **Markdown body** — the actual knowledge content. Can be as long as needed.
- **`entry_type`** — classifies the entry (e.g., `reference`, `learning`, `pattern`, `guide`).

## 5. Sign your items

Before items can be executed or loaded, they must be signed. Signing validates the item's structure and records an integrity hash.

```
rye_sign(item_type="directive", item_id="greet_user", project_path="/path/to/your/project")
rye_sign(item_type="tool", item_id="word_count", project_path="/path/to/your/project")
rye_sign(item_type="knowledge", item_id="project_conventions", project_path="/path/to/your/project")
```

The `item_id` is the relative path from `.ai/<type>/` without the file extension. For items in subdirectories, include the path: `"my-category/my-item"`.

### Execute vs Load

**For knowledge entries, there's an important distinction:**

- **`rye_execute(item_type="knowledge", ...)`** — Returns _only the content_ (the Markdown body). Keeps context tight. Use this when you want the knowledge to flow into the agent's reasoning without metadata overhead.

- **`rye_load(item_type="knowledge", ...)`** — Returns the _entire file_ including YAML frontmatter and metadata. Use this for inspection, copying between spaces, or when you need the full context including tags, version, and entry type.

## 6. Execute items

Run your directive:

```
rye_execute(
    item_type="directive",
    item_id="greet_user",
    project_path="/path/to/your/project",
    parameters={"user_name": "Alice"}
)
```

Run your tool:

```
rye_execute(
    item_type="tool",
    item_id="word_count",
    project_path="/path/to/your/project",
    parameters={"text": "Hello world, this is a test."}
)
```

Run your knowledge (feed content into reasoning):

```
rye_execute(
    item_type="knowledge",
    item_id="project_conventions",
    project_path="/path/to/your/project"
)
```

This returns only the Markdown content:

```
# Project Conventions

## File naming

- Python files: `snake_case.py`
- Markdown docs: `kebab-case.md`
- Test files: `test_<module>.py`

## Code style

- Use type hints on all public functions
- Docstrings on all public classes and functions
- Maximum line length: 100 characters

## Git workflow

- Branch names: `feature/<description>`, `fix/<description>`
- Commit messages: imperative mood, 72-char subject line
```

**Note:** If you need the full file with metadata (frontmatter), use `rye_load()` instead:

```
rye_load(
    item_type="knowledge",
    item_id="project_conventions",
    project_path="/path/to/your/project"
)
```

This returns the complete file including YAML frontmatter and metadata.

## 7. Search for items

Find items across all spaces (project, user, and system):

```
rye_search(scope="directive", query="greet", project_path="/path/to/your/project")
rye_search(scope="tool", query="count", project_path="/path/to/your/project")
rye_search(scope="knowledge", query="conventions", project_path="/path/to/your/project")
```

You can narrow the scope with namespace prefixes:

```
rye_search(scope="tool.rye.core.*", query="create", project_path="/path/to/your/project")
```

## What's next

- [The .ai/ Directory](ai-directory.md) — Learn about the full directory structure, namespaces, and the 3-tier space system.
- Explore the system bundle: `rye_search(scope="directive", query="create", project_path=".")` to see the built-in directives for creating new items.
