```yaml
id: directives
title: "Authoring Directives"
description: How to write directive files — the workflow instructions that AI agents follow
category: authoring
tags: [directives, authoring, format, metadata]
version: "1.1.0"
```

# Authoring Directives

Directives are workflow definitions that AI agents follow. They're XML-in-Markdown files stored in `.ai/directives/` that describe **what to do** — the sequence of steps, what tools to call, what inputs to accept, and what permissions are needed.

## File Structure

A directive file has this layout:

```
Line 1:  Signature comment (added by rye_sign)
         Markdown title and description
         XML fence containing ONLY metadata, inputs, outputs
         Process steps AFTER the fence (the LLM reads these)
```

The critical distinction: the XML fence is **infrastructure metadata** — the parser extracts limits, permissions, model config, and inputs from it. The process steps are **LLM instructions** — natural language that the agent reads and follows.

## Anatomy of a Directive

````markdown
<!-- rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID -->

# Directive Title

Description of what this directive does.

```xml
<directive name="directive_name" version="1.0.0">
  <metadata>
    <description>What this directive does</description>
    <category>category/path</category>
    <author>author-name</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits max_turns="6" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <directive>*</directive>
      </search>
      <load>
        <knowledge>category/*</knowledge>
      </load>
      <sign>
        <directive>*</directive>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="param_name" type="string" required="true" default="value">
      Description of this input
    </input>
  </inputs>

  <outputs>
    <output name="result_name" type="string">Description</output>
  </outputs>
</directive>
```

<process>
  <step name="step_name">
    Natural language instructions the LLM follows.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={...})`
  </step>
</process>

<success_criteria>
<criterion>Measurable success condition</criterion>
</success_criteria>
````

## Key Rules

1. **Line 1 is the signature comment** — added by `rye_sign`, never written manually
2. **The XML fence contains ONLY metadata, inputs, outputs** — infrastructure reads this
3. **Process steps are AFTER the fence** — the LLM reads these as instructions
4. **The XML is NOT parsed by the LLM** — it reads it as structured text. The parser extracts metadata for infrastructure (limits, permissions, model, inputs)
5. **Input interpolation** — `{input:name}` in process steps gets replaced with actual values at execution time
6. **Category matches directory** — `category: rye/core` means the file lives at `.ai/directives/rye/core/`

## Metadata Fields

### Required

| Field         | Purpose                                 | Example                          |
| ------------- | --------------------------------------- | -------------------------------- |
| `name`        | Directive identifier (matches filename) | `create_directive`               |
| `version`     | Semantic version                        | `1.0.0`                          |
| `description` | What the directive does                 | `Create a simple directive file` |
| `category`    | Directory path in `.ai/directives/`     | `rye/core`                       |
| `author`      | Creator                                 | `ryeos`                          |
| `model`       | LLM tier for execution                  | `<model tier="haiku" />`         |
| `permissions` | Capability declarations                 | See below                        |

### Model Configuration

The `<model>` element controls which LLM tier runs the directive:

```xml
<!-- Simple task — cheap and fast -->
<model tier="low" />

<!-- With specific model ID -->
<model tier="haiku" id="claude-3-5-haiku-20241022" />

<!-- Complex orchestration — needs reasoning -->
<model tier="orchestrator" fallback="general" />
```

Common tiers:

- **low / haiku** — cheap/fast for simple tasks (file writes, searches)
- **sonnet / general** — reasoning for moderate orchestration
- **orchestrator** — complex multi-step workflows with subagent spawning

### Limits

```xml
<!-- Simple directive -->
<limits turns="6" tokens="4096" />

<!-- Threaded directive with cost control -->
<limits max_turns="15" max_tokens="200000" />
```

For threaded directives, complexity maps to defaults:

- **simple**: max_turns=6, max_tokens=4096, spend=$0.05
- **moderate**: max_turns=15, max_tokens=200000, spend=$0.50
- **complex**: max_turns=30, max_tokens=200000, spend=$1.00

### Permissions

Permissions declare what capabilities the directive needs. They use a hierarchical structure with four primary actions:

```xml
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>        <!-- Execute any file-system tool -->
    <tool>rye.agent.threads.spawn_thread</tool>  <!-- Execute specific tool -->
  </execute>
  <search>
    <directive>*</directive>              <!-- Search all directives -->
    <tool>rye.registry.*</tool>           <!-- Search registry tools -->
  </search>
  <load>
    <knowledge>rye/core/*</knowledge>     <!-- Load core knowledge entries -->
  </load>
  <sign>
    <directive>*</directive>              <!-- Sign any directive -->
  </sign>
</permissions>
```

**Capability string format:** `rye.{primary}.{item_type}.{item_id_dotted}` with fnmatch wildcards.

Wildcard shortcuts:

```xml
<permissions>*</permissions>              <!-- God mode — all permissions -->
<execute>*</execute>                      <!-- Execute everything -->
<search>*</search>                        <!-- Search everything -->
```

### Inputs and Outputs

```xml
<inputs>
  <input name="name" type="string" required="true">
    Directive name in snake_case (e.g., "deploy_app")
  </input>
  <input name="timeout" type="integer" required="false" default="120">
    Timeout in seconds
  </input>
</inputs>

<outputs>
  <output name="directive_path">Path to the created file</output>
  <output name="signed">Whether signing succeeded</output>
</outputs>
```

Input values are interpolated in process steps as `{input:name}`. Defaults are supported with both `{input:name:default}` and `{input:name|default}` (colon and pipe separators work identically).

### How Outputs Become `<returns>` in the Prompt

When a directive is executed via `thread_directive`, the `<outputs>` block from the XML fence is **not** sent to the LLM as-is. Instead, the infrastructure deterministically transforms it into a `<returns>` block appended to the end of the prompt body. This tells the LLM what structured output to produce.

**What you write** (in the XML fence):

```xml
<outputs>
  <output name="directive_path">Path to the created file</output>
  <output name="signed">Whether signing succeeded</output>
</outputs>
```

**What the LLM sees** (appended after process steps):

```xml
<returns>
  <output name="directive_path">Path to the created file</output>
  <output name="signed">Whether signing succeeded</output>
</returns>
```

The transformation handles two formats:

| `outputs` format                    | Behavior                                                                                   |
| ----------------------------------- | ------------------------------------------------------------------------------------------ |
| List of `{name, description}` dicts | Each becomes `<output name="...">description</output>` (or self-closing if no description) |
| Dict of `{key: value}` pairs        | Each becomes `<output name="key">value</output>`                                           |

Parent threads and orchestrators match these output keys when consuming child thread results, so the names must be consistent between the directive's `<outputs>` declaration and what the parent expects.

## Process Steps

Process steps go **after** the XML fence. They contain natural language instructions and tool calls.

### XML Format

Structured XML with action elements:

```markdown
<process>
  <step name="check_duplicates">
    Search for existing directives with a similar name to avoid duplicates.
    `rye_search(item_type="directive", query="{input:name}")`
  </step>

  <step name="write_file">
    Generate the directive and write it to .ai/directives/{input:category}/{input:name}.md
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "...", "content": "..."})`
  </step>

  <step name="sign">
    Validate and sign the new directive.
    `rye_sign(item_type="directive", item_id="{input:name}")`
  </step>
</process>
```

### Markdown Format

Plain markdown with backtick-wrapped tool calls:

```markdown
## Process

**Check for duplicates**
Search for existing directives with a similar name to avoid duplicates.
`rye_search(item_type="directive", query="{input:name}")`

**Write directive file**
Generate the directive and write it to .ai/directives/{input:category}/{input:name}.md
`rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "...", "content": "..."})`

**Sign the directive**
Validate and sign the new directive.
`rye_sign(item_type="directive", item_id="{input:name}")`
```

Steps can also use XML action elements for richer structure:

```xml
<step name="check_duplicates">
  <description>Search for existing directives.</description>
  <search item_type="directive" query="{input:name}" />
</step>

<step name="write_file">
  <description>Write the directive file.</description>
  <execute item_type="tool" item_id="rye/file-system/fs_write">
    <param name="path" value=".ai/directives/{input:category}/{input:name}.md" />
    <param name="content" value="file content here" />
  </execute>
</step>
```

## Optional Metadata

### Cost Tracking (for threaded directives)

```xml
<cost>
  <context estimated_usage="high" turns="20" spawn_threshold="5">
    100000
  </context>
  <duration>900</duration>
  <spend currency="USD">30.00</spend>
</cost>
```

### Context and Relationships

```xml
<context>
  <related_files>
    - scripts/deploy.py
    - tests/integration/
  </related_files>
  <requires>subagent</requires>
  <depends_on>build-images</depends_on>
  <suggests>monitoring-setup</suggests>
</context>
```

### Hooks (event-driven actions)

```xml
<hooks>
  <hook id="warn_cost" event="after_step">
    <condition path="cost.spend" op="gte" value="0.9" />
    <action primary="execute" item_type="directive" item_id="warn-cost-critical" />
  </hook>
  <hook id="handle_permission_denied" event="error">
    <condition path="error.type" op="eq" value="permission_denied" />
    <action primary="execute" item_type="directive" item_id="request-elevated-permissions">
      <param name="requested_resource">${error.resource}</param>
    </action>
  </hook>
</hooks>
```

## Real Examples

### Simple Directive: `create_directive`

From `ryeos/rye/.ai/directives/rye/core/create_directive.md`:

````markdown
# Create Directive

Create minimal directives with essential fields only.

```xml
<directive name="create_directive" version="2.0.0">
  <metadata>
    <description>Create a simple directive file with minimal required fields, check for duplicates, write to disk, and sign it.</description>
    <category>rye/core</category>
    <author>ryeos</author>
    <model tier="low" />
    <limits turns="6" tokens="4096" />
    <permissions>
      <search>
        <directive>*</directive>
      </search>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <sign>
        <directive>*</directive>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="name" type="string" required="true">
      Directive name in snake_case (e.g., "deploy_app", "create_component")
    </input>
    <input name="category" type="string" required="true">
      Directory path relative to .ai/directives/ (e.g., "workflows", "testing")
    </input>
    <input name="description" type="string" required="true">
      What does this directive do? Be specific and actionable.
    </input>
    <input name="process_steps" type="string" required="false">
      Brief summary of process steps (bullet points)
    </input>
  </inputs>

  <process>
    <step name="check_duplicates">
      <description>Search for existing directives with a similar name to avoid creating duplicates.</description>
      <search item_type="directive" query="{input:name}" />
    </step>

    <step name="validate_inputs">
      <description>Validate that name is snake_case alphanumeric, category is non-empty, and description is non-empty. Halt if any validation fails.</description>
    </step>

    <step name="write_directive_file">
      <description>Generate the directive markdown file and write it to .ai/directives/{input:category}/{input:name}.md</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value=".ai/directives/{input:category}/{input:name}.md" />
        ...
      </execute>
    </step>

    <step name="sign_directive">
      <description>Validate XML and generate a signature for the new directive file.</description>
      <sign item_type="directive" item_id="{input:name}" />
    </step>
  </process>

  <success_criteria>
    <criterion>No duplicate directive with the same name exists</criterion>
    <criterion>Directive file created at .ai/directives/{input:category}/{input:name}.md</criterion>
    <criterion>All required XML elements present and well-formed</criterion>
    <criterion>Signature validation comment present at top of file</criterion>
  </success_criteria>

  <outputs>
    <success>Created directive: {input:name} at .ai/directives/{input:category}/{input:name}.md (v1.0.0). Run it to test, or edit steps and re-sign.</success>
    <failure>Failed to create directive: {input:name}. Check that name is snake_case, category path is valid, and XML is well-formed.</failure>
  </outputs>
</directive>
```
````

**What to notice:**

- `tier="low"` — this is a simple task, use cheap model
- Minimal permissions: search directives, execute file-system tools, sign directives
- Process steps use XML action elements (`<search>`, `<execute>`, `<sign>`)
- Success criteria are measurable conditions
- Outputs provide both success and failure messages

### Threaded Directive: `create_threaded_directive`

From `ryeos/rye/.ai/directives/rye/core/create_threaded_directive.md`:

````markdown
<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->

# Create Threaded Directive

Create a directive with full thread execution support — model configuration, cost limits, capability permissions for autonomous thread-based execution via thread_directive.

```xml
<directive name="create_threaded_directive" version="2.0.0">
  <metadata>
    <description>Creates directives with full thread execution support — model configuration, cost limits, capability permissions for autonomous thread-based execution.</description>
    <category>rye/core</category>
    <author>rye</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits max_turns="8" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <directive>*</directive>
      </search>
      <load>
        <directive>*</directive>
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
      Comma-separated capability strings (e.g., rye.execute.tool.rye.file-system.*,rye.search.directive.*)
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
    `rye_search(item_type="directive", query="{input:name} {input:category}")`
  </step>

  <step name="load_reference">
    Load an example threaded directive to use as a structural reference.
    `rye_load(item_type="directive", item_id="rye/core/create_threaded_directive")`
  </step>

  <step name="determine_limits">
    Map {input:complexity} to default limits:
    - simple: max_turns=6, max_tokens=4096, spend=0.05
    - moderate: max_turns=15, max_tokens=200000, spend=0.50
    - complex: max_turns=30, max_tokens=200000, spend=1.00
  </step>

  <step name="write_directive">
    Generate the directive and write it to .ai/directives/{input:category}/{input:name}.md
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={...})`
  </step>

  <step name="sign_directive">
    `rye_sign(item_type="directive", item_id="{input:category}/{input:name}")`
  </step>
</process>

<success_criteria>
<criterion>Model tier, limits, and permissions correctly configured for {input:complexity}</criterion>
<criterion>Permissions parsed from {input:permissions_needed} into hierarchical XML entries</criterion>
<criterion>Process steps present after the XML fence</criterion>
<criterion>Signature validation passed</criterion>
</success_criteria>
````

**What to notice:**

- Signature comment on line 1
- Process steps use backtick-wrapped `rye_*()` calls (natural language style)
- The `load_reference` step loads itself as a structural template
- `complexity` input maps to different limit configurations
- `permissions_needed` is parsed from comma-separated strings into hierarchical XML

## Directive Inheritance with `extends`

Directives can inherit from a parent directive using the `extends` attribute on the `<directive>` element:

```xml
<directive name="deploy_staging" version="1.0.0" extends="rye/agent/core/base">
  ...
</directive>
```

When a directive extends a parent, the thread setup walks the `extends` chain (leaf → parent → … → root) and composes:

- **Context** — `<context>` items from the entire chain are merged, root-first. Base layers come first, overlays from the leaf are appended.
- **Capabilities** — taken from the leaf directive (most restrictive). The parent's capabilities are not automatically inherited unless the leaf omits its own `<permissions>` block.
- **Hooks** — hooks from parent directives participate in the merge alongside project and system hooks.

Circular chains are detected and rejected at startup:

```
Circular extends chain: rye/agent/core/base (chain: deploy_staging → base_deploy → rye/agent/core/base)
```

## Context Injection with `<context>`

The `<context>` metadata section declares knowledge items to inject into the LLM prompt, or suppresses hook-driven context layers. Items are loaded at thread startup and merged with hook-injected context:

```xml
<directive name="deploy_staging" version="1.0.0" extends="rye/agent/core/base">
  <metadata>
    <context>
      <system>rye/agent/core/identity</system>
      <system>rye/agent/core/behavior</system>
      <before>project/deploy/environment-rules</before>
      <after>project/deploy/completion-checklist</after>
      <suppress>tool-protocol</suppress>
    </context>
    ...
  </metadata>
  ...
</directive>
```

### Positions

| Position      | Where it goes                                           | Use case                                |
| ------------- | ------------------------------------------------------- | --------------------------------------- |
| `<system>`    | Appended to the system message (after hook layers)      | Extra system-level instructions         |
| `<before>`    | Between hook before-context and directive body           | Domain rules, project conventions       |
| `<after>`     | Between directive body and hook after-context             | Checklists, extra completion rules      |
| `<suppress>`  | Skips the named hook-driven context layer                | Replace default layers with custom ones |

### Suppressing Context Layers

`<suppress>` skips a hook-driven context layer by matching against the hook's `id` field or the action's full knowledge `item_id`:

```xml
<context>
  <suppress>system_tool_protocol</suppress>
  <before>project/custom-tool-protocol</before>
</context>
```

This removes the default tool-protocol from the system message and injects a custom one in the user message instead. Suppressions compose through `extends` — if any directive in the chain suppresses a layer, it stays suppressed.

### Composition through `extends`

When a directive extends a parent, context items from the entire chain are merged root-first. Duplicates are deduplicated — if both the base and the leaf declare the same knowledge item, it appears only once. Suppressions are unioned across the chain.

```
Chain: rye/agent/core/base → project/deploy/base → deploy_staging

System items:  [identity, behavior]          ← from rye/agent/core/base
Before items:  [environment-rules]           ← from project/deploy/base
After items:   [completion-checklist]        ← from deploy_staging
Suppressions:  [tool-protocol]              ← from deploy_staging
```

See [Context Injection](../orchestration/context-injection.md) for the full system overview including project-level customization via conditional hooks.

## Acknowledging Capability Risks

When a directive requires capabilities classified as `elevated` or `unrestricted`, you must explicitly acknowledge the risk in the `<permissions>` block using `<acknowledge>`:

```xml
<permissions>
  <execute>
    <tool>rye.bash.*</tool>
  </execute>
  <acknowledge risk="elevated">
    This directive executes shell commands to run the build pipeline.
  </acknowledge>
</permissions>
```

Without the `<acknowledge>` tag:
- **`elevated`** capabilities log a warning but still execute (`acknowledge_required` policy)
- **`unrestricted`** capabilities are **blocked** and the thread fails with an error suggesting you add the acknowledgment

The `risk` attribute must match one of the risk tiers defined in `capability_risk.yaml`: `safe`, `write`, `elevated`, or `unrestricted`.

See [Permissions and Capabilities — Capability Risk Classification](../orchestration/permissions-and-capabilities.md#capability-risk-classification) for the full risk model.

## Example: Extending a Base with Context and Risk Acknowledgment

````markdown
<!-- rye:signed:2026-02-24T00:00:00Z:placeholder:unsigned:unsigned -->

# Deploy Staging

Deploy the current build to the staging environment.

```xml
<directive name="deploy_staging" version="1.0.0" extends="rye/agent/core/base">
  <metadata>
    <description>Deploy the current build to staging</description>
    <category>project/deploy</category>
    <author>team</author>
    <model tier="sonnet" />
    <limits max_turns="15" max_tokens="200000" />
    <context>
      <system>rye/agent/core/identity</system>
      <before>project/deploy/environment-rules</before>
      <after>project/deploy/completion-checklist</after>
    </context>
    <permissions>
      <execute>
        <tool>rye.bash.*</tool>
        <tool>rye.file-system.*</tool>
      </execute>
      <load>
        <knowledge>project/deploy/*</knowledge>
      </load>
      <acknowledge risk="elevated">
        Needs shell access to run deploy scripts and health checks.
      </acknowledge>
    </permissions>
  </metadata>

  <inputs>
    <input name="target" type="string" required="true">
      Deployment target (e.g., "staging-us-east-1")
    </input>
  </inputs>
</directive>
```

<process>
  <step name="load_config">
    Load the deployment configuration for {input:target}.
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"path": ".ai/config/deploy/{input:target}.yaml"})`
  </step>

  <step name="run_deploy">
    Execute the deployment script.
    `rye_execute(item_type="tool", item_id="rye/bash/bash", parameters={"command": "./scripts/deploy.sh {input:target}"})`
  </step>
</process>
````

**What to notice:**

- `extends="rye/agent/core/base"` — inherits base context (identity, behavior) from the parent
- `<context>` — adds project-specific knowledge in `before` and `after` positions
- `<acknowledge risk="elevated">` — explicitly allows shell execution
- Capabilities are tightly scoped to bash and file-system tools

## Best Practices

- **Principle of least privilege** — only declare the permissions the directive actually needs
- **One fence, one process** — metadata goes inside the XML fence, instructions go outside
- **Measurable success criteria** — each criterion should be verifiable
- **Action-oriented names** — `deploy_staging`, not `staging` or `deployment_handler`
- **Always sign** — the last step should be `rye_sign` to validate the file

## References

- [Directive Metadata Reference](../../ryeos/rye/.ai/knowledge/rye/core/directive-metadata-reference.md)
- [Terminology](../../ryeos/rye/.ai/knowledge/rye/core/terminology.md)
