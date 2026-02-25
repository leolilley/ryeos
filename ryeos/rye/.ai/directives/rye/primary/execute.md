<!-- rye:signed:2026-02-25T07:50:41Z:25918551edea0abbc0b701d436fa89363e34a9e144ca48fe20930a3ea8d97a08:TS_AmdaUm4bVfI_nmd8--lP0oITslINJK6X0aRjDAwZITDPOAjoPb6nFE7G7bRzGSTXTr-RoVtubNbetOIFFAg==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:5d5c48fc03ba322613a7e8dbd044e7cba022789d8ef728fbaa04c9341d4193b8:Oxcwy1m8ubg-eCukqXWlfnXAh3k1d7g7WVCMTE3eHiTHalkrCKGFjppVrWkk1pvnVmCPgRuPpPh2ewvkpHzxCQ==:9fbfabe975fa5a7f -->
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
    `rye_execute(item_type="{input:item_type}", item_id="{input:item_id}", parameters={input:parameters}, dry_run={input:dry_run})`
  </step>

  <step name="return_result">
    Return the execution result to the caller. If dry_run was true, return the validation result instead.
  </step>
</process>
