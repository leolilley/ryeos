<!-- rye:signed:2026-03-11T07:14:55Z:6967a5eca52850e819885eecf8ffb64b9b619338427d135edcf3c2234a1a2b08:3rgBqvLoRUh8H0Em0xKFxK11pYEitfgE0JRD6pvH_7znmh7yDy7izNYuGmYK8iiqzBCrvb7I6DFXb7QbFwW6Bw==:4b987fd4e40303ac -->

# Search and Report

Multi-step directive that searches the knowledge base for entries on a topic, then writes a summary report to a file.

```xml
<directive name="search_and_report" version="1.0.0">
  <metadata>
    <description>Search knowledge entries for a topic and write a summary report.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022" />
    <limits turns="5" tokens="2048" />
    <permissions>
      <fetch><knowledge>*</knowledge></fetch>
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

  <outputs>
    <success>Knowledge search complete. Report written to {input:report_path}.</success>
    <failure>No knowledge entries found for topic "{input:topic}".</failure>
  </outputs>
</directive>
```

<process>
  <step name="search_knowledge">
    Search the knowledge base for entries related to "{input:topic}".
  </step>
  <step name="write_report">
    Compile the search findings into a summary and write it to `{input:report_path}`.
  </step>
</process>
