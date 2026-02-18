<!-- rye:signed:2026-02-18T05:40:31Z:353d7921fbc3adf9b2c229e25e5f192093ee8fcf6423c664b0bf9691471447f4:SnnWezuXKSkQ7cVDT5_q61YtThECc_-Nl06z1P2ATpqwADcIdjIcDhr0FkZlT21HcVqPztpVYoxhUb7XA_hbAA==:440443d0858f0199 -->
# Web Search

Search the web using DuckDuckGo or Exa and return results.

```xml
<directive name="websearch" version="1.0.0">
  <metadata>
    <description>Search the web using configurable provider and return ranked results.</description>
    <category>rye/web</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.web.websearch</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="query" type="string" required="true">Search query string</input>
    <input name="num_results" type="integer" required="false">Number of results to return (default 10, max 20)</input>
    <input name="provider" type="string" required="false">Search provider: "duckduckgo" or "exa"</input>
  </inputs>

  <outputs>
    <output name="results">List of search results with titles, URLs, and snippets</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:query} is non-empty. If empty, halt with an error.
  </step>

  <step name="execute_search">
    Call the web search tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/web/websearch", parameters={"query": "{input:query}", "num_results": "{input:num_results}", "provider": "{input:provider}"})`
  </step>

  <step name="return_results">
    Return the search results as {output:results}.
  </step>
</process>
