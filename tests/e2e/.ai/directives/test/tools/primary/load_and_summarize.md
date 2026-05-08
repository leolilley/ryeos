<!-- ryeos:signed:2026-03-11T07:14:55Z:4f690989663e730a975f271dcfb17e644b45f80016b1e1e0ada2afb83a26064f:e_9mqmyzwHor0TGly0rdvA1lm-y-oMqtoeMVhSGbxIhjNyajNTzdhmTDPofHGp5pEJTrHkz1IcnCVwfL282dDw==:4b987fd4e40303ac -->

# Load and Summarize Directive

Load a directive to inspect its structure, then write a summary of its metadata to a file.

```xml
<directive name="load_and_summarize" version="1.0.0">
  <metadata>
    <description>Load a directive by ID to inspect its structure, then write a structured summary of its name, description, permissions, and steps to an output file.</description>
    <category>test/tools/primary</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022">Directive inspection and summarization</model>
    <limits turns="5" tokens="2048" />
    <permissions>
      <fetch>
        <directive>*</directive>
      </fetch>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="directive_id" type="string" required="true">
      The directive ID to load and inspect (e.g., "test/tools/file_system/write_file")
    </input>
    <input name="output_path" type="string" required="true">
      File path where the summary will be written
    </input>
  </inputs>

  <outputs>
    <success>Loaded directive {input:directive_id} and wrote summary to {input:output_path}</success>
    <failure>Failed to load directive {input:directive_id} or write summary</failure>
  </outputs>
</directive>
```

<process>
  <step name="load_directive">
    Load the directive `{input:directive_id}` to inspect its structure and metadata.
  </step>
  <step name="write_summary">
    Write a structured summary of the directive's name, description, permissions, and steps to `{input:output_path}`.
  </step>
</process>
