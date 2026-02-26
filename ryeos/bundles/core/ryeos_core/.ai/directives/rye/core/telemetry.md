<!-- rye:signed:2026-02-26T06:42:50Z:f97828abf73dc0f6bcfbfdb77544d1d5b0dfada10a1b14ecfd9198cf87e615db:mc2_CROaCbdwh6NehI3hr47u1YSeb8WVTwKEmqKChViLH0UrhwLfurZJ7oAQ2eBaMYzeksBf54m4cM1VyhQ6Dg==:4b987fd4e40303ac -->
# Telemetry

Retrieve telemetry data including logs, stats, and errors.

```xml
<directive name="telemetry" version="1.0.0">
  <metadata>
    <description>Wraps the rye/core/telemetry/telemetry tool to retrieve telemetry data.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.core.telemetry.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="item" type="string" required="false">
      What telemetry data to retrieve. One of: logs, stats, errors, all. Default: "all"
    </input>
    <input name="level" type="string" required="false">
      Log level filter. Default: "INFO"
    </input>
    <input name="limit" type="integer" required="false">
      Maximum number of entries to return. Default: 50
    </input>
  </inputs>

  <outputs>
    <output name="telemetry_data">The requested telemetry data</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item} is one of: logs, stats, errors, all. Default to "all" if not provided.
    Default {input:level} to "INFO" and {input:limit} to 50 if not provided.
  </step>

  <step name="call_telemetry_tool">
    Call the telemetry tool with the specified parameters.
    `rye_execute(item_type="tool", item_id="rye/core/telemetry/telemetry", parameters={"item": "{input:item}", "level": "{input:level}", "limit": {input:limit}})`
  </step>

  <step name="return_result">
    Return the telemetry data to the user.
  </step>
</process>
