# Directive Metadata Reference

Complete specification of metadata fields for directives in Rye OS.

## Overview

Directives are XML-formatted workflow definitions with a required `<metadata>` section describing the directive's purpose, execution model, and resource requirements.

---

## Required Fields

### `<directive>` Root Element

```xml
<directive name="directive_name" version="1.0.0">
  <metadata>...</metadata>
</directive>
```

**Attributes:**

- `name` (required): snake-case identifier (e.g., `create_project`, `deploy_staging`). Must match file name.
- `version` (required): Semantic versioning format `X.Y.Z`

### `<description>`

**Type:** string  
**Required:** Yes

Brief, one-line description of what the directive does.

```xml
<description>Deploy staging environment and run integration tests</description>
```

### `<category>`

**Type:** string  
**Required:** Yes

Primary categorization for organizational purposes. Examples: `workflows`, `patterns`, `core`, `meta`, `implementation`. Must align with directory structure.

```xml
<category>workflows</category>
```

### `<author>`

**Type:** string  
**Required:** Yes

Creator/maintainer identifier.

```xml
<author>rye-team</author>
```

### `<model>` (Optional)

**Type:** element with required `tier` attribute and optional attributes  
**Required:** No

Specifies LLM model complexity tier for execution with fallback options. Only needed for directives that run on the Rye agent harness with model-specific execution.

```xml
<model tier="orchestrator" fallback="general" id="model-custom-id">
  Orchestrator-level directive with parallel execution capabilities
</model>
```

**Required attributes:**

- `tier` - Non-empty string, user-defined (e.g., `fast`, `general`, `balanced`, `reasoning`, `expert`, `orchestrator`)

**Optional attributes:**

- `fallback` - Model tier to use if primary fails
- `fallback_id` - Model ID to use if primary fails
- `id` - Optional identifier for specific model choice

**Text content:** Description of the directive's reasoning approach (1-2 sentences)

### `<permissions>` (Optional)

**Type:** element with permission rules  
**Purpose:** Declares capabilities this directive requires  
**Required:** No

Only enforced when directives run on the Rye agent harness. Simple directives can omit this.

```xml
<permissions>
  <read resource="filesystem" path="**/*" />
  <write resource="filesystem" path="tests/**" />
  <execute resource="shell" action="pytest" />
  <execute resource="kiwi-mcp" action="execute" />
</permissions>
```

**Permission elements:**

- `<read resource="..." path="..."/>` - Read access with optional path patterns
- `<write resource="..." path="..."/>` - Write access with optional path patterns
- `<execute resource="..." action="..."/>` - Execute permissions with optional action specification

**Resource types:**

- `filesystem` - File system with path patterns (glob syntax supported)
- `shell` - Shell command execution (action: command name or `*` for all)
- `tool` - Specific tool invocation by ID
- `kiwi-mcp` - Kiwi MCP system operations (action: `search`, `load`, `execute`, `publish`, `update`, `delete`, or `*`)

**Examples:**

```xml
<!-- File system access -->
<read resource="filesystem" path=".ai/directives/**/*.md" />
<write resource="filesystem" path="build/**" />

<!-- Shell commands -->
<execute resource="shell" action="pytest" />
<execute resource="shell" action="*" />  <!-- All shell commands -->

<!-- Kiwi MCP operations -->
<execute resource="kiwi-mcp" action="search" />
<execute resource="kiwi-mcp" action="*" />  <!-- All MCP operations -->

<!-- Specific tool -->
<execute resource="tool" id="deploy-kubernetes" />
```

### `<cost>` (Optional)

**Type:** element with budget tracking fields  
**Purpose:** Resource usage tracking and budget management  
**Required:** No

Only enforced when directives run on the Rye agent harness with cost tracking enabled.

```xml
<cost>
  <context estimated_usage="high" turns="15" spawn_threshold="5">
    12000
  </context>
  <duration>600</duration>
  <spend currency="USD">20.00</spend>
</cost>
```

**Sub-elements:**

**`<context>`** (Recommended)

- `estimated_usage` - Enum: `low`, `medium`, `high` - Expected context consumption level
- `turns` - Integer - Maximum number of LLM turns/iterations allowed
- `spawn_threshold` - Integer - Number of turns before spawning subagents
- Text content (required) - Max context tokens allowed (integer)

**`<duration>`** (Optional)

- Integer - Maximum execution time in seconds

**`<spend>`** (Optional)

- `currency` - Currency code (e.g., `USD`, `EUR`)
- Text content - Maximum spend amount (decimal number)

**Examples:**

```xml
<!-- Full example -->
<cost>
  <context estimated_usage="high" turns="20" spawn_threshold="5">
    15000
  </context>
  <duration>900</duration>
  <spend currency="USD">25.00</spend>
</cost>

<!-- Minimal example -->
<cost>
  <context estimated_usage="medium" turns="10" spawn_threshold="3">
    8000
  </context>
</cost>
```

---

## Optional Fields

### `<context>`

**Type:** element with permission rules  
**Purpose:** Declares capabilities this directive requires  
**Required:** Yes

```xml
<permissions>
  <read resource="filesystem" path="**/*" />
  <write resource="filesystem" path="tests/**" />
  <execute resource="shell" action="pytest" />
  <execute resource="kiwi-mcp" action="execute" />
</permissions>
```

**Permission elements:**

- `<read resource="..." path="..."/>` - Read access with optional path patterns
- `<write resource="..." path="..."/>` - Write access with optional path patterns
- `<execute resource="..." action="..."/>` - Execute permissions with optional action specification

**Resource types:**

- `filesystem` - File system with path patterns (glob syntax supported)
- `shell` - Shell command execution (action: command name or `*` for all)
- `tool` - Specific tool invocation by ID
- `kiwi-mcp` - Kiwi MCP system operations (action: `search`, `load`, `execute`, `publish`, `update`, `delete`, or `*`)

**Examples:**

```xml
<!-- File system access -->
<read resource="filesystem" path=".ai/directives/**/*.md" />
<write resource="filesystem" path="build/**" />

<!-- Shell commands -->
<execute resource="shell" action="pytest" />
<execute resource="shell" action="*" />  <!-- All shell commands -->

<!-- Kiwi MCP operations -->
<execute resource="kiwi-mcp" action="search" />
<execute resource="kiwi-mcp" action="*" />  <!-- All MCP operations -->

<!-- Specific tool -->
<execute resource="tool" id="deploy-kubernetes" />
```

---

## Optional Fields

### `<context>`

**Type:** element  
**Purpose:** Contextual information for execution and relationships

```xml
<context>
  <related_files>
    - src/handlers/directive.py
    - tests/directive_handler_test.py
  </related_files>
  <dependencies>
    - pytest>=7.0
    - asyncio
  </dependencies>
  <requires>subagent</requires>
  <depends_on>another-directive</depends_on>
  <used_by>orchestrator-directive</used_by>
  <suggests>related-directive</suggests>
</context>
```

**Sub-elements:**

**Metadata:**

- `<related_files>` - Relevant file paths
- `<dependencies>` - External dependencies

**Relationships:**

- `<requires>` - Required capability/directive
- `<depends_on>` - Depends on another directive
- `<used_by>` - This directive is used by (can appear multiple times)
- `<suggests>` - Optionally suggests related directive
- `<conflicts_with>` - Incompatible with directive
- `<example_of>` - Is an example of pattern/base directive

### `<hooks>`

**Type:** element containing hook definitions triggered by a harness on an llm thread
**Purpose:** Define conditional actions to execute on events/conditions via MCP

```xml
<hooks>
  <hook>
    <when>cost.current > cost.limit * 0.8</when>
    <execute item_type="directive">warn-cost-threshold</execute>
  </hook>

  <hook>
    <when>loop_count > 3</when>
    <execute item_type="directive">handle-loop-detected</execute>
    <inputs>
      <loop_count>${loop_count}</loop_count>
      <original_directive>${directive.name}</original_directive>
    </inputs>
  </hook>
</hooks>
```

**Hook structure:**

- `<when>` (required) - Expression evaluated against event context
- `<execute>` (required) - MCP execution with `item_type` attribute and item name as text content
- `<inputs>` (optional) - Parameters to pass using `${variable}` substitution

**Execute types:**

- `item_type="directive"` - Execute a directive
- `item_type="tool"` - Execute a tool
- `item_type="knowledge"` - Execute/load knowledge entry

**Available context variables:**

- `cost.current` / `cost.limit` - Cost tracking
- `loop_count` - Number of loop iterations detected
- `error.type` / `error.resource` - Error context (permission denied, etc.)
- `deviation.type` / `deviation.message` - Deviation/exception details
- `directive.name` - Name of current directive
- Custom variables from directive state

**Execution examples:**

```xml
<!-- Execute directive on cost threshold -->
<hook>
  <when>cost.current > cost.limit * 0.9</when>
  <execute item_type="directive">warn-cost-critical</execute>
</hook>

<!-- Execute directive with inputs on error -->
<hook>
  <when>error.type == "permission_denied"</when>
  <execute item_type="directive">request-elevated-permissions</execute>
  <inputs>
    <requested_resource>${error.resource}</requested_resource>
    <original_directive>${directive.name}</original_directive>
  </inputs>
</hook>

<!-- Load knowledge entry on deviation -->
<hook>
  <when>deviation.type == "schema_mismatch"</when>
  <execute item_type="knowledge">schema-mismatch-resolution</execute>
</hook>

<!-- Execute tool for schema repair -->
<hook>
  <when>deviation.type == "schema_mismatch"</when>
  <execute item_type="tool">fix-schema-deviation</execute>
  <inputs>
    <deviation_details>${deviation.message}</deviation_details>
    <auto_fix>true</auto_fix>
  </inputs>
</hook>

<!-- Deployment failure rollback directive -->
<hook>
  <when>deviation.type == "rollback_required"</when>
  <execute item_type="directive">handle-deployment-failure</execute>
  <inputs>
    <rollback_to_version>previous</rollback_to_version>
    <ask_user>true</ask_user>
  </inputs>
</hook>
```

---

## Minimal Example (v1.0)

```xml
<directive name="deploy-staging" version="1.0.0">
  <metadata>
    <description>Deploy application to staging environment</description>
    <category>workflows</category>
    <author>devops-team</author>
  </metadata>

  <process>
    <step name="build">
      <description>Build application</description>
      <action><![CDATA[
npm run build
      ]]></action>
    </step>
    <step name="deploy">
      <description>Deploy to staging</description>
      <action><![CDATA[
npm run deploy:staging
      ]]></action>
    </step>
  </process>

  <success_criteria>
    <criterion>Build completes successfully</criterion>
    <criterion>Staging deployment is accessible</criterion>
  </success_criteria>

  <outputs>
    <output name="deployment_url">Final URL of deployed application</output>
    <output name="status">Deployment status (success/failed)</output>
    <output name="logs">Build and deployment log file path</output>
  </outputs>
</directive>
```

---

## Complete Example (Advanced)

```xml
<directive name="deploy-and-test" version="1.0.0">
  <metadata>
    <description>Deploy to staging and run integration tests with parallel verification</description>
    <category>workflows</category>
    <author>devops-team</author>

    <model tier="orchestrator" fallback="general">
      Multi-agent deployment orchestration with parallel health checks
    </model>

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

    <permissions>
      <read resource="filesystem" path="k8s/**,scripts/**" />
      <write resource="filesystem" path="build/**,logs/**" />
      <execute resource="shell" action="docker" />
      <execute resource="shell" action="kubectl" />
      <execute resource="kiwi-mcp" action="execute" />
      <execute resource="kiwi-mcp" action="search" />
    </permissions>

    <cost>
      <context estimated_usage="high" turns="20" spawn_threshold="5">
        100000
      </context>
      <duration>900</duration>
      <spend currency="USD">30.00</spend>
    </cost>

    <hooks>
      <hook>
        <when>cost.current > cost.limit * 0.9</when>
        <execute item_type="directive">warn-cost-critical</execute>
      </hook>
      <hook>
        <when>loop_count > 3</when>
        <execute item_type="directive">handle-loop-detected</execute>
        <inputs>
          <original_directive>${directive.name}</original_directive>
        </inputs>
      </hook>
      <hook>
        <when>error.type == "permission_denied"</when>
        <execute item_type="directive">request-elevated-permissions</execute>
        <inputs>
          <requested_resource>${error.resource}</requested_resource>
        </inputs>
      </hook>
      <hook>
        <when>deviation.type == "rollback_required"</when>
        <execute item_type="directive">handle-deployment-failure</execute>
        <inputs>
          <rollback_to_version>previous</rollback_to_version>
          <ask_user>true</ask_user>
        </inputs>
      </hook>
    </hooks>
  </metadata>

  <!-- Inputs, process, outputs follow -->
</directive>
```

---

## Validation Rules

1. **`name` and `version`** are required attributes on root element
2. **`description`, `category`, `author`** are required metadata fields
3. **`model`, `permissions`, `cost`** are optional (only used by Rye agent harness)
4. **`version`** must be valid semantic version (X.Y.Z)
5. **`model tier`** is user-defined string (if model specified)
6. **`category`** is free-form string (must align with directory structure)
7. **`permissions`** must have at least one permission element (if specified)
8. **`cost`** must have at least a `<context>` sub-element with `estimated_usage` and `turns` attributes (if specified)
9. **`outputs`** (if present) must contain one or more `<output name="...">` elements, where `name` is the output identifier and element text is the description
10. **Permission paths** support glob patterns (e.g., `**/*.py`)
11. **Relationship elements** must be direct children of `<context>`
12. **Hook `<when>` expressions** evaluated against context variables

---

## Best Practices

### Naming

- Use snake-case: `my_directive`, not `MyDirective` or `my-directive`
- Be descriptive and action-oriented: `deploy-production`, not `deploy`
- Include outcomes: `sync-and-publish`, not just `sync`

### Descriptions

- One line, imperative tense: "Deploy to staging and run tests"
- Include main outcome in description
- Avoid redundancy with directive name

### Model Selection

- Choose tier based on reasoning complexity required
- Provide meaningful fallback for robustness

### Permissions

- Declare minimal required permissions (principle of least privilege)
- Use glob patterns for flexible paths (e.g., `.ai/directives/**/*.md`)
- Group related resources together
- Document why each permission is needed

### Context

- List all related files and dependencies
- Use relationships to build knowledge graph
- Document how directive fits into larger system

### Cost Tracking

- Always define `<context>` in cost section
- Set realistic `turns` based on expected complexity
- Set `spawn_threshold` to control subagent spawning
- Include `<spend>` for cost-conscious directives

### Hooks

- Use hooks to handle failure conditions gracefully
- Keep hook directives small and focused
- Provide fallback tier in hook inputs for model diversity
- Document hook triggering conditions clearly

---

## References

- [RYE MCP Permission Model]()
- [Safety Harness Design]()
