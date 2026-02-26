<!-- rye:signed:2026-02-26T06:42:50Z:415fd8b5e5f57a66dbc29808b38614736a63b3201e086da24ea4692325849ff8:OyXjoO1tuoBhPwq0-374_C_zMRx5PiIXxNgrhToJTebuZq2S4SeirVBarkwofDzkFFfcjnSN28i2Xhqmu09DBA==:4b987fd4e40303ac -->
# Registry Search

Search the registry for items.

```xml
<directive name="search" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=search to search the registry.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
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
