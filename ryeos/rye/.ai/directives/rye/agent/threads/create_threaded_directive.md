<!-- rye:signed:2026-02-21T05:56:40Z:4b59b87de5bcef04a06e87844bea10f11fb022adb88542ed4de81a916455c60e:MABOAW6Bglh00oltA86t1kRSDoYFmY1crmOBFRHQLt0z5e4M7w7l_9u85iy3Aaef_9nj9EK4cMfe250FUZsUDg==:9fbfabe975fa5a7f -->

# Create Threaded Directive

Create a directive with full thread execution support — model configuration, cost limits, capability permissions for autonomous thread-based execution via thread_directive.

```xml
<directive name="create_threaded_directive" version="2.0.0">
  <metadata>
    <description>Creates directives with full thread execution support — model configuration, cost limits, capability permissions for autonomous thread-based execution.</description>
    <category>rye/agent/threads</category>
    <author>rye</author>
    <model tier="fast" />
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

    The generated file must follow this structure:
    1. Signature comment placeholder at the top
    2. Markdown title and description
    3. A single ```xml fenced block containing ONLY metadata (with model, limits, permissions), inputs, and outputs
    4. Pseudo-XML process steps AFTER the fence

    Parse {input:permissions_needed} into hierarchical permission entries grouped by primary action (execute, search, load, sign).
    Use {input:process_steps} if provided to write the process steps.

    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": ".ai/directives/{input:category}/{input:name}.md", "content": "<generated directive content>", "create_dirs": true})`

  </step>

  <step name="sign_directive">
    `rye_sign(item_type="directive", item_id="{input:category}/{input:name}")`
  </step>
</process>

<success_criteria>
<criterion>No duplicate directive with the same name exists</criterion>
<criterion>Directive file created at .ai/directives/{input:category}/{input:name}.md</criterion>
<criterion>Model tier, limits, and permissions correctly configured for {input:complexity}</criterion>
<criterion>Permissions parsed from {input:permissions_needed} into hierarchical XML entries</criterion>
<criterion>Process steps present after the XML fence</criterion>
<criterion>Signature validation passed</criterion>
</success_criteria>

<results>
  <success>Created threaded directive: {input:name} at .ai/directives/{input:category}/{input:name}.md with {input:complexity} limits. Execute with: rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "{input:category}/{input:name}"})</success>
  <failure>Failed to create threaded directive: {input:name}. Check complexity mapping, permissions format, and XML structure.</failure>
</results>
