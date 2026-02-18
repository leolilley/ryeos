<!-- rye:signed:2026-02-18T05:40:31Z:35db36d2f1a11cf6775cf3ac75416295e646b7c2bf11807960de923547ebc6e1:BW7nxIRH5ovAkvr5mlK0530ofq0UwZQ3Lsjfg9gElrRYRBQdOF8BpotHTDWGl0MbaQ59X-NtobFxvQh-OLDeCQ==:440443d0858f0199 -->
# Execute

Execute a directive, tool, or knowledge item by id with optional parameters.

```xml
<directive name="execute" version="1.0.0">
  <metadata>
    <description>Execute a directive, tool, or knowledge item. Supports dry_run mode for validation without side effects.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="4" max_tokens="4096" />
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
    `rye_execute(item_type="{input:item_type}", item_id="{input:item_id}", parameters={input:parameters}, dry_run={input:dry_run})`
  </step>

  <step name="return_result">
    Return the execution result to the caller. If dry_run was true, return the validation result instead.
  </step>
</process>
