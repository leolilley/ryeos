<!-- rye:signed:2026-02-25T07:50:41Z:d3c8312782b5580249ecc3907aebaa75a6323b56205848667b36185edc6034a2:7mbu4N_FjE0GYod8-AyGy_9MMA0kJlcRlYAKvzBQHa85fgvMeh2gZuku4ojZEgT6Cl8-cn2JzoJhnCiJ8WeVDg==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:f901f840d5406a52de18ef0bb495a3ea1ac865ddb3dda3e71d9411b840714c80:o0JSEEn0PvOt3RyvfnrnuxkOZPhp8NkQxMDhviljRwafEcuaRB48omGZL08GUiOwYob9LHHbVQKvGfUqHJPgBA==:9fbfabe975fa5a7f -->
# Glob

Find files matching a glob pattern.

```xml
<directive name="glob" version="1.0.0">
  <metadata>
    <description>Find files matching a glob pattern, optionally scoped to a base directory.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
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
