<!-- rye:signed:2026-03-11T07:13:35Z:0cfde65873b8fec596dcf0f7c52916993e448c60ef9a1311c7b6a144a8d7da5f:yVBpXNlaWl53SHBW-wiD_kKHryVkfsjZFqimLGwMq6ahuu9np5EIszMSzvGpN6LAlmsKohS6VDdV6O0jkz1WDQ==:4b987fd4e40303ac -->
<!-- -->

# Summarize Text

Takes text content and writes a concise summary to a file.

```xml
<directive name="summarize_text" version="1.0.0">
  <metadata>
    <description>Takes text content and writes a concise summary to a file.</description>
    <category>test/graphs</category>
    <author>rye-os</author>
    <model tier="fast" provider="zen" />
    <limits turns="4" tokens="16000" spend="0.05" />
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

    Write the summary to `{input:output_path}`.
  </step>

  <step name="return_result">
    Return `summary` and `word_count` using directive_return.
  </step>
</process>
