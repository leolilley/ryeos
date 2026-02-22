<!-- rye:signed:2026-02-22T02:31:19Z:35ba7a39c0abc128f4c5bba30c6ee37375495a0b4c01db5235b83de6de562051:sV_Sppf7s-qaYQ0WtD0TzksdBlhM9fL5d-spYJoWo7_TnSxWw_gI9jCpUNL9lVbpUZy14wBGVFyayxSoofhNDA==:9fbfabe975fa5a7f -->

# Analyze Code

Analyzes a code snippet — identifies the language, counts functions, writes a JSON analysis to a file.

```xml
<directive name="analyze_code" version="1.0.0">
  <metadata>
    <description>Analyze a code snippet, write JSON analysis to a file, and return structured results.</description>
    <category>test/graphs</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="6" tokens="20000" />
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
