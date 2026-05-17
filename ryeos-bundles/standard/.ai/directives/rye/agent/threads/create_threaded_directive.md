<!-- ryeos:signed:2026-05-17T21:44:36Z:80cfc45457ae646f0ab34ba020a14077fb8f94375022518d47c867dfe4a9cbc7:CYLsgG0kYcNdk9b/flayDLVJbz98Gzr+vuRUux/T1CSHqlK5wzjVu1IE30rsXgrGYE4MLebpicH66ae3PBJbDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Creates directives with full thread execution support — model configuration, cost limits, capability permissions for autonomous thread-based execution."
version: "2.0.0"
model_tier: fast
limits:
  turns: 8
  tokens: 4096
permissions:
  execute:
    - tool:rye.file-system.*
  fetch:
    - directive:*
  sign:
    - directive:*
---

# Create Threaded Directive

Create a directive with full thread execution support — model configuration, cost limits, capability permissions for autonomous thread-based execution via thread_directive.

<process>
  <step name="search_existing">
    Search for similar existing directives to avoid duplication and gather patterns.
    `rye_fetch(scope="directive", query="{input:name} {input:category}")`
  </step>

  <step name="load_reference">
    Load an example threaded directive to use as a structural reference.
    `rye_fetch(item_type="directive", item_id="rye/core/create_threaded_directive")`
  </step>

  <step name="determine_limits">
    Map {input:complexity} to default limits:
    - simple: turns=6, tokens=4096, spend=0.05
    - moderate: turns=15, tokens=200000, spend=0.50
    - complex: turns=30, tokens=200000, spend=1.00
  </step>

  <step name="write_directive">
    Generate the directive and write it to .ai/directives/{input:category}/{input:name}.md

    The generated file must follow this structure:
    1. Signature comment placeholder at the top
    2. Markdown title and description
    3. A single ```xml fenced block containing ONLY metadata (with model, limits, permissions), inputs, and outputs
    4. Pseudo-XML process steps AFTER the fence

    Parse {input:permissions_needed} into hierarchical permission entries grouped by primary action (execute, fetch, sign).
    Use {input:process_steps} if provided to write the process steps.

    `rye_execute(item_id="rye/file-system/write", parameters={"path": ".ai/directives/{input:category}/{input:name}.md", "content": "<generated directive content>", "create_dirs": true})`

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
