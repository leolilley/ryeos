<!-- rye:signed:2026-02-22T02:31:19Z:1309a0f3fb2ca932821e04c4222c871825a036278bfab51be60f316449c82261:79LQw0GZUDBkP9hAVJRQqIZB1-TrdVd13EoxqE81tFvz8Te_g0OnohZdBcT8mw1Y7bnkyFzlNIJjuz3jlyxQBg==:9fbfabe975fa5a7f -->
# Summarize Text

Takes text content and writes a concise summary to a file.

```xml
<directive name="summarize_text" version="1.0.0">
  <metadata>
    <description>Takes text content and writes a concise summary to a file.</description>
    <category>test/graphs</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="4" tokens="16000" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="text" type="string" required="true">
      The text to summarize.
    </input>
    <input name="output_path" type="string" required="true">
      Where to write the summary.
    </input>
  </inputs>

  <outputs>
    <output name="summary">A concise 2-3 sentence summary</output>
    <output name="word_count">Word count of the original text</output>
  </outputs>
</directive>
```

<process>
  <step name="summarize_and_write">
    Here is the text to summarize:

    ```
    {input:text}
    ```

    Write a concise 2-3 sentence summary of this text. Count the words in the original text.

    Then write the summary to `{input:output_path}`:
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_path}", "content": "<your summary text>"})`
  </step>

  <step name="return_result">
    Return `summary` and `word_count` using directive_return with the values you determined.
  </step>
</process>
