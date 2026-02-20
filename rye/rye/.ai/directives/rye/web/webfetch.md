<!-- rye:signed:2026-02-20T01:09:07Z:5aa7c3b6bb16adb4ab4d40a5bfa868ad185bd9860cdb5bc446ae5cf652a7b358:QoZlq50IGp0lppIkvyfm4nw5QMUKHLiwv9586plSMmVZDJnpY9NomB25RSUJjNbP78imXleTg8nNMjor3dE6Ag==:440443d0858f0199 -->
# Web Fetch

Fetch the content of a web page and return it in the specified format.

```xml
<directive name="webfetch" version="1.0.0">
  <metadata>
    <description>Fetch web page content and return it as text, markdown, or HTML.</description>
    <category>rye/web</category>
    <author>rye-os</author>
    <model tier="fast" />
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
