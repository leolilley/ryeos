---
description: "Create a knowledge entry file with YAML frontmatter and markdown content, then sign it."
version: "3.0.0"
model_tier: fast
limits:
  turns: 6
  tokens: 4096
permissions:
  execute:
    - tool:rye.file-system.*
  fetch:
    - knowledge:*
  sign:
    - knowledge:*
---

# Create Knowledge Entry

Create a new knowledge entry with proper metadata, validation, and signing.

<process>
  <step name="check_duplicates">
    Search for existing knowledge entries with a similar ID to avoid duplicates.
    `rye_fetch(scope="knowledge", query="{input:name}")`
  </step>

  <step name="write_entry">
    Write the knowledge file with YAML frontmatter and markdown content to .ai/knowledge/{input:category}/{input:name}.md

    Generate this structure:
    ---
    name: {input:name}
    title: {input:title}
    category: {input:category}
    version: '1.0.0'
    author: rye-os
    tags: (split {input:tags} on commas into individual list entries)
    ---

    # {input:title}

    {input:content}

    `rye_execute(item_id="rye/file-system/write", parameters={"path": ".ai/knowledge/{input:category}/{input:name}.md", "content": "<generated knowledge entry>", "create_dirs": true})`
  </step>

  <step name="sign_entry">
    Validate and sign the new knowledge entry.
    `rye_sign(item_type="knowledge", item_id="{input:category}/{input:name}")`
  </step>
</process>

<success_criteria>
  <criterion>No duplicate knowledge entry with the same ID exists</criterion>
  <criterion>Knowledge file created at .ai/knowledge/{input:category}/{input:name}.md</criterion>
  <criterion>YAML frontmatter contains name, title, category, version, author, and tags</criterion>
  <criterion>Tags parsed from comma-separated {input:tags} into individual YAML list entries</criterion>
  <criterion>Markdown content follows the frontmatter</criterion>
  <criterion>Signature validation passed</criterion>
</success_criteria>
