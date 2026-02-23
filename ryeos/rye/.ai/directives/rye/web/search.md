<!-- rye:signed:2026-02-23T02:07:54Z:1f8e17f934c53d9d6e7ae542e31ebe5fe26d795dcf066748f12993839d611aef:VfwDR3ks7yWKTrJ3GoYzyFA_H41jja_Q_sQMZ5-0vedGvxm-yd5J0aGIUAGXmDOunXmoyj9CDiXCOnmHPYmrAg==:9fbfabe975fa5a7f -->
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
    <limits max_turns="3" max_tokens="4096" />
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
