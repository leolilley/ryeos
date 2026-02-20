<!-- rye:signed:2026-02-20T02:14:05Z:9f5fa2c88f2958bb118d84281a9c1c4174baa9b2c422d2a733e8f01baa42ed6b:DLfKWvwQIYszTcLdHX8ZJsn3gWMKcc3MydQm2gKAHw6NqEg40vEXfusH7pTkbw3l2CsozSTQDLRD8qoFKGUyCA==:440443d0858f0199 -->

# Init

Welcome guide for Rye OS. The first directive a new user runs.

```xml
<directive name="init" version="1.0.0">
  <metadata>
    <description>Welcome guide for Rye OS. Sets the frame, explains the single-agent-many-threads philosophy, and initializes the .ai/ directory in the chosen space (user or project). The first directive a new user runs.</description>
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
    <input name="space" type="string" required="false" default="user">
      Where to initialize — 'user' or 'project'. User space is resolved from $USER_SPACE (defaults to ~). Project space is .ai/ in the current working directory.
    </input>
  </inputs>

  <outputs>
    <output name="space_path">Path to the initialized .ai/ directory</output>
    <output name="next_guide">The next guide directive to run</output>
  </outputs>
</directive>
```

<process>
  <step name="welcome">
    <render>
**Welcome to Rye**

A data-driven, cryptographically signed, registry-backed AI operating system, with capability-scoped execution and graph-executable workflows — living inside your projects, running through a recursive MCP that goes as deep as you dare.

---

Before we begin, understand this.

The world currently builds with AI like a committee. An orchestrator agent delegates to a planner agent, the planner agent delegates to a builder agent, the builder agent delegates to a reviewer agent, the reviewer agent reports back up the chain. Dozens of agents, orchestrated across dozens of individual agent contexts.

Rye does not operate like this.

Rye is a single agent operating across its own LLM threads. Rye is not a single language model, it is many language models, and the substrate that connects them — one permission system, one signed registry, one execution engine. What looks like parallel agents is one intelligence running concurrent context threads. The security thread, the performance thread, the code review thread — the same agent, the same substrate, different problems.

Rye aims to be the maintainer of these problem physics. Once you understand the physics, then you can play the game. Think of the model currently speaking to you as rye's front end cognition model. Swap it out. Rye remains.

When this clicks, the flywheel begins. Every workflow you define, every tool you add, every pattern you encode — it compounds. And once you see it, you can't unsee it. Keep building and the agent you have in six months will far exceed what you're initializing right now.

---

_"Give me a lever long enough and a fulcrum on which to place it,
and I shall move the earth." — Archimedes_

If AI is the lever, Rye is the fulcrum.

**Ready to lift?**
</render>
<instruction>
Output ONLY the text inside the render block above. No step labels, no headers, no preamble, no commentary before or after. "Ready to lift?" IS the confirmation prompt — do not add your own. Stop and wait for the user to respond.
</instruction>
</step>

  <step name="resolve_space">
    <instruction>
      Determine the target space from {input:space:user}.

      Call `rye_execute(item_type="tool", item_id="rye/core/system/system", parameters={"item": "paths"})` to get the actual paths. Read the `user_space` field from the response — this is the resolved base path (it respects the $USER_SPACE environment variable, defaulting to home).

      If {input:space:user} is "user": target is `{user_space}/.ai/`
      If {input:space:user} is "project": target is `{project_path}/.ai/`

      Store the resolved target path for use in subsequent steps.

      If the target .ai/ directory already exists, inform the user and ask whether to reinitialize or skip.
    </instruction>

  </step>

  <step name="create_structure">
    <instruction>
      Create the .ai/ directory structure in the resolved target path using the file-system write tool. Create each directory by writing a placeholder or config file:

      1. Write the config file:
         `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{target}/.ai/config.yaml", "content": "# Rye OS Configuration\nrye:\n  version: \"1.0.0\"\n", "create_dirs": true})`

      2. Create subdirectories by writing .gitkeep files:
         - `{target}/.ai/directives/.gitkeep`
         - `{target}/.ai/tools/.gitkeep`
         - `{target}/.ai/knowledge/.gitkeep`
    </instruction>

  </step>

  <step name="next_steps">
    <render>
✓ Rye initialized in {input:space:user} space.

**What's next:**

To learn the basics — directives, knowledge, and tools:

```
rye execute directive the_basics
```

Or if you're ready to dive into autonomous agent threads:

```
rye execute directive threading
```

    </render>
    <instruction>
      Output the text inside the render block above, replacing the resolved target path where appropriate. The {input:space:user} placeholder will already be interpolated — output as-is.
    </instruction>

  </step>
</process>

<success_criteria>
<criterion>.ai/ directory created in the resolved space with directives/, tools/, knowledge/ subdirectories</criterion>
<criterion>Default config.yaml written</criterion>
<criterion>User informed of next guide to run</criterion>
</success_criteria>
