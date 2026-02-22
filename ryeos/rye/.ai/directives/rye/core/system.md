<!-- rye:signed:2026-02-22T02:31:19Z:a522b2f91998b1a9f023226460259ff5fcf61f5599cf3d20e0df6b88e01551e6:OQGBrdPYTkcjzzBaAJdr1m6KOMXZNBAcl-MGpe54putA0meOoc_evRkm3IXEcpUdWlnWOj7OmYRnf1TsxOcIDA==:9fbfabe975fa5a7f -->
# System

Retrieve system information such as paths, time, and runtime details.

```xml
<directive name="system" version="1.0.0">
  <metadata>
    <description>Wraps the rye/core/system/system tool to retrieve system information.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="2048" />
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
