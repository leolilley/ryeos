<!-- ryeos:signed:2026-03-11T07:13:35Z:c4e58a6439eddd9e73806d8b9bd5f0c17fae60cef7a3e4a317840957799af84c:hi2R83OKYIt7yVANtsh452vbWzCFiWmwqA5AoTXbHyU3tg3cV8wd8hULqpAIXYOCpL1TBHQRVwX8KNx8tARlBQ==:4b987fd4e40303ac -->

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
