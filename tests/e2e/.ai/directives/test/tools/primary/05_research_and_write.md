<!-- rye:validated:2026-02-10T03:00:00Z:placeholder -->

# Research and Write Report

Research a topic by searching knowledge, loading the best match, then writing a detailed report.

```xml
<directive name="research_and_write" version="1.0.0">
  <metadata>
    <description>Research a topic by searching the knowledge base, loading a known reference entry for context, then writing a detailed report combining search results and loaded knowledge.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022">Multi-tool research and report generation</model>
    <limits turns="6" tokens="3072" />
    <permissions>
      <search>
        <knowledge>*</knowledge>
      </search>
      <load>
        <knowledge>*</knowledge>
      </load>
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

  <process>
    <step name="search_knowledge">
      <description>Search the knowledge base for entries relevant to the topic</description>
      <search item_type="knowledge" query="{input:topic}" />
    </step>

    <step name="load_reference">
      <description>Load the rye-architecture knowledge entry as reference context</description>
      <load item_type="knowledge" item_id="rye-architecture" />
    </step>

    <step name="write_report">
      <description>Write a research report combining search results and loaded knowledge to the report path</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:report_path}" />
        <param name="content" value="# Research Report: {input:topic}

## Overview
Research findings on: {input:topic}

## Search Results
(entries found via knowledge search for the topic)

## Reference Context
(context from rye-architecture knowledge entry)

## Analysis
(synthesized findings combining search results and reference material)

## Recommendations
(actionable recommendations based on research)
" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Researched topic "{input:topic}" and wrote report to {input:report_path}</success>
    <failure>Failed to research topic "{input:topic}" or write report</failure>
  </outputs>
</directive>
```
