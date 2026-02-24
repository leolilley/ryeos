<!-- rye:signed:2026-02-22T02:31:19Z:44f6fe84f4d3ea9efb826457e28438475b0504e155263a3ae3c956ea46d0b5f6:eJw26JzzkjhtUdwd2gahzBPG534BrihtSLh_njJmPak7ZL2NKgy3Igx-iKRvNQpsU_f1qUEoRmqIB_LBYNX3BA==:9fbfabe975fa5a7f -->

# Zen OpenAI Test

Test directive that exercises the Zen provider with an OpenAI-compatible model — verifies data-driven response parsing, message conversion, and streaming via the Chat Completions API profile.

```xml
<directive name="zen_openai_test" version="1.0.0">
  <metadata>
    <description>Test Zen provider with OpenAI-compatible model (MiniMax) via Chat Completions profile.</description>
    <category>test</category>
    <author>rye-os</author>
    <model id="minimax-m2.5-free" provider="zen/zen" />
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
    Write a test marker file to {input:output_dir|outputs}/zen_openai.txt with content "zen_openai_start".
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/zen_openai.txt", "content": "zen_openai_start", "create_dirs": true})`
  </step>

  <step name="read_back">
    Read the file back to verify it was written correctly.
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"path": "{input:output_dir|outputs}/zen_openai.txt"})`
  </step>

  <step name="append_info">
    Append the model name and a completion marker to the file.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/zen_openai.txt", "content": "\nmodel: minimax-m2.5-free via zen openai_compat profile\nstatus: complete", "mode": "append"})`
  </step>
</process>

<success_criteria>
  <criterion>File outputs/zen_openai.txt exists with marker content</criterion>
  <criterion>File read-back matches written content</criterion>
  <criterion>Append operation succeeds</criterion>
</success_criteria>
