<!-- ryeos:signed:2026-03-11T07:14:55Z:b7a62f43316f39ee8626becbf4d9e67842f4928fb218c5be53ee03d638051555:UVLMBiiMaDGjQtm0rRyp8IQ8yEPVuRmWbQhy9G7Me0DCOq8lSNNBkWLs4Mn19y3vYI60IJeP5pWlvrddZ678CA==:4b987fd4e40303ac -->

# Multi-Thread Pipeline

Multi-thread orchestration pipeline that spawns multiple child directives in sequence, each building on the previous one's output. Demonstrates cross-thread coordination via shared filesystem.

```xml
<directive name="multi_thread_pipeline" version="1.0.0">
  <metadata>
    <description>Multi-thread orchestration pipeline — spawns child directives in sequence with cross-thread coordination via shared filesystem. Writes a manifest, spawns research, verifies output, then produces analysis and summary.</description>
    <category>test/tools/threads</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022">Multi-stage pipeline orchestration with cross-thread file coordination</model>
    <limits turns="10" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <execute>
        <directive>test/*</directive>
      </execute>
      <fetch>
        <knowledge>*</knowledge>
      </fetch>
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

  <outputs>
    <success>Pipeline complete — manifest, research, analysis, and summary written to {input:workspace_dir}/. All child threads completed successfully.</success>
    <failure>Pipeline failed — check that test/tools/primary/search_and_report directive exists and {input:workspace_dir} is writable. Inspect manifest.json for stage statuses.</failure>
  </outputs>
</directive>
```

<process>
  <step name="write_manifest">
    Write a pipeline manifest JSON to `{input:workspace_dir}/manifest.json` listing all stages and their expected outputs.
  </step>
  <step name="spawn_research">
    Spawn child directive `test/tools/primary/search_and_report` with topic "{input:topic}" and report path `{input:workspace_dir}/research.md`.
  </step>
  <step name="verify_research">
    Read `{input:workspace_dir}/research.md` to verify the research stage completed and gather findings.
  </step>
  <step name="write_analysis">
    Write an analysis document to `{input:workspace_dir}/analysis.md` synthesizing the research findings.
  </step>
  <step name="write_summary">
    Write a final pipeline summary to `{input:workspace_dir}/summary.md` combining all stage outputs.
  </step>
</process>
