<!-- rye:signed:2026-02-26T05:02:48Z:bb91580d1d4e297308c1ac99ecd753880a69105cbcc85a0cd9030ff25e07fc3e:ZMHPNCbCZtaxzjwNFi_HNnGuUwmGwseV9rEUkQMqIOHiLogBM-xIimAZ2xzwINMCG76re5gwVUueCFIAzttyDQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->
# Web Search

Search the web using DuckDuckGo or Exa and return results.

```xml
<directive name="search" version="1.0.0">
  <metadata>
    <description>Search the web using configurable provider and return ranked results.</description>
    <category>rye/web</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.web.search.*</tool>
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
    `rye_execute(item_type="tool", item_id="rye/web/search/search", parameters={"query": "{input:query}", "num_results": "{input:num_results}", "provider": "{input:provider}"})`
  </step>

  <step name="return_results">
    Return the search results as {output:results}.
  </step>
</process>
