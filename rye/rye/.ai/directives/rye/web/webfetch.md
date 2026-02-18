<!-- rye:signed:2026-02-18T05:40:31Z:fa0c0a36e8336535432d12f8c408b5449dd410b22ca7b8f2d0aee63f99a1d7a4:2a6Jxg3dxuIWH47UyDmIDDLptWc7EW5eYprecrnlt3xjYU8FGhIsqCXGRCEIZvB0FhdxiFoov_NtnD3hmOddDQ==:440443d0858f0199 -->
# Web Fetch

Fetch the content of a web page and return it in the specified format.

```xml
<directive name="webfetch" version="1.0.0">
  <metadata>
    <description>Fetch web page content and return it as text, markdown, or HTML.</description>
    <category>rye/web</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.web.webfetch</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="url" type="string" required="true">URL to fetch (must start with http:// or https://)</input>
    <input name="format" type="string" required="false">Output format: "text", "markdown", or "html" (default "markdown")</input>
    <input name="timeout" type="integer" required="false">Request timeout in seconds (default 30)</input>
  </inputs>

  <outputs>
    <output name="content">Fetched page content in the requested format</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:url} starts with "http://" or "https://". If not, halt with an error.
  </step>

  <step name="fetch_content">
    Call the web fetch tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/web/webfetch", parameters={"url": "{input:url}", "format": "{input:format}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_content">
    Return the fetched content as {output:content}.
  </step>
</process>
