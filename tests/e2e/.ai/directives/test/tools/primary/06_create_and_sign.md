<!-- rye:validated:2026-02-10T03:00:00Z:placeholder -->

# Create and Sign Knowledge Entry

Create a new knowledge entry file with YAML frontmatter, then sign it for validation.

```xml
<directive name="create_and_sign" version="1.0.0">
  <metadata>
    <description>Create a new knowledge entry file with YAML frontmatter and markdown body, then validate and sign it.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022">Knowledge creation and signing</model>
    <limits turns="5" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <sign>
        <knowledge>*</knowledge>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="entry_id" type="string" required="true">
      Unique identifier for the knowledge entry in kebab-case (e.g., "caching-patterns")
    </input>
    <input name="title" type="string" required="true">
      Human-readable title for the knowledge entry
    </input>
    <input name="content" type="string" required="true">
      Markdown content for the body of the knowledge entry
    </input>
  </inputs>

  <process>
    <step name="write_entry">
      <description>Create the knowledge entry file with YAML frontmatter and content body</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value=".ai/knowledge/{input:entry_id}.md" />
        <param name="content" value="---
id: {input:entry_id}
title: {input:title}
version: '1.0.0'
---

# {input:title}

{input:content}
" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>

    <step name="sign_entry">
      <description>Validate and sign the newly created knowledge entry</description>
      <sign item_type="knowledge" item_id="{input:entry_id}" />
    </step>
  </process>

  <outputs>
    <success>Created and signed knowledge entry: {input:entry_id} at .ai/knowledge/{input:entry_id}.md</success>
    <failure>Failed to create or sign knowledge entry: {input:entry_id}</failure>
  </outputs>
</directive>
```
