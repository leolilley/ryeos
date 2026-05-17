---
description: "Create a directive file with minimal required fields, check for duplicates, write to disk, and sign it."
version: "3.0.0"
model_tier: fast
limits:
  turns: 6
  tokens: 4096
permissions:
  fetch:
    - directive:*
  execute:
    - tool:rye.file-system.*
  sign:
    - directive:*
---

# Create Directive

Create a new directive file with proper metadata, validate, and sign it.

<process>
  <step name="check_duplicates">
    Search for existing directives with a similar name to avoid creating duplicates.
    `rye_fetch(scope="directive", query="{input:name}")`
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

    `rye_execute(item_id="rye/file-system/write", parameters={"path": ".ai/directives/{input:category}/{input:name}.md", "content": "<generated directive content>", "create_dirs": true})`
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
