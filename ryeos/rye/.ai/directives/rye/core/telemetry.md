<!-- rye:signed:2026-02-21T05:56:40Z:11cc710adfad74bfe344b4a6a6a13de51cfb12455d2bbd5559d7b732b785a7dc:YCVxEhWvjKU0-_6MEvB1njoLF6_SKTWCfUfus8YZa49pFgCekYiOKRZ1R7dA2nJagJyKrjfOwqxNGdfXS8mBAA==:9fbfabe975fa5a7f -->
# Telemetry

Retrieve telemetry data including logs, stats, and errors.

```xml
<directive name="telemetry" version="1.0.0">
  <metadata>
    <description>Wraps the rye/core/telemetry/telemetry tool to retrieve telemetry data.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="4096" />
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
