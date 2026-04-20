<!-- rye:signed:2026-04-19T09:49:53Z:58dc44d716d8fb348c195ab4da701dd60372c86bf72db9ec9a62cfd7446d3d12:cUGNq5QfuJjR/lmmVNnCBSU9Tq7pVhbUeJCrJOhLX329wyDHRKWXOgue6oPGyAPZZEQSlCnsMmaoogBb1vf6Bw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# Execute

Execute a directive, tool, or knowledge item by id with optional parameters.

```xml
<directive name="execute" version="1.0.0">
  <metadata>
    <description>Execute a directive, tool, or knowledge item. Supports dry_run mode for validation without side effects.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" />
    <permissions>
      <execute>
        <tool>*</tool>
        <directive>*</directive>
        <knowledge>*</knowledge>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="item_type" type="string" required="true">
      Type of item to execute: directive, tool, or knowledge
    </input>
    <input name="item_id" type="string" required="true">
      Fully qualified item id (e.g., "rye/file-system/read", "rye/bash/bash")
    </input>
    <input name="parameters" type="object" required="false">
      Parameters to pass to the item's execute function as a JSON object
    </input>
    <input name="dry_run" type="boolean" required="false">
      If true, validate inputs and permissions without executing (default: false)
    </input>
  </inputs>

  <outputs>
    <output name="result">Execution result from the item</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item_type} is one of: directive, tool, knowledge.
    Validate that {input:item_id} is non-empty.
    Default {input:dry_run} to false if not provided.
  </step>

  <step name="call_execute">
    Execute the item:
    `rye_execute(item_id="{input:item_id}", parameters={input:parameters}, dry_run={input:dry_run})`
  </step>

  <step name="return_result">
    Return the execution result to the caller. If dry_run was true, return the validation result instead.
  </step>
</process>
