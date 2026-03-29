<!-- rye:signed:2026-03-11T07:14:55Z:1c14a5c825934a9eb03a864003eaa32f2a96080d098869e76c01a93303b611b9:9Annt0g4V4To4n2MPpOCtk9z4tis1Af-fuFrRdSnApV9EO4nU75PqAOlddyngN_Eg3-6Y2iy0IZSZUujmd2JCA==:4b987fd4e40303ac -->

# Research and Write Report

Research a topic by searching knowledge, loading the best match, then writing a detailed report.

```xml
<directive name="research_and_write" version="1.0.0">
  <metadata>
    <description>Research a topic by searching the knowledge base, loading a known reference entry for context, then writing a detailed report combining search results and loaded knowledge.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022">Multi-tool research and report generation</model>
    <limits turns="6" tokens="3072" />
    <permissions>
      <fetch>
        <knowledge>*</knowledge>
      </fetch>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="topic" type="string" required="true">
      The topic to research (e.g., "authentication patterns", "caching strategies")
    </input>
    <input name="report_path" type="string" required="true">
      File path where the research report will be written
    </input>
  </inputs>

  <outputs>
    <success>Researched topic "{input:topic}" and wrote report to {input:report_path}</success>
    <failure>Failed to research topic "{input:topic}" or write report</failure>
  </outputs>
</directive>
```

<process>
  <step name="search_knowledge">
    Search the knowledge base for entries relevant to "{input:topic}".
  </step>
  <step name="load_reference">
    Load the `rye-architecture` knowledge entry for reference context.
  </step>
  <step name="write_report">
    Write a research report combining search results and loaded knowledge to `{input:report_path}`.
  </step>
</process>
