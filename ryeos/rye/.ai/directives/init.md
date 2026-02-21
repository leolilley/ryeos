<!-- rye:signed:2026-02-21T06:11:17Z:cf69cb2a2d3dec5870d97320dd9120a827c2b8452f5edd2768f659cbafbdc33c:ndjxRNR9P9jKQ6ZABmVwYRZlcIEez-NGxKhh22yjU4ayr0oqhtXQupRR0yI8fOKZy88sHNqRZEZIw9Dpwpo8BA==:9fbfabe975fa5a7f -->

# Init

Welcome guide for Rye OS. The first directive a new user runs.

```xml
<directive name="init" version="1.0.0">
  <metadata>
    <description>Welcome guide for Rye OS. Initializes the .ai/ directory. The first directive a new user runs.</description>
    <category></category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="6" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
        <tool>rye.core.system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="project_type" type="string" required="false" default="user">
      What to initialize. 'user' sets up user space ($USER_SPACE/.ai/) with the welcome guide. 'project' sets up project space (.ai/ in project_path). Future values may route to language/framework-specific init templates.
    </input>
  </inputs>

  <outputs>
    <output name="space_path">Path to the initialized .ai/ directory</output>
    <output name="next_guide">The next guide directive to run</output>
  </outputs>
</directive>
```

<process>
  <step name="welcome" condition="only when {input:project_type:user} is 'user'">
    <render>
**Welcome to Rye**

A data-driven, cryptographically signed, registry-backed AI operating system, with capability-scoped execution and graph-executable workflows — living inside your projects, running through a recursive MCP that goes as deep as you dare.

---

Before we begin, understand this.

The world currently builds with AI like a committee. An orchestrator agent delegates to a planner agent, the planner agent delegates to a builder agent, the builder agent delegates to a reviewer agent, the reviewer agent reports back up the chain. Dozens of agents, orchestrated across dozens of individual agent contexts.

Rye does not operate like this.

Rye is a single agent operating across its own LLM threads. Rye is not a single language model, it is many language models, and the substrate that connects them — one permission system, one signed registry, one execution engine. What looks like parallel agents is one intelligence running concurrent context threads. The security thread, the performance thread, the code review thread — the same agent, the same substrate, different problems.

Rye aims to be the maintainer of these problem physics. Once you understand the physics, then you can play the game. Think of the model currently speaking to you now as rye's front end cognition model. Swap it out. Rye remains.

When this clicks, the flywheel begins. Every workflow you define, every tool you add, every pattern you encode — it compounds. And once you see it, you can't unsee it. Keep building and the agent you have in six months will far exceed what you're initializing right now.

---

_"Give me a lever long enough and a fulcrum on which to place it,
and I shall move the earth." — Archimedes_

If AI is the lever, Rye is the fulcrum.

**Ready to lift?**
</render>
<instruction>
SKIP this step entirely if {input:project_type:user} is "project".
Otherwise, output ONLY the text inside the render block above. No step labels, no headers, no preamble, no commentary before or after. "Ready to lift?" IS the confirmation prompt — do not add your own. Stop and wait for the user to respond.
</instruction>
</step>

  <step name="setup_user_space" condition="only when {input:project_type:user} is 'user'">
    <instruction>
      SKIP this step if {input:project_type:user} is "project".
      This runs after the user responds to "Ready to lift?"

      If the user confirms ("yes", "ready", "let's go", "lift", etc.):
        Output ONLY: "Good. Now let me set up your user space."
        Then immediately proceed to create_structure — do NOT wait for another user response.

      If the user declines, hesitates, or asks questions:
        Output the render block below and stop. Do NOT proceed to create_structure.
    </instruction>
    <render>

No worries. I'll be here when you're ready.

When you want to pick this up again, you know what to do.
</render>
</step>

  <step name="create_structure">
    <instruction>
      If {input:project_type:user} is "project":
        The target is the project_path provided in the execute call.

      If {input:project_type:user} is "user":
        Call `rye_execute(item_type="tool", item_id="rye/core/system/system", parameters={"item": "paths"})` to resolve the user_space path (respects $USER_SPACE env var, defaults to home). The target is the resolved user_space.

      If the target .ai/ directory already exists, inform the user and ask whether to reinitialize or skip.

      Before creating the structure, output the render block below, replacing {target} with the resolved path. No other commentary.
    </instruction>
    <render>
User space is {target}. Setting up Rye now.
    </render>
    <instruction>
      <rule>You MUST use rye_execute to call the file-system write tool. Do NOT use shell commands (mkdir, touch, bash). The write tool auto-creates parent directories.</rule>
      <rule>The write tool rejects paths outside project_path. For user space init, you MUST pass project_path={target} so the write paths are within scope.</rule>

      Create all four .gitkeep files in a single batch call — copy this exactly, replacing {target} with the resolved path:
      <tool_call>rye_execute(item_type="tool", item_id="rye/file-system/write", project_path="{target}", parameters={"files": [{"file_path": "{target}/.ai/config/.gitkeep", "content": ""}, {"file_path": "{target}/.ai/directives/.gitkeep", "content": ""}, {"file_path": "{target}/.ai/tools/.gitkeep", "content": ""}, {"file_path": "{target}/.ai/knowledge/.gitkeep", "content": ""}]})</tool_call>
    </instruction>

  </step>

  <step name="confirm">
    <instruction>
      If {input:project_type:user} is "project": output only "Rye initialized in project space." and stop.
      If {input:project_type:user} is "user": output the render block below EXACTLY and in FULL. Do not truncate, summarize, or rephrase. The code blocks contain commands the user must copy — they must appear verbatim.
    </instruction>
    <render>
Ready to go.

Now let's get moving. Here are the guides — run them in order, or jump ahead:

**The basics** — tools, directives, knowledge, and the three-tier space system:
```
rye execute directive the_basics
```

**Core utilities** — file system tools, search, and system introspection:
```
rye execute directive core_utils
```

**MCP discovery** — connecting external MCP servers:
```
rye execute directive mcp_discovery
```

**Registry** — publishing, pulling, and sharing items:
```
rye execute directive registry
```

**Advanced tools** — building data-driven tools with extractors and runtimes:
```
rye execute directive advanced_tools
```

Or jump to where it gets interesting:

**Threading** — concurrent LLM threads, orchestration, and agent substrates:
```
rye execute directive threading
```

**Graphs** — state graph workflows and executable pipelines:
```
rye execute directive graphs
```

    </render>
    <instruction>
      When the user runs any of the above commands, map the short name to the full item_id under rye/guides/. For example:
      - "the_basics" → item_id="rye/guides/the_basics"
      - "threading" → item_id="rye/guides/threading"
      - "graphs" → item_id="rye/guides/graphs"
      - "core_utils" → item_id="rye/guides/core_utils"
      - "mcp_discovery" → item_id="rye/guides/mcp_discovery"
      - "registry" → item_id="rye/guides/registry"
      - "advanced_tools" → item_id="rye/guides/advanced_tools"
    </instruction>

  </step>
</process>

<success_criteria>
<criterion>.ai/ directory created in the resolved space with config/, directives/, tools/, knowledge/ subdirectories</criterion>
<criterion>User informed of next guide to run (user space only)</criterion>
</success_criteria>
