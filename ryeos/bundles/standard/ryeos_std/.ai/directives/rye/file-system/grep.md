<!-- rye:signed:2026-02-25T07:50:41Z:aead5d7280c16264e8dcbb486cde7b29a312b89222602120e3b57e7bf45b9391:-hTVT7zMBj5OHT8D9cvjK3ayu01UlgSWGJxR4wJT5OGdiWWAsdkV871UjnQ_eFTGsEl2w2qQ54KVDaHIPmMyCg==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:7d84f010f0fdf6b6ee41285aed3b86aae11aac80ba88686a56d2949a4e7a1c1c:wcAi8yKewNUCMv7avgJJOpa1dzk_we6DiK1tVmioRDu0ejh1s9RFz_A1XarDIbLgOuO0PMOk9U3u5o0sBrIYCw==:9fbfabe975fa5a7f -->
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
    `rye_execute(item_type="tool", item_id="rye/file-system/grep", parameters={"pattern": "{input:pattern}", "path": "{input:path}", "include": "{input:include}"})`
  </step>

  <step name="return_result">
    Return the list of matching lines with file paths, line numbers, and content.
  </step>
</process>
