<!-- rye:signed:2026-03-11T07:13:35Z:fe7a38b51a5c10976a4b2ce72b7a00a2c688d016982e56f4e69fe853cc3661e0:Ejr0sNufEsh5pOto3JNsY5zzc2wW4QlSS57JTNMI7QooeFmfkPiQE1cr9yJ4SkqGZRb8W4bGcMnPfP3SHbKoBw==:4b987fd4e40303ac -->

# Zen Anthropic Test

Test directive that exercises the Zen provider with a Claude model — verifies data-driven response parsing, message conversion, and streaming via the Anthropic Messages API profile.

```xml
<directive name="zen_anthropic_test" version="1.0.0">
  <metadata>
    <description>Test Zen provider with Claude model via Anthropic Messages API profile.</description>
    <category>test</category>
    <author>rye-os</author>
    <model tier="fast" provider="zen/zen" />
    <limits turns="6" tokens="4096" spend="0.05" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="output_dir" type="string" required="false">
      Directory for test output files (default: outputs)
    </input>
  </inputs>

  <outputs>
    <output name="result">Contents of the test output file</output>
  </outputs>
</directive>
```

<process>
  <step name="write_marker">
    Write a test marker file to {input:output_dir|outputs}/zen_anthropic.txt with content "zen_anthropic_start".
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/zen_anthropic.txt", "content": "zen_anthropic_start", "create_dirs": true})`
  </step>

  <step name="read_back">
    Read the file back to verify it was written correctly.
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"path": "{input:output_dir|outputs}/zen_anthropic.txt"})`
  </step>

  <step name="append_info">
    Append the model name and a completion marker to the file.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/zen_anthropic.txt", "content": "\nmodel: claude via zen anthropic profile\nstatus: complete", "mode": "append"})`
  </step>
</process>

<success_criteria>
  <criterion>File outputs/zen_anthropic.txt exists with marker content</criterion>
  <criterion>File read-back matches written content</criterion>
  <criterion>Append operation succeeds</criterion>
</success_criteria>
