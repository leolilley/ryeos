<!-- rye:signed:2026-02-18T05:40:31Z:519eaeda35dece7f0c388591d0f99a07080e6bf30ae598602587be217b40b0d1:NmA1FAcPuUp3w6Z2t1txL9enc0DXHoqYCEt4X3TNoQfIgHiYqpd8sODDdgUmG2gkErtoLdyGGqGSF6WbomWkDg==:440443d0858f0199 -->
# Telemetry

Retrieve telemetry data including logs, stats, and errors.

```xml
<directive name="telemetry" version="1.0.0">
  <metadata>
    <description>Wraps the rye/core/telemetry/telemetry tool to retrieve telemetry data.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="haiku" />
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
