<!-- rye:signed:2026-02-20T01:09:07Z:4215daf85b978a921a48bbdca35b64b1a28379dcf52193d68c10a9092e312a6f:cTfKKJTfNTOgQJo5OOl0iSTqHO4Nb0C0skl3cWEH5lLs5ohy4ieDxW-xhfAoA71PYDgvFHqyzvHgthuB5PeyBg==:440443d0858f0199 -->
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
