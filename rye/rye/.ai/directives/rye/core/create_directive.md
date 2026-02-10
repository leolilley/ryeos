<!-- rye:validated:2026-02-10T02:00:00Z:placeholder -->
# Create Simple Directive

Create minimal directives with essential fields only.

```xml
<directive name="create_directive" version="2.0.0">
  <metadata>
    <description>Create a simple directive file with minimal required fields, check for duplicates, write to disk, and sign it.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022">Simple directive creation</model>
    <limits max_turns="6" max_tokens="4096" />
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
        <param name="content" value="<!-- rye:validated:2026-02-10T02:00:00Z:placeholder -->
# {input:name}

{input:description}

```xml
<directive name=&quot;{input:name}&quot; version=&quot;1.0.0&quot;>
  <metadata>
    <description>{input:description}</description>
    <category>{input:category}</category>
    <author>user</author>
    <model tier=&quot;haiku&quot; id=&quot;claude-3-5-haiku-20241022&quot;>Brief model rationale</model>
    <limits max_turns="6" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name=&quot;example_input&quot; type=&quot;string&quot; required=&quot;true&quot;>
      Describe this input
    </input>
  </inputs>

  <process>
    <step name=&quot;step_1&quot;>
      <description>What this step does</description>
      <execute item_type=&quot;tool&quot; item_id=&quot;rye/file-system/fs_write&quot;>
        <param name=&quot;path&quot; value=&quot;target/path&quot; />
        <param name=&quot;content&quot; value=&quot;file content&quot; />
      </execute>
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
``` " />
        <param name="create_dirs" value="true" />
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
