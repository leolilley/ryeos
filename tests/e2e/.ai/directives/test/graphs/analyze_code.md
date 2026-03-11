<!-- rye:signed:2026-03-11T08:29:37Z:99bf1a3c40c20310248470011d9d0fa27e422525b6ccc4c5b3651392adbb859a:sVAv2wow9LzZLZ3DUxtZvhcvbGqauGO-9jDFVQQWzZEQfuwfPTogl1WRLDgHGjlbtppqVSUevn1SQz2byqYFDg==:4b987fd4e40303ac -->
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
      Project-relative path for the JSON analysis file (e.g. "analysis-result.json").
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
  <step name="analyze_code">
    Here is the code to analyze:

    ```
    {input:code_snippet}
    ```

    Analyze the code and determine:
    - The programming language
    - The number of function/method definitions (def, async def, function, etc.)
    - A 2-3 sentence summary of what the code does

    Write the result as a JSON object to `{project_path}/{input:output_path}` with keys: `language`, `function_count`, `summary`.
  </step>

  <step name="return_result">
    Return `language`, `function_count`, and `summary` using directive_return.
  </step>
</process>
