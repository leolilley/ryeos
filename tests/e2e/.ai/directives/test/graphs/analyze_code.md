<!-- rye:signed:2026-02-23T09:06:00Z:8f087db0dc93e42fb3e100a99cb0864154397f634bd169d822ffbe3b15f777dd:apGaesEULaKglHURFOZS333fI6TOljpPVoF4HCkomIcItEQikmtLVatd1agi_Cs8HkA_DsYQRczazVIg7-ZWCQ==:9fbfabe975fa5a7f -->
<!-- -->

# Analyze Code

Analyzes a code snippet — identifies the language, counts functions, writes a JSON analysis to a file.

```xml
<directive name="analyze_code" version="1.0.0">
  <metadata>
    <description>Analyze a code snippet, write JSON analysis to a file, and return structured results.</description>
    <category>test/graphs</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="20000" spend="0.05" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="code_snippet" type="string" required="true">
      The code to analyze.
    </input>
    <input name="output_path" type="string" required="true">
      Where to write the JSON analysis file.
    </input>
  </inputs>

  <outputs>
    <output name="language">The programming language identified</output>
    <output name="function_count">Number of function/method definitions found</output>
    <output name="summary">A 2-3 sentence summary of what the code does</output>
  </outputs>
</directive>
```

<process>
  <step name="write_analysis">
    Here is the code to analyze:

    ```
    {input:code_snippet}
    ```

    Analyze it and determine: the programming language, the number of function/method definitions (def, async def, function, etc.), and a 2-3 sentence summary of what the code does.

    Then write the result as a JSON object to `{input:output_path}`:
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_path}", "content": "{\"language\": \"...\", \"function_count\": N, \"summary\": \"...\"}"})`
  </step>

  <step name="return_result">
    Return `language`, `function_count`, and `summary` using directive_return with the values you determined.
  </step>
</process>
