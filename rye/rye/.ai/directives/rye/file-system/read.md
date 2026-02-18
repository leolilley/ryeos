<!-- rye:signed:2026-02-18T05:40:31Z:bc1f7d996a4ca532f248e2e78c821fcbbf8836ae98de41eea92c773a07a5125c:wmiiHfhufRS1serkBCgaY1NA7890pT1UBqW1uw0pR2_7UZ75jJmLl2DKJ_q-w1osT7rY999MPo9IpYkqGOrFAQ==:440443d0858f0199 -->
# Read

Read file contents with optional offset and line limit.

```xml
<directive name="read" version="1.0.0">
  <metadata>
    <description>Read file contents from disk with optional offset and line limit for large files.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.file-system.read</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="file_path" type="string" required="true">
      Path to the file to read (absolute or relative to project root)
    </input>
    <input name="offset" type="integer" required="false">
      Starting line number, 1-indexed (default: 1)
    </input>
    <input name="limit" type="integer" required="false">
      Maximum number of lines to return (default: 2000)
    </input>
  </inputs>

  <outputs>
    <output name="content">File contents with line numbers</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:file_path} is non-empty.
    Default {input:offset} to 1 and {input:limit} to 2000 if not provided.
  </step>

  <step name="call_read">
    Read the file:
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"file_path": "{input:file_path}", "offset": {input:offset}, "limit": {input:limit}})`
  </step>

  <step name="return_result">
    Return the file contents with line numbers.
  </step>
</process>
