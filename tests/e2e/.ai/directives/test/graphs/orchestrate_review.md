<!-- rye:signed:2026-02-18T10:03:52Z:7a04ecd7e14570b3dc877d185838b2fe960714e33e6e32c0538db3d8b8a070e3:SomZ5D2bFpRkkM7FAsqkjSYlFmbUBRZu8o5Dw4gClJXTnzFTopcsSZF5I4ZYkY_5JnnrHtmuP6KuxnvFw_yOCQ==:440443d0858f0199 -->
# Orchestrate Review

Orchestrates a multi-step code review by spawning nested LLM threads for analysis and summarization, then writes a combined review.

```xml
<directive name="orchestrate_review" version="1.0.0">
  <metadata>
    <description>Orchestrate a code review by chaining analyze_code and summarize_text directives via nested LLM threads.</description>
    <category>test/graphs</category>
    <author>rye-os</author>
    <model tier="sonnet" />
    <limits turns="12" tokens="40000" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye.agent.threads.*</tool></execute>
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

    First, spawn a nested LLM thread to analyze this code by calling:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "test/graphs/analyze_code", "inputs": {"code_snippet": "<paste the code above here>", "output_path": "{input:output_dir}/orchestrated_analysis.json"}, "limit_overrides": {"turns": 6, "spend": 0.05}})`

    Wait for the result. Note the thread_id from the response.
  </step>

  <step name="read_analysis">
    Now read the analysis file that was written:

    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"file_path": "{input:output_dir}/orchestrated_analysis.json"})`
  </step>

  <step name="summarize">
    Take the analysis JSON you just read and spawn another nested LLM thread to summarize it:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "test/graphs/summarize_text", "inputs": {"text": "<paste the analysis JSON content here>", "output_path": "{input:output_dir}/orchestrated_summary.md"}, "limit_overrides": {"turns": 4, "spend": 0.03}})`

    Wait for the result. Note the thread_id.
  </step>

  <step name="write_review">
    Now write a comprehensive review to `{input:output_dir}/orchestrated_review.md` that includes:
    1. The original code (from the input)
    2. The analysis results (from the analysis thread)
    3. The summary (from the summary thread)
    4. Your own assessment and recommendations

    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"file_path": "{input:output_dir}/orchestrated_review.md", "content": "<your comprehensive review>"})`
  </step>

  <step name="return_result">
    Return `review`, `analysis_thread_id`, and `summary_thread_id` using directive_return.
  </step>
</process>
