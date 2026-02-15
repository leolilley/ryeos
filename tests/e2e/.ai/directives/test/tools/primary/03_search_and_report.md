<!-- rye:validated:2026-02-10T03:00:00Z:placeholder -->

# Search and Report

Multi-step directive that searches the knowledge base for entries on a topic, then writes a summary report to a file.

```xml
<directive name="search_and_report" version="1.0.0">
  <metadata>
    <description>Search knowledge entries for a topic and write a summary report.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits turns="5" tokens="2048" />
    <permissions>
      <search><knowledge>*</knowledge></search>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="topic" type="string" required="true">
      The topic to search for in the knowledge base.
    </input>
    <input name="report_path" type="string" required="true">
      The file path to write the summary report to.
    </input>
  </inputs>

  <process>
    <step name="search_knowledge">
      <description>Search the knowledge base for entries related to the topic.</description>
      <search item_type="knowledge" query="{input:topic}" />
    </step>

    <step name="write_report">
      <description>Compile the search findings into a summary and write it to the report file.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:report_path}" />
        <param name="content" value="# Report: {input:topic}\n\nSummary of knowledge base findings on the topic." />
        <param name="mode" value="overwrite" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Knowledge search complete. Report written to {input:report_path}.</success>
    <failure>No knowledge entries found for topic "{input:topic}".</failure>
  </outputs>
</directive>
```
