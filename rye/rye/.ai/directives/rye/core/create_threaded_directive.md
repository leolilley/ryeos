<!-- rye:validated:2026-02-10T02:00:00Z:placeholder -->

# Create Threaded Directive

```xml
<directive name="create_threaded_directive" version="1.0.0">
  <metadata>
    <description>Creates directives with full thread execution support — model configuration, cost limits, capability permissions, and structured action tags (execute, search, load, sign) for autonomous thread-based execution via thread_directive.</description>
    <category>rye/core</category>
    <author>rye</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022">Threaded directive creation</model>
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
    <input name="complexity" type="string" required="true" enum="simple|moderate|complex">
      Complexity level — determines default limits and turn counts
    </input>
    <input name="permissions_needed" type="string" required="true">
      Comma-separated capability strings (e.g., rye.execute.tool.rye.file-system.*,rye.search.directive.*)
    </input>
    <input name="process_steps" type="string" required="false">
      Optional summary of the steps the directive should perform
    </input>
  </inputs>

  <process>
    <step name="search_existing">
      <description>Search for similar existing directives to avoid duplication and gather patterns</description>
      <search item_type="directive" query="{input:name} {input:category}" />
    </step>

    <step name="load_reference">
      <description>Load an example threaded directive to use as a reference pattern for structure and metadata</description>
      <load item_type="directive" item_id="rye/core/create_threaded_directive" />
    </step>

    <step name="write_directive">
      <description>Write the threaded directive file with full thread execution metadata including model, limits, permissions, inputs, and structured action tags in process steps</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value=".ai/directives/{input:category}/{input:name}.md" />
        <param name="content" value="Generate a threaded directive with: name={input:name}, category={input:category}, description={input:description}, complexity={input:complexity}, permissions={input:permissions_needed}, steps={input:process_steps}. Include <model>, <limits>, <permissions> with hierarchical <execute>/<search>/<load>/<sign> permission entries, and use <execute>, <search>, <load>, <sign> action tags in process steps." />
      </execute>
    </step>

    <step name="sign_directive">
      <description>Sign the newly created threaded directive to validate it</description>
      <sign item_type="directive" item_id="{input:category}/{input:name}" />
    </step>
  </process>

  <outputs>
    <output name="directive_path" type="string">
      Path to the created threaded directive file
    </output>
    <output name="signed" type="boolean">
      Whether the directive was successfully signed
    </output>
  </outputs>
</directive>
```
