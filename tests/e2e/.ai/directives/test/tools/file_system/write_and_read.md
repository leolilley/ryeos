<!-- rye:signed:2026-03-11T07:13:35Z:e3f1b69c28369a7d9c11ce8e019e23b418829134adcfde64510a5f1db49e3d4d:T_mHI4hoDGCqLvrakhVAgS1KlvxcrjNpTd-uxjfmP50GTIn1V2mYWBAHGZ1cqVDIzUEOqc8W9DrPz40NzB01Cg==:4b987fd4e40303ac -->

# Write and Read

Two-step directive that writes content to a file, then reads it back to verify the contents match.

```xml
<directive name="write_and_read" version="1.0.0">
  <metadata>
    <description>Write content to a file then read it back to verify correctness.</description>
    <category>test/tools/file_system</category>
    <author>rye-os</author>
    <model tier="fast" id="claude-3-5-haiku-20241022" />
    <limits turns="5" tokens="2048" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="content" type="string" required="true">
      The content to write to the file.
    </input>
    <input name="file_path" type="string" required="true">
      The file path to write to and read from.
    </input>
  </inputs>

  <outputs>
    <success>Content written to {input:file_path} and verified successfully.</success>
    <failure>Content mismatch detected after write — file contents do not match input.</failure>
  </outputs>
</directive>
```

<process>
  <step name="write_content">
    Write the content "{input:content}" to `{input:file_path}`.
  </step>
  <step name="read_and_verify">
    Read `{input:file_path}` back and confirm the contents match the original input.
  </step>
</process>
