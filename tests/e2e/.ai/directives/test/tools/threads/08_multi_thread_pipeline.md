<!-- rye:validated:2026-02-10T03:00:00Z:placeholder -->

# Multi-Thread Pipeline

Multi-thread orchestration pipeline that spawns multiple child directives in sequence, each building on the previous one's output. Demonstrates cross-thread coordination via shared filesystem.

```xml
<directive name="08_multi_thread_pipeline" version="1.0.0">
  <metadata>
    <description>Multi-thread orchestration pipeline — spawns child directives in sequence with cross-thread coordination via shared filesystem. Writes a manifest, spawns research, verifies output, then produces analysis and summary.</description>
    <category>test</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022">Multi-stage pipeline orchestration with cross-thread file coordination</model>
    <limits turns="10" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <execute>
        <directive>test/*</directive>
      </execute>
      <search>
        <knowledge>*</knowledge>
      </search>
    </permissions>
  </metadata>

  <inputs>
    <input name="topic" type="string" required="true">
      The research topic for the pipeline to investigate
    </input>
    <input name="workspace_dir" type="string" required="true">
      Path to the directory for all pipeline outputs
    </input>
  </inputs>

  <process>
    <step name="write_manifest">
      <description>Write a pipeline manifest listing all stages and their expected outputs</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:workspace_dir}/manifest.json" />
        <param name="content" value='{
  "pipeline": "multi_thread_pipeline",
  "topic": "{input:topic}",
  "stages": [
    {"name": "manifest", "output": "{input:workspace_dir}/manifest.json", "status": "complete"},
    {"name": "research", "output": "{input:workspace_dir}/research.md", "status": "pending"},
    {"name": "analysis", "output": "{input:workspace_dir}/analysis.md", "status": "pending"},
    {"name": "summary", "output": "{input:workspace_dir}/summary.md", "status": "pending"}
  ]
}' />
      </execute>
    </step>

    <step name="spawn_research">
      <description>Spawn child directive test/tools/primary/03_search_and_report to research the topic and write findings</description>
      <execute item_type="directive" item_id="test/tools/primary/03_search_and_report">
        <param name="topic" value="{input:topic}" />
        <param name="report_path" value="{input:workspace_dir}/research.md" />
      </execute>
    </step>

    <step name="verify_research">
      <description>Read the research output to verify the research stage completed and gather findings for analysis</description>
      <execute item_type="tool" item_id="rye/file-system/fs_read">
        <param name="path" value="{input:workspace_dir}/research.md" />
      </execute>
    </step>

    <step name="write_analysis">
      <description>Write an analysis document synthesizing the research findings into actionable insights</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:workspace_dir}/analysis.md" />
        <param name="content" value="# Analysis: {input:topic}

## Source
Based on research findings from {input:workspace_dir}/research.md

## Key Findings
- Analysis of research output for topic: {input:topic}
- Cross-thread coordination verified — research child completed successfully
- Findings synthesized from child directive test/tools/primary/03_search_and_report

## Recommendations
- Review research.md for detailed findings
- Proceed to summary stage for final consolidation
" />
      </execute>
    </step>

    <step name="write_summary">
      <description>Write a final pipeline summary combining all stage outputs into a consolidated report</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:workspace_dir}/summary.md" />
        <param name="content" value="# Pipeline Summary: {input:topic}

## Pipeline Stages Completed
1. **Manifest** — {input:workspace_dir}/manifest.json
2. **Research** — {input:workspace_dir}/research.md (child: test/tools/primary/03_search_and_report)
3. **Analysis** — {input:workspace_dir}/analysis.md
4. **Summary** — this file

## Cross-Thread Coordination
- Research stage executed via spawned child thread
- Analysis stage consumed research output via shared filesystem
- All stages completed in sequence

## Result
Multi-thread pipeline for topic '{input:topic}' completed successfully.
" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Pipeline complete — manifest, research, analysis, and summary written to {input:workspace_dir}/. All child threads completed successfully.</success>
    <failure>Pipeline failed — check that test/tools/primary/03_search_and_report directive exists and {input:workspace_dir} is writable. Inspect manifest.json for stage statuses.</failure>
  </outputs>
</directive>
```
