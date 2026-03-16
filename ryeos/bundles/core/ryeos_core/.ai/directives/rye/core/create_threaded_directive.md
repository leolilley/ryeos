<!-- rye:signed:2026-03-16T09:53:44Z:a081b222bffc9714e906beccd866f563a210255708bd91bcad4014d422f1792f:98f8ZishzFy5e4xYmpgDixQ8rSgX5Q4jMS4ao1zaXqhoZKukLEXNzB6sdqhROn90TKJoyf82oKM6JwrNmUu4Bw==:4b987fd4e40303ac -->
# Create Threaded Directive

Create a directive with full thread execution support — model configuration, cost limits,
capability permissions for autonomous thread-based execution via thread_directive.

```xml
<directive name="create_threaded_directive" version="3.0.0">
  <metadata>
    <description>Creates directives with full thread execution support — model, limits, permissions, context for autonomous execution.</description>
    <category>rye/core</category>
    <author>rye</author>
    <model tier="fast" />
    <limits turns="8" tokens="4096" spend="0.10" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <directive>*</directive>
      </search>
      <load>
        <directive>*</directive>
        <knowledge>*</knowledge>
      </load>
      <sign>
        <directive>*</directive>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="name" type="string" required="true">
      Name of the threaded directive to create (snake_case)
    </input>
    <input name="category" type="string" required="true">
      Category path for the directive (e.g., rye/core, project/build)
    </input>
    <input name="description" type="string" required="true">
      What the threaded directive does
    </input>
    <input name="complexity" type="string" required="true">
      Complexity level: simple, moderate, or complex — determines default limits and turn counts
    </input>
    <input name="permissions_needed" type="string" required="true">
      Comma-separated capability strings (e.g., mcp.my-server.my_type.create,rye.file-system.*)
    </input>
    <input name="extends_from" type="string" required="false">
      Optional base directive to extend (e.g., my-project/agent/base). Inherits context and merges permissions.
    </input>
    <input name="process_steps" type="string" required="false">
      Optional summary of the steps the directive should perform
    </input>
  </inputs>

  <outputs>
    <output name="directive_path">Path to the created threaded directive file</output>
    <output name="signed">Whether the directive was successfully signed</output>
  </outputs>
</directive>
```

<process>
  <step name="search_existing">
    Search for similar existing directives to avoid duplication and gather patterns.
  </step>

  <step name="load_reference">
    Load an example threaded directive to use as a structural reference.
    Also load the directive-authoring knowledge for context/extends/permissions rules.
  </step>

  <step name="determine_limits">
    Map {input:complexity} to default limits:
    - simple: turns=5, tokens=20000, spend=0.05
    - moderate: turns=15, tokens=200000, spend=0.50
    - complex: turns=30, tokens=200000, spend=1.00
  </step>

  <step name="write_directive">
    Generate the directive and write it to .ai/directives/{input:category}/{input:name}.md

    The generated file must follow this structure:
    1. Markdown title and description
    2. A single ```xml fenced block containing the `<directive>` element with metadata, inputs, outputs
    3. Process steps AFTER the xml fence as `<process>` pseudo-XML

    ## Structural rules

    **XML block** contains ONLY:
    - `<metadata>` with description, category, author, model, limits, context (optional), permissions
    - `<inputs>` with typed input declarations
    - `<outputs>` with output declarations (use `required="false"` for optional outputs)

    **Process steps** go AFTER the xml fence. Describe WHAT to do, not HOW.
    The LLM sees its tools as flat API tools and infers usage from their schemas.
    Do NOT write `rye_execute(...)` or `rye_search(...)` calls in process steps.

    ## Permissions rules

    Permissions map directly to flat API tools the LLM will see. Scope them tightly:
    - `<tool>mcp.my-server.my_type.create</tool>` → LLM sees `mcp_my_server_my_type_create(...)`
    - `<tool>mcp.my-server.my_type.*</tool>` → LLM sees all tools under that namespace
    - `<tool>rye.file-system.*</tool>` → LLM sees `rye_file_system_read(...)`, `rye_file_system_write(...)`, etc.
    - Do NOT grant `mcp.my-server.*` if only specific operations are needed

    For MCP tools, ensure specific YAML tool wrappers exist at `.ai/tools/mcp/<server>/<type>/<action>.yaml`
    with `fixed_params` and `params_key` so the LLM gets exact parameter schemas.

    ## Context rules

    `<context>` controls what gets injected into the thread agent's prompt:
    - `<system>knowledge/item/id</system>` — sets the system prompt from a knowledge item
    - `<before>knowledge/item/id</before>` — injects content before the directive body
    - `<suppress>knowledge/item/id</suppress>` — removes context inherited from a parent directive

    ## Extends rules

    `extends="category/name"` inherits context and permissions from a parent directive:
    - Parent context items are injected unless suppressed
    - Child permissions are attenuated against parent (can only narrow, not widen)
    - Do NOT extend from `agent/core/base` unless you need the full Rye protocol docs
      (search/load/sign/execute protocol knowledge). Most focused directives should NOT
      extend from it — define their own identity context and permissions instead.
    - Use extends for shared identity (e.g., a base that sets `<system>` to an identity knowledge item)
    - Each leaf directive should own its permissions — the base provides context, not capabilities

    Parse {input:permissions_needed} into hierarchical `<permissions>` entries grouped by
    primary action (execute, search, load, sign). All tool permissions go under `<execute><tool>`.
    If {input:extends_from} is provided, add `extends="{input:extends_from}"` to the directive element.
    Use {input:process_steps} if provided to write the process steps.
  </step>

  <step name="sign_directive">
    Sign the created directive.
  </step>
</process>

<success_criteria>
  <criterion>No duplicate directive with the same name exists</criterion>
  <criterion>Directive file created at .ai/directives/{input:category}/{input:name}.md</criterion>
  <criterion>Model tier, limits (including spend), and permissions correctly configured</criterion>
  <criterion>Permissions scoped tightly to exactly the tools needed</criterion>
  <criterion>Process steps describe WHAT, not HOW — no rye_execute calls</criterion>
  <criterion>Context and extends configured correctly if specified</criterion>
  <criterion>Signature validation passed</criterion>
</success_criteria>
