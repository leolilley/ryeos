<!-- rye:signed:2026-02-26T06:42:50Z:d3c8312782b5580249ecc3907aebaa75a6323b56205848667b36185edc6034a2:AHhWJnXQ4kCn2Iut_QL6B_X1HNQ44d33JZVsWrPkrMxtEFstNU2c1h5FoyxZJfD65YdcOCIlRbspsldw9I7AAA==:4b987fd4e40303ac -->
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
