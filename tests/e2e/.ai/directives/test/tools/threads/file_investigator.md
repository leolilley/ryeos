<!-- rye:signed:2026-02-18T08:20:02Z:8ef96f85e031dd38730317933ae6c70c8e9ecc4125c075fc35363249c1c6643a:Ho2nD2MiViPxbSWyXt3v7kB7Z2SWEmfMdtEgni05Lyu0d1-tXUNBN5G-Pfy7E_tPxDMOg9L-XRpbghWtSZiRAQ==:440443d0858f0199 -->
# File Investigator

Creates a mystery file, then investigates it — reads it back, lists the directory, writes a report summarizing findings.

```xml
<directive name="file_investigator" version="1.0.0">
  <metadata>
    <description>Write a mystery file, investigate it with reads and directory listing, then write a report.</description>
    <category>test/tools/threads</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="10" tokens="40000" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="topic" type="string" required="true">
      Topic for the mystery file content.
    </input>
  </inputs>

  <outputs>
    <output name="mystery_path">Path to the created mystery file</output>
    <output name="report_path">Path to the investigation report</output>
    <output name="file_count">Number of files found in the investigation directory</output>
  </outputs>
</directive>
```

<process>
  <step name="create_mystery">
    Write a short mystery file about the topic to `investigation/mystery.md`. Include a title, 3 clues as bullet points, and a hidden answer at the bottom.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"file_path": "investigation/mystery.md", "content": "..."})`
  </step>

  <step name="read_mystery">
    Read the mystery file back to verify its contents:
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"file_path": "investigation/mystery.md"})`
  </step>

  <step name="list_directory">
    List the investigation directory to see what files exist:
    `rye_execute(item_type="tool", item_id="rye/file-system/list", parameters={"dir_path": "investigation"})`
  </step>

  <step name="write_report">
    Based on what you read and found, write an investigation report to `investigation/report.md` summarizing: what the mystery file contained, how many files are in the directory, and your analysis of the clues.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"file_path": "investigation/report.md", "content": "..."})`
  </step>

  <step name="verify_report">
    Read the report back to confirm:
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"file_path": "investigation/report.md"})`
  </step>

  <step name="return_result">
    Return mystery_path, report_path, and file_count using directive_return.
  </step>
</process>
