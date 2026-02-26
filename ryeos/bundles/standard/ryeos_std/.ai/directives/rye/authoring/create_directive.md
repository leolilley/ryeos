<!-- rye:signed:2026-02-25T07:50:41Z:a07c0727c956ef985c0bbf3e94b77cca4ee28025eba2093c48cd6a655683ab1f:2V2bOmRCeDb_gHxMt0aAEaXHzo4YLRW7ESl-KxZVBMqNtU2zWZeErZeUO_dbIC2OEiXyuNotR9if0UTY8PolBw==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:c395af2f34e41bd77d61502d0fca1e38200dacf61dd9e3aaa24663976be0ed61:yn-aCTL8iEppNw105L9fBS4T4BSwNj6SX3elOJxd6z7mTnNTJL1Sd4tuLTdse260tONtX6VbdznmWcWyBjwuDw==:9fbfabe975fa5a7f -->
# Create Directive

Create a new directive file with proper metadata, validate, and sign it.

```xml
<directive name="create_directive" version="3.0.0">
  <metadata>
    <description>Create a directive file with minimal required fields, check for duplicates, write to disk, and sign it.</description>
    <category>rye/authoring</category>
    <author>rye-os</author>
    <model tier="fast" />
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

  <outputs>
    <output name="directive_path">Path to the created directive file</output>
    <output name="signed">Whether the directive was successfully signed</output>
  </outputs>
</directive>
```

<process>
  <step name="check_duplicates">
    Search for existing directives with a similar name to avoid creating duplicates.
    `rye_search(item_type="directive", query="{input:name}")`
    If a duplicate exists, stop and report it.
  </step>

  <step name="validate_inputs">
    Validate that {input:name} is snake_case alphanumeric, {input:category} is non-empty, and {input:description} is non-empty. Halt if any validation fails.
  </step>

  <step name="write_directive_file">
    Generate the directive markdown file and write it to .ai/directives/{input:category}/{input:name}.md

    The generated file must contain:
    1. A signature comment placeholder at the top
    2. A markdown title and description
    3. A single ```xml fenced block containing ONLY metadata, inputs, and outputs
    4. Pseudo-XML process steps AFTER the fence for the LLM to follow

    Use {input:process_steps} if provided to inform the step content.

    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": ".ai/directives/{input:category}/{input:name}.md", "content": "<generated directive content>", "create_dirs": true})`
  </step>

  <step name="sign_directive">
    Validate and sign the new directive file.
    `rye_sign(item_type="directive", item_id="{input:category}/{input:name}")`
  </step>
</process>

<success_criteria>
  <criterion>No duplicate directive with the same name exists</criterion>
  <criterion>Directive file created at .ai/directives/{input:category}/{input:name}.md</criterion>
  <criterion>XML fence contains well-formed metadata, inputs, and outputs</criterion>
  <criterion>Signature validation comment present at top of file</criterion>
</success_criteria>
