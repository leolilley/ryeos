<!-- rye:signed:2026-03-11T07:13:35Z:ef1d32354fc54be67f3f900d9f728ac9b11cef9d26a1eee090bc09f881dc6138:YBn3X67p2qLA7l7ECp9wjL1pRtwwavTskqb5IoGHgXuVywEQu4nVpzMzLKLdby3Ru6Tmy8wnk6aZSZmaLX6iBg==:4b987fd4e40303ac -->
<!-- -->

# Orchestrate Review

Orchestrates a multi-step code review by spawning nested LLM threads for analysis and summarization, then writes a combined review.

```xml
<directive name="orchestrate_review" version="1.0.0">
  <metadata>
    <description>Orchestrate a code review by chaining analyze_code and summarize_text directives via nested LLM threads.</description>
    <category>test/graphs</category>
    <author>rye-os</author>
    <model tier="general" provider="zen/zen" />
    <limits turns="12" tokens="40000" spend="0.50" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
        <tool>rye.agent.threads.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="code_snippet" type="string" required="true">
      The code to review.
    </input>
    <input name="output_dir" type="string" required="true">
      Directory for output files.
    </input>
  </inputs>

  <outputs>
    <output name="review">The final combined review text</output>
    <output name="analysis_thread_id">Thread ID of the analysis step</output>
    <output name="summary_thread_id">Thread ID of the summary step</output>
  </outputs>
</directive>
```

<process>
  <step name="analyze">
    Here is the code to review:

    ```
    {input:code_snippet}
    ```

    Spawn a nested thread to analyze the code using the `test/graphs/analyze_code` directive with the code above as `code_snippet` and output path `{input:output_dir}/orchestrated_analysis.json`. Use limit overrides: turns=6, spend=0.05.

    Wait for the result. Note the thread_id from the response.
  </step>

  <step name="read_analysis">
    Read the analysis file from `{input:output_dir}/orchestrated_analysis.json`.
  </step>

  <step name="summarize">
    Spawn another nested thread to summarize the analysis JSON using the `test/graphs/summarize_text` directive with the analysis content as `text` and output path `{input:output_dir}/orchestrated_summary.md`. Use limit overrides: turns=4, spend=0.03.

    Wait for the result. Note the thread_id.
  </step>

  <step name="write_review">
    Write a comprehensive review to `{input:output_dir}/orchestrated_review.md` that includes:
    1. The original code (from the input)
    2. The analysis results (from the analysis thread)
    3. The summary (from the summary thread)
    4. Your own assessment and recommendations
  </step>

  <step name="return_result">
    Return `review`, `analysis_thread_id`, and `summary_thread_id` using directive_return.
  </step>
</process>
