<!-- rye:validated:2026-02-10T03:00:00Z:placeholder -->

# Spawn Child Directive

Recursive directive spawning test. Parent writes a plan file, spawns child directive `test/tools/file_system/write_file` to write a greeting, reads the child's output to verify, then appends a completion summary.

```xml
<directive name="07_spawn_child" version="1.0.0">
  <metadata>
    <description>Recursive directive spawning — parent orchestrates a plan, spawns a child directive to write a greeting file, verifies the child's output, and appends a completion summary.</description>
    <category>test</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022">Child thread spawning and cross-thread verification</model>
    <limits turns="8" tokens="3072" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <execute>
        <directive>test/*</directive>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="greeting" type="string" required="true">
      The greeting message the child directive should write
    </input>
    <input name="output_dir" type="string" required="true">
      Path to the directory where output files will be created
    </input>
  </inputs>

  <outputs>
    <success>Parent orchestration complete — plan written, child spawned and verified, completion summary appended to {input:output_dir}/plan.md</success>
    <failure>Spawn child pipeline failed — check that test/tools/file_system/write_file directive exists and {input:output_dir} is writable</failure>
  </outputs>
</directive>
```

<process>
  <step name="write_plan">
    <description>Write a plan file describing what the parent will orchestrate</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="{input:output_dir}/plan.md" />
      <param name="content" value="# Spawn Child Plan

## Objective
Orchestrate child directive test/tools/file_system/write_file to write a greeting.

## Stages
1. Write this plan file
2. Spawn child directive with greeting: {input:greeting}
3. Verify child output at {input:output_dir}/greeting.md
4. Append completion summary to this file
" />
    </execute>
  </step>

  <step name="spawn_child">
    <description>Spawn child directive test/tools/file_system/write_file to write the greeting to a file</description>
    <execute item_type="directive" item_id="test/tools/file_system/write_file">
      <param name="message" value="{input:greeting}" />
      <param name="output_path" value="{input:output_dir}/greeting.md" />
    </execute>
  </step>

  <step name="verify_child_output">
    <description>Read the greeting file written by the child directive to verify it completed</description>
    <execute item_type="tool" item_id="rye/file-system/fs_read">
      <param name="path" value="{input:output_dir}/greeting.md" />
    </execute>
  </step>

  <step name="append_completion">
    <description>Append a completion summary to the plan file</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="{input:output_dir}/plan.md" />
      <param name="content" value="
## Completion Summary
- Child directive test/tools/file_system/write_file executed successfully
- Greeting file verified at {input:output_dir}/greeting.md
- Pipeline complete
" />
      <param name="mode" value="append" />
    </execute>
  </step>
</process>
