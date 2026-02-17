## User

Execute the directive as specified now.



# System

Retrieve system information such as paths, time, and runtime details.

```xml
<directive name="system" version="1.0.0">
  <metadata>
    <description>Wraps the rye/core/system/system tool to retrieve system information.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="haiku" />
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
    Validate that paths is one of: paths, time, runtime, all. Default to "all" if not provided.
  </step>

  <step name="call_system_tool">
    Call the system tool with the specified item.
    `rye_execute(item_type="tool", item_id="rye/core/system/system", parameters={"item": "paths"})`
  </step>

  <step name="return_result">
    Return the system information to the user.
  </step>
</process>

---

## Error

Provider 'rye/agent/providers/anthropic' failed (HTTP 401): x-api-key header is required

