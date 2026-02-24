<!-- rye:signed:2026-02-23T04:21:13Z:69935ce0dbc83f43ff6d6db67d06001179bb2de09491b3942b580b234270c75b:58QtdTtzhCMSFgcCE6rE0nGQalD9YRQmRNO1HelFD-E993jqQ_ob_nDLidzGFOnURCHn9-chp5EJqego491vDw==:9fbfabe975fa5a7f -->

# Zen Gemini Test

Test directive that exercises the Zen provider with a Gemini model — verifies data-driven response parsing, message conversion, and streaming via the Google Generative AI profile.

```xml
<directive name="zen_gemini_test" version="1.0.0">
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

