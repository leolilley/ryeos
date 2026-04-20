<!-- rye:signed:2026-04-19T09:49:53Z:48ac0bd3e9fc7f5475dda4459f4bba2418c1f02fd94d8ab91bc4770dcbd576b2:U9GYQt9hn47i0R5UuiX5iT56P9euNOhOt7TxRrlTEja0rcAE3JFPq2m+XnndwCH95pjV3MzX7T6a2W1m76FHAQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
    `rye_execute(item_id="rye/file-system/glob", parameters={"pattern": "{input:pattern}", "path": "{input:path}"})`
  </step>

  <step name="return_result">
    Return the list of matching file paths.
  </step>
</process>
