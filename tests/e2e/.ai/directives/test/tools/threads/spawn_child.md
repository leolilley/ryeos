<!-- ryeos:signed:2026-03-11T07:14:55Z:b30c0392986bbae7602e6f3af0adea94b21c164df1292d75138b1dcefa05569b:so4LcWWK75V3DwJYR7-NDDI2Fh-CKi7YB0NkF85oxOHrMWIU9af6IYX7qDKSv1ty6KTGkE-MRfPB3ZLBjX_vBA==:4b987fd4e40303ac -->

# Spawn Child Directive

Recursive directive spawning test. Parent writes a plan file, spawns child directive `test/tools/file_system/write_file` to write a greeting, reads the child's output to verify, then appends a completion summary.

```xml
<directive name="spawn_child" version="1.0.0">
  <metadata>
    <description>Recursive directive spawning — parent orchestrates a plan, spawns a child directive to write a greeting file, verifies the child's output, and appends a completion summary.</description>
    <category>test/tools/threads</category>
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
    Write a plan file to `{input:output_dir}/plan.md` describing the parent's orchestration stages.
  </step>
  <step name="spawn_child">
    Spawn the child directive `test/tools/file_system/write_file` with the greeting "{input:greeting}" and output path `{input:output_dir}/greeting.md`.
  </step>
  <step name="verify_child_output">
    Read `{input:output_dir}/greeting.md` to verify the child directive completed successfully.
  </step>
  <step name="append_completion">
    Append a completion summary to `{input:output_dir}/plan.md` confirming the child executed and was verified.
  </step>
</process>
