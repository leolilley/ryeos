<!-- rye:validated:2026-02-10T02:00:00Z:placeholder -->

# Create Knowledge Entry

Create a new knowledge entry with proper metadata, validation, and signing.

```xml
<directive name="create_knowledge" version="2.0.0">
  <metadata>
    <description>Create a knowledge entry file with YAML frontmatter and markdown content, then sign it.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022">Knowledge entry creation</model>
    <limits max_turns="6" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <knowledge>*</knowledge>
      </search>
      <sign>
        <knowledge>*</knowledge>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="id" type="string" required="true">
      Unique identifier in kebab-case (e.g., "jwt-validation", "deployment-strategies")
    </input>
    <input name="title" type="string" required="true">
      Human-readable title for the knowledge entry
    </input>
    <input name="category" type="string" required="true">
      Directory path relative to .ai/knowledge/ (e.g., "security/authentication", "patterns")
    </input>
    <input name="content" type="string" required="true">
      Main markdown content of the knowledge entry
    </input>
    <input name="tags" type="string" required="false">
      Comma-separated tags (3-5 recommended, e.g., "jwt, tokens, security")
    </input>
  </inputs>

  <process>
    <step name="check_duplicates">
      <description>Search for existing knowledge with a similar ID to avoid duplicates</description>
      <search item_type="knowledge" query="{input:id}" />
    </step>

    <step name="write_entry">
      <description>Write the knowledge file with YAML frontmatter and markdown content to .ai/knowledge/{input:category}/{input:id}.md</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value=".ai/knowledge/{input:category}/{input:id}.md" />
        <param name="content" value="---
id: {input:id}
title: {input:title}
category: {input:category}
version: '1.0.0'
author: rye-os
tags:
  - {input:tags}
---

# {input:title}

{input:content}
" />
      </execute>
    </step>

    <step name="sign_entry">
      <description>Validate and sign the new knowledge entry</description>
      <sign item_type="knowledge" item_id="{input:id}" />
    </step>
  </process>

  <outputs>
    <success>Created knowledge entry: {input:id} at .ai/knowledge/{input:category}/{input:id}.md</success>
    <failure>Failed to create knowledge entry: {input:id}</failure>
  </outputs>
</directive>
```
