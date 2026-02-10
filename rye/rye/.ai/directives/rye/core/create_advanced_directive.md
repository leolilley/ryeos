<!-- rye:validated:2026-02-03T07:29:34Z:609ab0782bebbbbc5e46682624e60e5da6e49a9b865a30f79649cc81cab39558 -->
# Create Directive

Create new directives with proper XML structure, validate, and sign.

```xml
<directive name="create_advanced_directive" version="1.0.0">
  <metadata>
    <description>Create directives with proper XML structure, validation, and signing.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="balanced" fallback="general">
      Standard workflow for directive creation
    </model>
    <context_budget>
      <estimated_usage>15%</estimated_usage>
      <step_count>5</step_count>
      <spawn_threshold>50%</spawn_threshold>
    </context_budget>
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
    <relationships>
      <suggests>edit_directive</suggests>
    </relationships>
  </metadata>

  <context>
    <tech_stack>any</tech_stack>
  </context>

  <inputs>
    <input name="name" type="string" required="true">
      Directive name in snake_case (e.g., "deploy_app", "create_component", "validate_schema")
    </input>
    <input name="category" type="string" required="true">
      Directory path relative to .ai/directives/ (e.g., "rye/core", "workflows/api", "testing")
    </input>
    <input name="description" type="string" required="true">
      What does this directive do? Be specific and actionable.
    </input>
    <input name="complexity" type="string" required="false" default="moderate">
      Complexity level: simple, moderate, complex, orchestrator
      (Determines model tier: fast, balanced, reasoning, reasoning+orchestration)
    </input>
    <input name="inputs_schema" type="string" required="false">
      Input parameters as: name:type:required:description (pipe-separated list)
      (e.g., "service_name:string:true:Name of service | replicas:integer:false:Pod count")
    </input>
    <input name="process_steps" type="string" required="false">
      Summary of process steps (will be detailed in directive)
    </input>
  </inputs>

  <process>
    <step name="validate_inputs">
      <description>Validate all required inputs</description>
      <action><![CDATA[
1. name must be snake_case alphanumeric
2. category must be non-empty string (path relative to .ai/directives/)
3. description must be non-empty
4. complexity must be: simple, moderate, complex, or orchestrator (default: moderate)

Halt if any validation fails.
      ]]></action>
    </step>

    <step name="determine_file_path">
      <description>Calculate file path from category</description>
      <action><![CDATA[
File location is: .ai/directives/{category}/{name}.md

Examples:
- category="rye/core", name="create_directive" → .ai/directives/rye/core/create_directive.md
- category="workflows/api" → .ai/directives/workflows/api/{name}.md
- category="testing" → .ai/directives/testing/{name}.md

Create parent directories as needed.
      ]]></action>
    </step>

    <step name="determine_model_tier">
      <description>Map complexity to model tier</description>
      <action><![CDATA[
Complexity to tier mapping:
- simple → model tier="fast"
  Use for: file operations, template rendering, basic transformations
  Context: 5-10%, 2-3 steps

- moderate → model tier="balanced"
  Use for: standard workflows, multi-step processes (DEFAULT)
  Context: 15-20%, 3-5 steps

- complex → model tier="reasoning"
  Use for: architecture decisions, analysis, multi-layer changes
  Context: 25-30%, 5-10 steps

- orchestrator → model tier="reasoning" + orchestration
  Use for: parallel execution, multi-directive workflows
  Context: 30-40%, 5-15 steps, includes parallel_capable and subagent patterns

Select appropriate tier based on complexity input.
      ]]></action>
    </step>

    <step name="create_directive_content">
      <description>Generate markdown file with XML directive template</description>
      <action><![CDATA[
Create .md file with structure:

# {Title from name}

{description}

```xml
<directive name="{name}" version="1.0.0">
  <metadata>
    <description>{description}</description>
    <category>{category}</category>
    <author>rye-os</author>
    <model tier="{tier}" fallback="general">
      {Model context description}
    </model>
    <context_budget>
      <estimated_usage>{10-40 based on complexity}%</estimated_usage>
      <step_count>{2-15 based on complexity}</step_count>
      <spawn_threshold>50%</spawn_threshold>
    </context_budget>
    <permissions>
      <execute>
        <tool>*</tool>
      </execute>
      <search>*</search>
      <load>*</load>
      <sign>*</sign>
    </permissions>
    <relationships>
      <!-- <requires>other_directive</requires> -->
      <!-- <suggests>related_directive</suggests> -->
    </relationships>
  </metadata>

  <context>
    <tech_stack>any</tech_stack>
  </context>

  <inputs>
    {Defined from inputs_schema}
    <input name="example" type="string" required="true">
      Description of input
    </input>
  </inputs>

  <process>
    {Detailed from process_steps}
    <step name="step_1">
      <description>What this step does</description>
      <action><![CDATA[
Detailed action with commands/instructions
      ]]></action>
      <verification>
        <check>Success condition</check>
      </verification>
    </step>
  </process>

  <success_criteria>
    <criterion>Measurable success condition</criterion>
  </success_criteria>

  <outputs>
    <success>Success message and next steps</success>
    <failure>Failure message and common fixes</failure>
  </outputs>
</directive>
```

CRITICAL:
- All XML must be well-formed (matching tags, proper escaping)
- Use CDATA sections for multi-line action blocks: <![CDATA[ ... ]]>
- Escape XML special chars: & → &amp;, < → &lt;, > → &gt;
- Required metadata: name, version, description, category, author, model tier
- Required sections: metadata, context, inputs (if applicable), process, success_criteria, outputs
      ]]></action>
    </step>

    <step name="validate_and_sign">
      <description>Validate XML and generate signature</description>
      <action><![CDATA[
Run mcp_rye sign to validate and sign the directive:

mcp_rye_execute(
  item_type="directive",
  action="sign",
  item_id="{name}",
  parameters={"location": "project"},
  project_path="{project_path}"
)

This:
1. Parses and validates XML syntax
2. Checks all required metadata fields
3. Verifies process steps are well-formed
4. Validates input schema syntax
5. Generates SHA256 content hash
6. Creates validation signature comment at top of file
7. Makes directive discoverable in registry

If validation fails: fix XML errors and re-run sign.
Common errors: mismatched tags, unescaped special chars, missing CDATA for multi-line blocks
      ]]></action>
      <verification>
        <check>File has signature comment at top</check>
        <check>No XML parse errors</check>
        <check>All required metadata fields present</check>
        <check>Process steps are valid XML</check>
      </verification>
    </step>
  </process>

  <success_criteria>
    <criterion>Directive file created at correct path (.ai/directives/{category}/{name}.md)</criterion>
    <criterion>All required XML elements present and well-formed</criterion>
    <criterion>Metadata includes name, version, description, category, author, model tier</criterion>
    <criterion>Signature validation comment added to file</criterion>
    <criterion>Directive is discoverable via search</criterion>
  </success_criteria>

  <outputs>
    <success><![CDATA[
✓ Created directive: {name}
Location: .ai/directives/{category}/{name}.md
Version: 1.0.0
Tier: {tier}
Complexity: {complexity}

Next steps:
- Test: Run directive {name}
- Edit: Update steps and re-sign
- Link: Reference from other directives (relationships.requires/suggests)
- Document: Add example usage to knowledge base
    ]]></success>
    <failure><![CDATA[
✗ Failed to create directive: {name}
Error: {error}

Common fixes:
- name must be snake_case (create_directive, not CreateDirective)
- category must match target directory path
- XML must be well-formed (matching tags, proper escaping)
- Use CDATA for multi-line blocks: <![CDATA[ ... ]]>
- Escape XML special chars: & → &amp;, < → &lt;, > → &gt;
- All required metadata must be present (name, version, description, category, author)
- Process steps must have name, description, action
    ]]></failure>
  </outputs>
</directive>
```
