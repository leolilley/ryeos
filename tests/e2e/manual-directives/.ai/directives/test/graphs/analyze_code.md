<!-- ryeos:signed:2026-07-27T23:40:19Z:3a27f86806e60938a4bc6c7dbab116fd68e31a7922c90eb01b464d6027b2a974:0pXrBdM353398hbPbzwv4AWwu/b6JR/WbaERq4E89UCcqwrJMMxAI8rgQ1Xfh8aowYGrB9fS9u/zOdUScpWxBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
<!-- -->

# Analyze Code

Analyzes a code snippet — identifies the language, counts functions, writes a JSON analysis to a file.

```xml
<directive name="analyze_code" version="1.0.0">
  <metadata>
    <description>Analyze a code snippet, write JSON analysis to a file, and return structured results.</description>
    <category>test/graphs</category>
    <author>ryeos</author>
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
    ${inputs.code_snippet}
    ```

    Analyze the code and determine:
    - The programming language
    - The number of function/method definitions (def, async def, function, etc.)
    - A 2-3 sentence summary of what the code does

    Write the result as a JSON object to the project-relative path `${inputs.output_path}` with keys: `language`, `function_count`, `summary`.
  </step>

  <step name="return_result">
    Return `language`, `function_count`, and `summary` using directive_return.
  </step>
</process>
