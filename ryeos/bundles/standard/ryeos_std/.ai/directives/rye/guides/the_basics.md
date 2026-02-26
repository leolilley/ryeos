<!-- rye:signed:2026-02-26T05:02:40Z:cf1eb75bf45bdf65f8b47323294e9f6f4c013cb3d4f8c5a63bf0b54239362ad3:1FxMOPQRw8q04lWzKmHn4mjteukRmzqAhF_wOTgN0nCC4qeBwK2i7T5T4o4CpkmBahPImTYtQE5i0NMwwV8KCw==:4b987fd4e40303ac -->

# The Basics

While Rye is not technically an 'OS' in the traditional sense, it does help to think of it as one.

**In Linux, everything is a file. In Rye, everything is data.**

This guide introduces you to the primary items and tools that power Rye.

```xml
<directive name="the_basics" version="1.0.0">
  <metadata>
    <description>Guide 2: Introduction to the three primary items (directives, knowledge, tools) and four primary tools (execute, load, search, sign) across three-tier space resolution (project → user → system).</description>
    <category>rye/guides</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="20" tokens="8192" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
        <tool>rye.core.system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
  </inputs>

  <outputs>
    <output name="understanding">User understands three primary items, four primary tools, and how they interact</output>
  </outputs>
</directive>
```

<process>
  <step name="intro">
    <render>
## Three Primary Items

Every piece of automation in Rye is one of three types:

1. **Directives** — Workflows that guide the LLM through steps. This guide is a directive.
2. **Tools** — Executables (Python, bash, etc.) that perform actions.
3. **Knowledge** — Reference documentation and domain data.

All three live in `.ai/` directories and resolve across three spaces: **project** (your `.ai/`) → **user** (shared cross-project) → **system** (Rye built-ins). Project shadows user shadows system.

## Four Primary Tools

Every interaction with items uses one of four tools:

| Tool      | What it does                              |
| --------- | ----------------------------------------- |
| `execute` | Run a directive, tool, or knowledge item  |
| `load`    | Read item content or copy between spaces  |
| `search`  | Find items by keyword across all spaces   |
| `sign`    | Cryptographically sign items with Ed25519 |

## How They Connect

Each item type works with all four primary tools. All commands are prefaced with `rye`:

| Command             | Example                                            |
| ------------------- | -------------------------------------------------- |
| `execute directive` | `rye execute directive the_basics`                 |
| `execute tool`      | `rye execute tool rye/bash/bash`                   |
| `execute knowledge` | `rye execute knowledge rye/core/three-tier-spaces` |
| `load directive`    | `rye load directive init`                          |
| `load tool`         | `rye load tool rye/bash/bash`                      |
| `load knowledge`    | `rye load knowledge rye/core/three-tier-spaces`    |
| `search directive`  | `rye search directive init`                        |
| `search tool`       | `rye search tool bash`                             |
| `search knowledge`  | `rye search knowledge spaces`                      |
| `sign directive`    | `rye sign directive my-project/hello`              |
| `sign tool`         | `rye sign tool my-project/hello`                   |
| `sign knowledge`    | `rye sign knowledge my-project/guide`              |

Or with your own custom intent language via `AGENTS.md` or your harness equivalent.

**Does this make sense so far? Ready to get hands-on?**
</render>
<instruction>
Output ONLY the render block above. No preamble, no commentary. Then wait for the user to confirm they understand before proceeding.
</instruction>
</step>

  <step name="directives_explore">
    <instruction>
      Search for the init directive to show how discovery works:
      <tool_call>rye_search(scope="directive", query="init")</tool_call>

      Then load it to inspect its content:
      <tool_call>rye_load(item_type="directive", item_id="init")</tool_call>

      After the results come back, output the render block below.
    </instruction>
    <render>

That's a directive. The XML metadata block declares what it is — name, version, permissions, inputs, outputs. The `<process>` steps tell the LLM what to do and what to say. The `<render>` blocks are verbatim text. The `<instruction>` blocks are behavioral rules.

This is data, not code. The LLM reads it and follows it. You're reading one right now.
</render>
</step>

  <step name="directives_customize">
    <instruction>
      Copy the init directive to user space for customization:
      <tool_call>rye_load(item_type="directive", item_id="init", destination="user")</tool_call>

      Then output the render block below.

      Then sign the copy:
      <tool_call>rye_sign(item_type="directive", item_id="init", source="user")</tool_call>
    </instruction>
    <render>

Now you have your own copy in user space. You can edit it, change the welcome message, add steps — whatever you want. Project space overrides user space, user space overrides system space. This is the three-tier resolution order.
</render>
</step>

  <step name="knowledge_explore">
    <instruction>
      Search for knowledge entries about spaces:
      <tool_call>rye_search(scope="knowledge", query="spaces")</tool_call>

      Then load a knowledge entry:
      <tool_call>rye_execute(item_type="knowledge", item_id="rye/core/three-tier-spaces")</tool_call>

      After the results come back, output the render block below.
    </instruction>
    <render>

Knowledge entries are reference docs that ship with Rye. They have YAML metadata and markdown content. When you need to understand how something works, search knowledge first — it's your built-in manual.
</render>
</step>

  <step name="tools_create">
    <instruction>
      First resolve the user_space path:
      <tool_call>rye_execute(item_type="tool", item_id="rye/core/system/system", parameters={"item": "paths"})</tool_call>

      Then create a hello world tool in user space using the write tool. The file content is:

      ```python
      """Hello world — your first Rye tool."""

      __version__ = "1.0.0"
      __tool_type__ = "python"
      __executor_id__ = "rye/core/runtimes/python_script_runtime"
      __category__ = "hello"
      __tool_description__ = "A simple hello world tool"

      import json
      import sys
      import argparse

      def main():
          parser = argparse.ArgumentParser()
          parser.add_argument("--params", required=True)
          parser.add_argument("--project-path", required=True)
          args = parser.parse_args()
          params = json.loads(args.params)
          name = params.get("name", "world")
          print(json.dumps({"success": True, "message": f"Hello, {name}!"}))

      if __name__ == "__main__":
          main()
      ```

      <tool_call>rye_execute(item_type="tool", item_id="rye/file-system/write", project_path="{user_space}", parameters={"path": "{user_space}/.ai/tools/hello/hello.py", "content": "\"\"\"Hello world — your first Rye tool.\"\"\"\n\n__version__ = \"1.0.0\"\n__tool_type__ = \"python\"\n__executor_id__ = \"rye/core/runtimes/python_script_runtime\"\n__category__ = \"hello\"\n__tool_description__ = \"A simple hello world tool\"\n\nimport json\nimport sys\nimport argparse\n\ndef main():\n    parser = argparse.ArgumentParser()\n    parser.add_argument(\"--params\", required=True)\n    parser.add_argument(\"--project-path\", required=True)\n    args = parser.parse_args()\n    params = json.loads(args.params)\n    name = params.get(\"name\", \"world\")\n    print(json.dumps({\"success\": True, \"message\": f\"Hello, {name}!\"}))\n\nif __name__ == \"__main__\":\n    main()\n"})</tool_call>

      <rule>You MUST use rye_execute to call the file-system write tool. Do NOT use shell commands.</rule>
      <rule>The write tool rejects paths outside project_path. Pass project_path={user_space}.</rule>
      <rule>Replace {user_space} with the actual path from the system tool output.</rule>

      After creating the file, output the render block below.
    </instruction>
    <render>

That's your first tool. A Python script with metadata in dunder variables. The `__executor_id__` tells Rye which runtime to use — in this case, `python_script_runtime`. The script reads `--params` as JSON and prints JSON back. That's the contract.
</render>
</step>

  <step name="tools_sign_and_run">
    <instruction>
      Sign the new tool:
      <tool_call>rye_sign(item_type="tool", item_id="hello/hello", source="user")</tool_call>

      Then execute it:
      <tool_call>rye_execute(item_type="tool", item_id="hello/hello", parameters={"name": "Rye"})</tool_call>

      After the results come back, output the render block below.
    </instruction>
    <render>

Every tool runs through a three-layer chain:

```
Layer 3: hello.py                    → executor: python_script_runtime
Layer 2: python_script_runtime.yaml  → executor: subprocess primitive
Layer 1: subprocess primitive        → Lilux (direct execution)
```

Your tool declares its executor. The executor declares its executor. The chain resolves down to a Lilux primitive that actually runs the process. Every layer is signed. Every layer is verified. This is why adding a new language to Rye is just a YAML file — you're adding a Layer 2 runtime, not writing framework code.
</render>
</step>

  <step name="next">
    <render>
Three item types. Three spaces. One resolution order. You've now used all of them.

Next up — the core utility tools that power the infrastructure:

```
rye execute directive core_utils
```

    </render>
    <instruction>
      Output ONLY the render block above. No commentary. Stop and wait.
    </instruction>

  </step>
</process>

<success_criteria>
<criterion>User has searched, loaded, and copied a directive (demonstrating directive discovery and three-tier override)</criterion>
<criterion>User has searched and loaded a knowledge entry (demonstrating knowledge discovery)</criterion>
<criterion>User has created, signed, and executed a tool in user space (demonstrating the full tool lifecycle)</criterion>
<criterion>User understands the three-layer executor chain</criterion>
</success_criteria>
