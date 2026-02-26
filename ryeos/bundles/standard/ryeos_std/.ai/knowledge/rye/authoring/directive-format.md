<!-- rye:signed:2026-02-26T05:02:40Z:92084e8dce2b8c2c341ab8d9fa2feef63692d92d4db1e464d63032fe8ccc05d7:oqB2Fbr9kFVHV8jyvRXWwtJII79JQZvj2QI6yGwv8Arr_Lh51pVqdzTIkeg3bhOIXSpaBl9VSl2PEPi8OBUaDw==:4b987fd4e40303ac -->

```yaml
name: directive-format
title: "Directive Format Specification"
entry_type: reference
category: rye/authoring
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - directives
  - format
  - authoring
  - metadata
  - specification
  - workflow
  - xml
  - steps
  - process
  - writing-directives
  - create-directive
references:
  - tool-format
  - knowledge-format
  - "docs/authoring/directives.md"
```

# Directive Format Specification

Canonical format and metadata reference for directive files — XML-in-Markdown workflow definitions stored in `.ai/directives/`.

## File Structure

```
Line 1:  Signature comment (added by rye_sign, never written manually)
         Blank line
         Markdown title (# heading) and description paragraph
         Blank line
         XML fence (```xml ... ```) containing ONLY metadata, inputs, outputs
         Blank line
         Process steps AFTER the fence (LLM reads these)
         Success criteria AFTER the fence
```

### The Two-Zone Rule

| Zone | Location | Purpose | Consumer |
|------|----------|---------|----------|
| **XML fence** | Inside ` ```xml ``` ` | Infrastructure metadata: limits, permissions, model, inputs, outputs | Parser / infrastructure |
| **Process steps** | After the fence | LLM instructions: natural language, tool calls, step logic | LLM agent |

The XML fence is **not parsed by the LLM** — it reads it as structured text. The parser extracts metadata for infrastructure (limits, permissions, model config, inputs). Process steps are what the agent actually follows.

## Signature Comment

Line 1 of every signed directive:

```
<!-- rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID -->
```

| Field | Format | Example |
|-------|--------|---------|
| `TIMESTAMP` | ISO 8601 | `2026-02-10T02:00:00Z` |
| `HASH` | SHA-256 hex | `ae410d018a0a8367...` |
| `SIGNATURE` | Base64url Ed25519 | `jMpCTdpY3HZY2c4p...` |
| `KEYID` | Hex key fingerprint | `440443d0858f0199` |

- Added by `rye_sign` — never write manually
- Unsigned placeholder: `<!-- rye:signed:TIMESTAMP:placeholder:unsigned:unsigned -->`

---

## Complete File Anatomy

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
    <cost>
      <context estimated_usage="medium" turns="10" spawn_threshold="3">8000</context>
      <duration>600</duration>
      <spend currency="USD">5.00</spend>
    </cost>
    <context>
      <related_files>
        - src/handlers/directive.py
      </related_files>
      <requires>subagent</requires>
      <depends_on>build-images</depends_on>
      <suggests>monitoring-setup</suggests>
    </context>
    <hooks>
      <hook>
        <when>cost.current > cost.limit * 0.9</when>
        <execute item_type="directive">warn-cost-critical</execute>
      </hook>
    </hooks>
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

---

## `<directive>` Root Element

```xml
<directive name="directive_name" version="1.0.0">
  <metadata>...</metadata>
  <inputs>...</inputs>
  <outputs>...</outputs>
</directive>
```

### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string (snake_case) | **Yes** | Identifier matching the filename |
| `version` | string (semver) | **Yes** | Semantic version `X.Y.Z` |

---

## Metadata Fields — Required

### `<description>`

**Type:** string
**Required:** Yes

One-line imperative description of what the directive does.

```xml
<description>Deploy staging environment and run integration tests</description>
```

### `<category>`

**Type:** string
**Required:** Yes

Directory path in `.ai/directives/`. Must match actual file location.

```xml
<category>rye/core</category>
<category>workflows</category>
```

### `<author>`

**Type:** string
**Required:** Yes

Creator/maintainer identifier.

```xml
<author>rye-os</author>
```

### `<model>`

**Type:** element
**Required:** Yes

Controls which LLM tier runs the directive.

```xml
<!-- Simple — cheap and fast -->
<model tier="low" />

<!-- With specific model ID -->
<model tier="haiku" id="claude-3-5-haiku-20241022" />

<!-- Complex orchestration with fallback -->
<model tier="orchestrator" fallback="general" />

<!-- With fallback model ID -->
<model tier="orchestrator" fallback="general" fallback_id="claude-3-5-sonnet-20241022" />
```

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `tier` | string | **Yes** | Model complexity tier (user-defined) |
| `id` | string | No | Specific model identifier |
| `fallback` | string | No | Fallback tier if primary fails |
| `fallback_id` | string | No | Fallback model ID if primary fails |

**Text content:** Optional 1–2 sentence description of reasoning approach.

**Common tiers:**

| Tier | Use Case |
|------|----------|
| `low` / `haiku` | Cheap/fast for simple tasks (file writes, searches) |
| `sonnet` / `general` | Moderate reasoning and orchestration |
| `orchestrator` | Complex multi-step workflows, subagent spawning |

### `<permissions>`

**Type:** element with hierarchical permission rules
**Required:** Yes

Declares capabilities this directive requires. Uses four primary actions with item-type children.

```xml
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>
    <tool>rye.agent.threads.spawn_thread</tool>
  </execute>
  <search>
    <directive>*</directive>
    <tool>rye.registry.*</tool>
  </search>
  <load>
    <knowledge>rye/core/*</knowledge>
  </load>
  <sign>
    <directive>*</directive>
  </sign>
</permissions>
```

**Primary elements:**

| Element | Purpose |
|---------|---------|
| `<execute>` | Run/execute items |
| `<search>` | Search for items |
| `<load>` | Load/read item content |
| `<sign>` | Sign/validate items |

**Item type children:** `<tool>`, `<directive>`, `<knowledge>`

**Capability string format:** `rye.{primary}.{item_type}.{item_id_dotted}` with fnmatch wildcards.

**Wildcard shortcuts:**

```xml
<permissions>*</permissions>              <!-- God mode — all permissions -->
<execute>*</execute>                      <!-- Execute everything -->
<search>*</search>                        <!-- Search everything -->
<load>*</load>                            <!-- Load everything -->
<sign>*</sign>                            <!-- Sign everything -->
<tool>rye.agent.*</tool>                  <!-- All tools under rye.agent -->
```

### `<cost>`

**Type:** element
**Required:** Yes

Resource usage tracking and budget management.

```xml
<cost>
  <context estimated_usage="high" turns="15" spawn_threshold="5">12000</context>
  <duration>600</duration>
  <spend currency="USD">20.00</spend>
</cost>
```

**Sub-elements:**

| Element | Required | Attributes | Text Content |
|---------|----------|------------|-------------|
| `<context>` | **Recommended** | `estimated_usage` (low/medium/high), `turns` (int), `spawn_threshold` (int) | Max context tokens (int) |
| `<duration>` | No | — | Max execution time in seconds (int) |
| `<spend>` | No | `currency` (e.g., USD) | Max spend amount (decimal) |

**Complexity defaults:**

| Complexity | `max_turns` | `max_tokens` | `spend` |
|-----------|------------|-------------|---------|
| simple | 6 | 4,096 | $0.05 |
| moderate | 15 | 200,000 | $0.50 |
| complex | 30 | 200,000 | $1.00 |

---

## Metadata Fields — Optional

### `<limits>`

Shorthand for turn/token limits (alternative to `<cost>`):

```xml
<limits turns="6" tokens="4096" />
<limits max_turns="15" max_tokens="200000" />
```

### `<context>`

Contextual information for execution and relationships.

```xml
<context>
  <related_files>
    - scripts/deploy.py
    - tests/integration/
  </related_files>
  <dependencies>
    - pytest>=7.0
    - asyncio
  </dependencies>
  <requires>subagent</requires>
  <depends_on>build-images</depends_on>
  <suggests>monitoring-setup</suggests>
</context>
```

**Sub-elements:**

| Element | Purpose |
|---------|---------|
| `<related_files>` | Relevant file paths (markdown list) |
| `<dependencies>` | External dependencies (markdown list) |

**Relationships (direct children of `<context>`):**

| Element | Purpose |
|---------|---------|
| `<requires>` | Required capability or directive |
| `<depends_on>` | Depends on another directive |
| `<used_by>` | This directive is used by (can appear multiple times) |
| `<suggests>` | Optionally suggests related directive |
| `<conflicts_with>` | Incompatible with directive |
| `<example_of>` | Is an example of pattern/base directive |

### `<hooks>`

Event-driven conditional actions, triggered by the harness on an LLM thread.

```xml
<hooks>
  <hook>
    <when>cost.current > cost.limit * 0.9</when>
    <execute item_type="directive">warn-cost-critical</execute>
  </hook>
  <hook>
    <when>error.type == "permission_denied"</when>
    <execute item_type="directive">request-elevated-permissions</execute>
    <inputs>
      <requested_resource>${error.resource}</requested_resource>
    </inputs>
  </hook>
  <hook>
    <when>deviation.type == "schema_mismatch"</when>
    <execute item_type="tool">fix-schema-deviation</execute>
    <inputs>
      <deviation_details>${deviation.message}</deviation_details>
      <auto_fix>true</auto_fix>
    </inputs>
  </hook>
</hooks>
```

**Hook structure:**

| Element | Required | Purpose |
|---------|----------|---------|
| `<when>` | **Yes** | Expression evaluated against event context |
| `<execute>` | **Yes** | Item to execute; `item_type` attribute = `directive`, `tool`, or `knowledge` |
| `<inputs>` | No | Parameters using `${variable}` substitution |

**Available context variables:**

| Variable | Description |
|----------|-------------|
| `cost.current` / `cost.limit` | Cost tracking |
| `loop_count` | Loop iterations detected |
| `error.type` / `error.resource` | Error context |
| `deviation.type` / `deviation.message` | Deviation/exception details |
| `directive.name` | Current directive name |

---

## Inputs

Declared inside the XML fence in the `<inputs>` block.

```xml
<inputs>
  <input name="name" type="string" required="true">
    Directive name in snake_case (e.g., "deploy_app")
  </input>
  <input name="timeout" type="integer" required="false" default="120">
    Timeout in seconds
  </input>
  <input name="tags" type="string" required="false">
    Comma-separated tags
  </input>
</inputs>
```

### Input Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | **Yes** | Parameter identifier |
| `type` | string | **Yes** | Data type: `string`, `integer`, `boolean`, `object`, `array` |
| `required` | boolean | No | Whether mandatory (default: `false`) |
| `default` | varies | No | Default value if not provided |

**Text content:** Description of the input parameter.

### Input Interpolation Syntax

Input values are interpolated in process steps using these patterns:

| Syntax | Behavior |
|--------|----------|
| `{input:name}` | Required — fails if not provided |
| `{input:name?}` | Optional — replaced with empty string if absent |
| `{input:name:default_value}` | Default — uses `default_value` if input absent |
| `{input:name\|default_value}` | Default — uses `default_value` if input absent (pipe syntax) |

```xml
<step name="write_file">
  Write to .ai/directives/{input:category}/{input:name}.md
  Use timeout {input:timeout:120} seconds.
  Output dir: {input:output_dir|outputs}
  Tags: {input:tags?}
</step>
```

---

## Outputs

Declared inside the XML fence in the `<outputs>` block.

```xml
<outputs>
  <output name="directive_path">Path to the created file</output>
  <output name="signed">Whether signing succeeded</output>
</outputs>
```

### How Outputs Become `<returns>` in the Prompt

When a directive is executed via `thread_directive`, the `<outputs>` block is **not** sent to the LLM as-is. Infrastructure deterministically transforms it into a `<returns>` block appended after the process steps:

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

**Transformation rules:**

| `outputs` format | Behavior |
|------------------|----------|
| List of `{name, description}` dicts | Each becomes `<output name="...">description</output>` |
| Dict of `{key: value}` pairs | Each becomes `<output name="key">value</output>` |

Parent threads and orchestrators match output keys when consuming child results — names must be consistent between the directive's `<outputs>` and what the parent expects.

### Legacy Output Format

Older directives may use success/failure messages:

```xml
<outputs>
  <success>Created directive: {input:name}</success>
  <failure>Failed to create directive: {input:name}</failure>
</outputs>
```

---

## Process Steps

Process steps go **after** the XML fence. They contain natural language instructions and tool calls that the LLM follows.

### XML Format (Structured)

```xml
<process>
  <step name="check_duplicates">
    Search for existing directives with a similar name.
    `rye_search(item_type="directive", query="{input:name}")`
  </step>

  <step name="write_file">
    Generate the directive and write it to .ai/directives/{input:category}/{input:name}.md
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={...})`
  </step>

  <step name="sign">
    Validate and sign the new directive.
    `rye_sign(item_type="directive", item_id="{input:name}")`
  </step>
</process>
```

### XML Format with Action Elements

```xml
<process>
  <step name="check_duplicates">
    <description>Search for existing directives to avoid duplicates.</description>
    <search item_type="directive" query="{input:name}" />
  </step>

  <step name="write_file">
    <description>Write the directive file.</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value=".ai/directives/{input:category}/{input:name}.md" />
    </execute>
  </step>

  <step name="sign_directive">
    <description>Validate and sign.</description>
    <sign item_type="directive" item_id="{input:name}" />
  </step>
</process>
```

### Tool Call Styles in Steps

| Style | Syntax |
|-------|--------|
| Backtick-wrapped | `` `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={...})` `` |
| XML action element | `<execute item_type="tool" item_id="rye/file-system/write">` |
| XML search | `<search item_type="directive" query="{input:name}" />` |
| XML sign | `<sign item_type="directive" item_id="{input:name}" />` |

---

## Success Criteria

Measurable conditions placed after the process steps:

```xml
<success_criteria>
  <criterion>No duplicate directive with the same name exists</criterion>
  <criterion>Directive file created at .ai/directives/{input:category}/{input:name}.md</criterion>
  <criterion>All required XML elements present and well-formed</criterion>
  <criterion>Signature validation comment present at top of file</criterion>
</success_criteria>
```

Each criterion should be **verifiable** — not vague ("it works") but specific ("file exists at path X").

---

## Validation Rules

1. `name` and `version` are required attributes on `<directive>` root element
2. `description`, `category`, `author`, `model`, `permissions`, `cost` are required metadata fields
3. `version` must be valid semantic version (`X.Y.Z`)
4. `model tier` attribute is required; value is user-defined string
5. `category` must align with directory structure under `.ai/directives/`
6. `permissions` must have at least one primary element (`<execute>`, `<search>`, `<load>`, `<sign>`) or `*` for god mode
7. `cost` must have at least a `<context>` sub-element with `estimated_usage` and `turns` attributes
8. Capability paths support wildcard `*` suffix (e.g., `rye.agent.*`)
9. Relationship elements must be direct children of `<context>`
10. Hook `<when>` expressions are evaluated against context variables
11. Input `name` must be unique within the `<inputs>` block
12. Process steps must appear **after** the XML fence, not inside it

---

## Best Practices

### Naming
- snake_case: `deploy_staging`, not `DeployStaging` or `deploy-staging`
- Action-oriented: `deploy_staging`, not `staging` or `deployment_handler`
- Include outcomes: `sync_and_publish`, not just `sync`

### Structure
- **One fence, one process** — metadata inside XML fence, instructions outside
- **Principle of least privilege** — only declare permissions the directive actually needs
- **Measurable success criteria** — each criterion should be verifiable
- **Always sign** — last step should be `rye_sign`
- Category matches directory: `category: rye/core` → `.ai/directives/rye/core/`

### Model Selection
- Choose tier based on reasoning complexity
- Provide meaningful fallback for robustness
- Use `low`/`haiku` for simple file operations
- Use `orchestrator` only for multi-step workflows with subagents

### Cost Tracking
- Always define `<context>` in cost section
- Set realistic `turns` based on expected complexity
- Set `spawn_threshold` to control subagent spawning
- Include `<spend>` for cost-conscious directives

### Hooks
- Use hooks to handle failure conditions gracefully
- Keep hook directives small and focused
- Document hook triggering conditions clearly

---

## Complete Example

````markdown
<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->

# Deploy and Test

Deploy to staging and run integration tests with parallel verification.

```xml
<directive name="deploy_and_test" version="1.0.0">
  <metadata>
    <description>Deploy to staging and run integration tests with parallel verification</description>
    <category>workflows</category>
    <author>devops-team</author>
    <model tier="orchestrator" fallback="general" />
    <permissions>
      <execute>
        <tool>rye.shell.docker</tool>
        <tool>rye.shell.kubectl</tool>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <directive>*</directive>
        <tool>*</tool>
      </search>
      <load>
        <tool>*</tool>
      </load>
    </permissions>
    <cost>
      <context estimated_usage="high" turns="20" spawn_threshold="5">100000</context>
      <duration>900</duration>
      <spend currency="USD">30.00</spend>
    </cost>
    <context>
      <related_files>
        - scripts/deploy.py
        - tests/integration/
        - k8s/staging.yaml
      </related_files>
      <requires>docker-runtime</requires>
      <depends_on>build-images</depends_on>
      <suggests>monitoring-setup</suggests>
    </context>
    <hooks>
      <hook>
        <when>cost.current > cost.limit * 0.9</when>
        <execute item_type="directive">warn-cost-critical</execute>
      </hook>
      <hook>
        <when>error.type == "permission_denied"</when>
        <execute item_type="directive">request-elevated-permissions</execute>
        <inputs>
          <requested_resource>${error.resource}</requested_resource>
        </inputs>
      </hook>
    </hooks>
  </metadata>

  <inputs>
    <input name="environment" type="string" required="true">
      Target environment (staging, production)
    </input>
    <input name="skip_tests" type="boolean" required="false" default="false">
      Skip integration tests after deployment
    </input>
  </inputs>

  <outputs>
    <output name="deployment_id">ID of the created deployment</output>
    <output name="test_results">Summary of test results</output>
  </outputs>
</directive>
```

<process>
  <step name="build_images">
    Build Docker images for the service.
    `rye_execute(item_type="tool", item_id="rye/shell/docker", parameters={"command": "build"})`
  </step>

  <step name="deploy">
    Deploy to {input:environment} using kubectl.
    `rye_execute(item_type="tool", item_id="rye/shell/kubectl", parameters={"command": "apply"})`
  </step>

  <step name="run_tests">
    Run integration tests unless {input:skip_tests} is true.
  </step>
</process>

<success_criteria>
<criterion>Docker images built successfully</criterion>
<criterion>Deployment to {input:environment} completed without errors</criterion>
<criterion>All integration tests passed (or skipped if skip_tests=true)</criterion>
</success_criteria>
````
