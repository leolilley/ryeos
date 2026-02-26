<!-- rye:signed:2026-02-26T03:49:26Z:4a69e6391587a2f9d993edc3440317cf1d42b93a0022b7c8c507f28a7edde235:z9jI_gX1HPedY4EpQI1LX8H5VmchIxio2PLj8gXXbSsAfkFQZiqetFtSIp8Uv-QEM2qNExYxM4PZPHholvJ2Dw==:9fbfabe975fa5a7f -->
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
