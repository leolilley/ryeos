<!-- rye:signed:2026-04-01T08:05:35Z:30f0944a7457fb65894b1fad8559543e2cb8ff4f30143ddd3d7b505415bd14a1:3RAVc51inSJ9-jehtJSGhIsgmLM8AzIMdTnDr273CgnnQQCQ8pAOQgMNmqufo5mtBH-8I0Z6W4mhhY_d9Pt2Aw:4b987fd4e40303ac -->

# Zen Gemini Test

Test directive that exercises the Zen provider with a Gemini model — verifies data-driven response parsing, message conversion, and streaming via the Google Generative AI profile.

```xml
<directive name="zen_gemini_test" version="1.0.0" extends="rye/agent/core/base">
  <metadata>
    <description>Test Zen provider with Gemini model via Google Generative AI profile.</description>
    <category>test</category>
    <author>rye-os</author>
    <model id="gemini-3-flash" provider="zen/zen" />
    <limits turns="6" tokens="32000" spend="0.10" />
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
    Write a test marker file to {input:output_dir|outputs}/zen_gemini.txt with content "zen_gemini_start".
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/zen_gemini.txt", "content": "zen_gemini_start", "create_dirs": true})`
  </step>

  <step name="read_back">
    Read the file back to verify it was written correctly.
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"path": "{input:output_dir|outputs}/zen_gemini.txt"})`
  </step>

  <step name="append_info">
    Append the model name and a completion marker to the file.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/zen_gemini.txt", "content": "\nmodel: gemini-3-flash via zen gemini profile\nstatus: complete", "mode": "append"})`
  </step>
</process>

<success_criteria>
  <criterion>File outputs/zen_gemini.txt exists with marker content</criterion>
  <criterion>File read-back matches written content</criterion>
  <criterion>Append operation succeeds</criterion>
</success_criteria>

