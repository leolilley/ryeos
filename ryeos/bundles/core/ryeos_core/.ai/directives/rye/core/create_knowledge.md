<!-- rye:signed:2026-04-09T00:09:13Z:38a1b84328bc160ea75b20d63f5140e75ad53227b94742e125d9b3b9587081b9:4YogCSKt5ysJP4ikuGO7sfyDL2FDH6CQ0xMnhymrxSQHGl63qB3hCdydijxIh8JjMYOea_eZeXHI_L3A66RFAg:4b987fd4e40303ac -->
# Create Knowledge Entry

Create a new knowledge entry with proper metadata, validation, and signing.

```xml
<directive name="create_knowledge" version="3.0.0">
  <metadata>
    <description>Create a knowledge entry file with YAML frontmatter and markdown content, then sign it.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <fetch>
        <knowledge>*</knowledge>
      </fetch>
      <sign>
        <knowledge>*</knowledge>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="name" type="string" required="true">
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

  <outputs>
    <output name="knowledge_path">Path to the created knowledge entry file</output>
    <output name="signed">Whether the entry was successfully signed</output>
  </outputs>
</directive>
```

<process>
  <step name="check_duplicates">
    Search for existing knowledge entries with a similar ID to avoid duplicates.
    `rye_fetch(query="{input:name}", scope="knowledge")`
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
