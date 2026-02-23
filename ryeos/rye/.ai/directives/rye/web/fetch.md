<!-- rye:signed:2026-02-23T01:12:00Z:357b043ecd307b0543b0e6828dde9ee73d3c34aa83ad1494ab3e6fe348712d83:fsC0PQsTRvSonQmbbSPw4hssewPD71mHFxbCzOM0bbgi9pxLv2-vYfIMurVkD8BCufk80KcBLV9iRzJaf_VLDg==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->
# Web Fetch

Fetch the content of a web page and return it in the specified format.

```xml
<directive name="fetch" version="1.0.0">
  <metadata>
    <description>Fetch web page content and return it as text, markdown, or HTML.</description>
    <category>rye/web</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.web.fetch.*</tool>
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
    `rye_execute(item_type="tool", item_id="rye/web/fetch/fetch", parameters={"url": "{input:url}", "format": "{input:format}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_content">
    Return the fetched content as {output:content}.
  </step>
</process>
