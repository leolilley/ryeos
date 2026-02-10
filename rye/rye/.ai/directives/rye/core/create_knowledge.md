# Create Knowledge Entry

Create new knowledge entries with proper metadata, validate, and sign.

```xml
<directive name="create_knowledge" version="1.0.0">
  <metadata>
    <description>Create knowledge entries with proper structure, validation, and signing.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="balanced" fallback="general">
      Standard workflow for knowledge creation
    </model>
    <context_budget>
      <estimated_usage>10%</estimated_usage>
      <step_count>4</step_count>
      <spawn_threshold>50%</spawn_threshold>
    </context_budget>
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <knowledge>*</knowledge>
      </search>
      <load>
        <knowledge>*</knowledge>
      </load>
      <sign>
        <knowledge>*</knowledge>
      </sign>
    </permissions>
    <relationships>
      <suggests>edit_knowledge</suggests>
    </relationships>
  </metadata>

  <context>
    <tech_stack>any</tech_stack>
  </context>

  <inputs>
    <input name="id" type="string" required="true">
      Unique identifier in kebab-case (e.g., "jwt-validation", "deployment-strategies")
    </input>
    <input name="title" type="string" required="true">
      Human-readable title for the knowledge entry
    </input>
    <input name="category" type="string" required="true">
      Directory path relative to .ai/knowledge/ (e.g., "security/authentication", "patterns", "architecture/decisions")
    </input>
    <input name="content" type="string" required="true">
      Main markdown content of the knowledge entry
    </input>
    <input name="tags" type="string" required="false">
      Comma-separated tags (3-5 recommended, e.g., "jwt, tokens, security")
    </input>
    <input name="extends" type="string" required="false">
      Comma-separated knowledge IDs this builds upon (e.g., "authentication-basics, cryptography")
    </input>
    <input name="references" type="string" required="false">
      Comma-separated related knowledge IDs or URLs (e.g., "oauth-overview, https://example.com/docs")
    </input>
  </inputs>

  <process>
    <step name="validate_inputs">
      <description>Validate all required inputs</description>
      <action><![CDATA[
1. ID must be kebab-case alphanumeric
2. Title must be non-empty
3. Category must be a non-empty string (will become directory path)
4. Content must be non-empty markdown
5. Tags (if provided) should be 3-5 items

Halt if any validation fails.
      ]]></action>
    </step>

    <step name="determine_file_path">
      <description>Calculate file path from category</description>
      <action><![CDATA[
File location is: .ai/knowledge/{category}/{id}.md

Examples:
- category="security/authentication" → .ai/knowledge/security/authentication/jwt-validation.md
- category="patterns" → .ai/knowledge/patterns/retry-logic.md
- category="reference" → .ai/knowledge/reference/api-design.md

Create parent directories as needed.
      ]]></action>
    </step>

    <step name="create_entry">
      <description>Generate markdown file with YAML frontmatter</description>
      <action><![CDATA[
Create file with this structure:

---
id: {id}
title: {title}
category: {category}
version: "1.0.0"
author: {current_user_or_team}
created_at: {ISO_8601_timestamp}
tags:
  - {tag1}
  - {tag2}
  - {tag3}
extends:
  - {knowledge_id_1}
  - {knowledge_id_2}
references:
  - {knowledge_id_or_url}
  - https://example.com
used_by:
  # Will be populated by directives/tools that use this
---

# {title}

{content}

---

Notes:
- All required fields: id, title, category, version, author, created_at
- Optional: tags (3-5), extends, references, used_by
- version always starts at "1.0.0"
- content_hash and signatures are added by validation process
- No git commits—knowledge items are registry-managed
      ]]></action>
    </step>

    <step name="validate_and_sign">
      <description>Validate metadata and generate signature</description>
      <action><![CDATA[
Run mcp_rye sign to validate and sign the entry:

mcp_rye_execute(
  item_type="knowledge",
  action="sign",
  item_id="{id}",
  parameters={"location": "project"},
  project_path="{project_path}"
)

This:
1. Validates YAML frontmatter syntax
2. Checks all required fields are present
3. Verifies category matches file path
4. Generates SHA256 content hash
5. Creates validation signature comment at top
6. Makes entry discoverable in registry

If validation fails: fix errors and re-run sign.
      ]]></action>
      <verification>
        <check>File has signature comment at top</check>
        <check>No YAML parse errors</check>
        <check>Category matches file path</check>
      </verification>
    </step>
  </process>

  <success_criteria>
    <criterion>Knowledge entry file created at correct path</criterion>
    <criterion>All required metadata fields present</criterion>
    <criterion>Signature validation comment added</criterion>
    <criterion>Entry is discoverable via search</criterion>
  </success_criteria>

  <outputs>
    <success><![CDATA[
✓ Created knowledge entry: {id}
Location: .ai/knowledge/{category}/{id}.md
Version: 1.0.0
Author: {author}
Created: {created_at}

Next steps:
- Edit: create_knowledge with new inputs
- Link: Reference from directives/tools with used_by
- Search: find knowledge entries by tag
    ]]></success>
    <failure><![CDATA[
✗ Failed to create knowledge entry: {id}
Error: {error}

Common fixes:
- ID must be kebab-case (jwt-validation, not JwtValidation)
- Category must exist or be created (e.g., security/authentication)
- YAML frontmatter must be valid
- Title must be non-empty
    ]]></failure>
  </outputs>
</directive>
```
