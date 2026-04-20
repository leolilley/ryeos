<!-- rye:signed:2026-04-19T09:49:53Z:394587f952f082db3bb9fac38057136a11119f46ff9b622db04a06833704ba87:XnGORGmsNcvDQOWuEmn0UefXiyiH5aJ3StQdFHsfsskU+yD4mv3UT3Yg0fY+NWMoOVqYJVKAKhBw1XqJdvs4BQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# Grep

Search file contents for a text or regex pattern.

```xml
<directive name="grep" version="1.0.0">
  <metadata>
    <description>Search file contents for a text or regex pattern, optionally filtered by path and file glob.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.file-system.grep</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="pattern" type="string" required="true">
      Text or regex pattern to search for
    </input>
    <input name="path" type="string" required="false">
      Directory or file path to search in. If omitted, searches from project root.
    </input>
    <input name="include" type="string" required="false">
      File glob filter to restrict which files are searched (e.g., "*.py", "*.md")
    </input>
  </inputs>

  <outputs>
    <output name="matches">List of matching lines with file path, line number, and content</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:pattern} is non-empty.
  </step>

  <step name="call_grep">
    Search for the pattern:
    `rye_execute(item_id="rye/file-system/grep", parameters={"pattern": "{input:pattern}", "path": "{input:path}", "include": "{input:include}"})`
  </step>

  <step name="return_result">
    Return the list of matching lines with file paths, line numbers, and content.
  </step>
</process>
