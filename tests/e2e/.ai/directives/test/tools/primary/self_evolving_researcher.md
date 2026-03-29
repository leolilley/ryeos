<!-- rye:signed:2026-03-11T07:14:55Z:c18e90d2221218e49901274ada8f96ad5cbb1f24f83c9246dfc39b44e183e461:BwbyWH9p9h52hvyUoXU2UHduv8pVyNJ9ya_zl6-sDrRzZP2pF1hYMb7r1KPR9NjdK7Y19-LZwg4rwI-o7gmSBA==:4b987fd4e40303ac -->

# Self-Evolving Researcher

Self-evolution and research directive. Searches for existing knowledge, loads reference context, writes a research report, creates a new knowledge entry from its findings, signs it, and logs the evolution. The system learns from its own execution.

```xml
<directive name="self_evolving_researcher" version="1.0.0">
  <metadata>
    <description>Self-evolving researcher — searches knowledge, synthesizes a research report, creates a new knowledge entry from findings, signs it, and logs the evolution. Demonstrates fetch, execute, and sign across all item types.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022">Self-evolving research with knowledge creation and signing</model>
    <limits turns="12" tokens="4096" />
    <permissions>
      <fetch>
        <knowledge>*</knowledge>
      </fetch>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <sign>
        <knowledge>*</knowledge>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="research_topic" type="string" required="true">
      The topic to research and create knowledge about
    </input>
    <input name="workspace_dir" type="string" required="true">
      Path to the directory for research outputs and logs
    </input>
  </inputs>

  <outputs>
    <success>Self-evolving research complete — report written, knowledge entry created and signed, evolution logged. New knowledge: {input:research_topic}-learnings</success>
    <failure>Research pipeline failed — check knowledge search results, verify {input:workspace_dir} is writable, and ensure .ai/knowledge/ directory exists</failure>
  </outputs>
</directive>
```

<process>
  <step name="search_existing_knowledge">
    Search for existing knowledge entries related to "{input:research_topic}".
  </step>
  <step name="load_reference_context">
    Load the `rye-architecture` knowledge entry for reference context about the system.
  </step>
  <step name="write_research_report">
    Synthesize findings from existing knowledge and reference context into a research report. Write it to `{input:workspace_dir}/research_report.md`.
  </step>
  <step name="create_knowledge_entry">
    Create a new knowledge entry at `.ai/knowledge/{input:research_topic}-learnings.md` with YAML frontmatter and a body summarizing the research learnings.
  </step>
  <step name="sign_knowledge">
    Sign the newly created knowledge entry `{input:research_topic}-learnings`.
  </step>
  <step name="write_evolution_log">
    Write an evolution log to `{input:workspace_dir}/evolution_log.md` documenting the actions taken and knowledge created.
  </step>
</process>
