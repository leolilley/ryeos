<!-- rye:signed:2026-02-18T05:40:31Z:04e8cf618672eb2695ab1216aadea854e8bd3bf04a691d4a773c0928e2e300c6:fjdBKbXyLdHey4OPZr0FdgjXxW2BKJOqvLvKJWK6OnFonMPyjy_NiRBjzMGb5-GCLVfFu-BbG2XXHA5bKx2rAA==:440443d0858f0199 -->
# Registry Search

Search the registry for items.

```xml
<directive name="search" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=search to search the registry.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="haiku" />
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
