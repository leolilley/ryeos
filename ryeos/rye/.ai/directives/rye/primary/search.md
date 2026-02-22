<!-- rye:signed:2026-02-22T02:31:19Z:f220fac3d71d8c8aa4219a995ffa4a8bf0b8f20131c06efea7d288803c613a9d:uKvCA3SDXJvd6sdqdzKvdvn4qvY7XzEI-q_jD8tjjNvv9XKSui3mbNZ29skBxUXrZKbyRRsOG400_t92aWC6AQ==:9fbfabe975fa5a7f -->
# Search

Search for directives, tools, or knowledge by scope and query.

```xml
<directive name="search" version="1.0.0">
  <metadata>
    <description>Search for directives, tools, or knowledge items by scope and query string.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <search>
        <directive>*</directive>
        <tool>*</tool>
        <knowledge>*</knowledge>
      </search>
    </permissions>
  </metadata>

  <inputs>
    <input name="query" type="string" required="true">
      Search keywords or phrase to match against item names, descriptions, and content
    </input>
    <input name="scope" type="string" required="true">
      Capability-format scope (e.g., "directive", "tool.rye.core.*", "knowledge")
    </input>
    <input name="space" type="string" required="false">
      Search space: project, user, system, or all (default: "all")
    </input>
    <input name="limit" type="integer" required="false">
      Maximum number of results to return (default: 10)
    </input>
  </inputs>

  <outputs>
    <output name="results">List of matching items with id, type, score, and summary</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:query} is non-empty and {input:scope} is a valid capability-format scope string.
    Default {input:space} to "all" and {input:limit} to 10 if not provided.
  </step>

  <step name="call_search">
    Execute the search tool:
    `rye_search(scope="{input:scope}", query="{input:query}", space="{input:space}", limit={input:limit})`
  </step>

  <step name="return_results">
    Return the search results to the caller. Each result includes the item id, type, relevance score, and a brief summary.
  </step>
</process>
