<!-- rye:signed:2026-04-19T09:49:53Z:18e174aee63c5608cebbf60b345465117971f311352e1ced6da0db957756b897:jHrleZEOgpisWHQPkF5NBV1xGqN3ZT9edeUz0zyNZ8gO++JBgnLbD0sqZgbiOBmQuW3DLoKEf44IC4EAPHNQAQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
    `rye_execute(item_id="rye/core/system/system", parameters={"item": "{input:item}"})`
  </step>

  <step name="return_result">
    Return the system information to the user.
  </step>
</process>
