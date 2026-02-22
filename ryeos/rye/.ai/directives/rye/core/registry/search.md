<!-- rye:signed:2026-02-22T02:31:19Z:4215daf85b978a921a48bbdca35b64b1a28379dcf52193d68c10a9092e312a6f:6dznx1w36n7K4s-LciWgLt3z9QzK6_5pcQ4cMZWc_AHEJetuebb2s8zw60y_cw4rxwujps0OKbeH8p-VP3cwDg==:9fbfabe975fa5a7f -->
# Registry Search

Search the registry for items.

```xml
<directive name="search" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=search to search the registry.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.registry.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="query" type="string" required="true">
      Search query string
    </input>
    <input name="item_type" type="string" required="false">
      Filter by item type (e.g., directive, tool, knowledge)
    </input>
  </inputs>

  <outputs>
    <output name="results">Search results from the registry</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:query} is non-empty.
  </step>

  <step name="call_registry_search">
    Call the registry tool with action=search.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "search", "query": "{input:query}", "item_type": "{input:item_type}"})`
  </step>

  <step name="return_result">
    Return the search results to the user.
  </step>
</process>
