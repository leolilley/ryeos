<!-- rye:signed:2026-02-26T05:52:23Z:4a69e6391587a2f9d993edc3440317cf1d42b93a0022b7c8c507f28a7edde235:HENv278cEeNrHIxQgaHM4IyqFzjnDWtxxQrBV5rdFVnsXF3nsjc0YNuj3tXZKOPdtKrvpRf0Wi6bMzyBGZNvDQ==:4b987fd4e40303ac -->
# System

Retrieve system information such as paths, time, and runtime details.

```xml
<directive name="system" version="1.0.0">
  <metadata>
    <description>Wraps the rye/core/system/system tool to retrieve system information.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="item" type="string" required="false">
      What system info to retrieve. One of: paths, time, runtime, all. Default: "all"
    </input>
  </inputs>

  <outputs>
    <output name="system_info">The requested system information</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item} is one of: paths, time, runtime, all. Default to "all" if not provided.
  </step>

  <step name="call_system_tool">
    Call the system tool with the specified item.
    `rye_execute(item_type="tool", item_id="rye/core/system/system", parameters={"item": "{input:item}"})`
  </step>

  <step name="return_result">
    Return the system information to the user.
  </step>
</process>
