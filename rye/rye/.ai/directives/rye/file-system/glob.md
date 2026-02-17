<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->
# Glob

Find files matching a glob pattern.

```xml
<directive name="glob" version="1.0.0">
  <metadata>
    <description>Find files matching a glob pattern, optionally scoped to a base directory.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.file-system.glob</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="pattern" type="string" required="true">
      Glob pattern to match files (e.g., "**/*.py", "*.md")
    </input>
    <input name="path" type="string" required="false">
      Base directory to search from. If omitted, searches from project root.
    </input>
  </inputs>

  <outputs>
    <output name="files">List of file paths matching the pattern</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:pattern} is non-empty.
  </step>

  <step name="call_glob">
    Find matching files:
    `rye_execute(item_type="tool", item_id="rye/file-system/glob", parameters={"pattern": "{input:pattern}", "path": "{input:path}"})`
  </step>

  <step name="return_result">
    Return the list of matching file paths.
  </step>
</process>
