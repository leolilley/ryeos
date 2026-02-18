<!-- rye:signed:2026-02-18T05:40:31Z:b31a9e3ecb281885559fa5d4f230d0357f77b085b51497dc4d9467ddbf8aa714:6-pr0TDvbbhw6DHH2RisfZHKcr7HAuwSmPDRytKGgCVm4hVTQHKFD0GeeJ2C-x64wZtzBJUNpNJOra8BYny0AA==:440443d0858f0199 -->
# Search

Search for directives, tools, or knowledge by scope and query.

```xml
<directive name="search" version="1.0.0">
  <metadata>
    <description>Search for directives, tools, or knowledge items by scope and query string.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="haiku" />
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
