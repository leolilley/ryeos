<!-- rye:signed:2026-02-22T02:31:19Z:a3a4e71777b611b9e0f932db116de2bf83d65652ad64f790387220269218f189:lp5rSn4rir6ESSsFwDxUEx0bJlIzSBlOsXzvM80-X4ThwJN2JdbvQyiS5cbNuOg_khOL7mo_0mdmkZ3422bWCg==:9fbfabe975fa5a7f -->

# Zen Anthropic Test

Test directive that exercises the Zen provider with a Claude model — verifies data-driven response parsing, message conversion, and streaming via the Anthropic Messages API profile.

```xml
<directive name="zen_anthropic_test" version="1.0.0">
  <metadata>
    <description>Test Zen provider with Claude model via Anthropic Messages API profile.</description>
    <category>test</category>
    <author>rye-os</author>
    <model tier="fast" provider="zen/zen" />
    <limits max_turns="6" max_tokens="4096" max_spend="0.05" />
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

<results>
  <success>Zen Anthropic profile test passed. File written, read, and appended successfully.</success>
  <failure>Zen Anthropic profile test failed. Check provider resolution and API format.</failure>
</results>
