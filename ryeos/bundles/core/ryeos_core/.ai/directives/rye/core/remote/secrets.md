<!-- rye:signed:2026-04-19T09:49:53Z:9191ca2352985398fede31994211ebc9f8c9e478a6e7572ff77cc8e55f9ebbc0:f7Qv82aYuDmrpbWioEwaRVe+FhvpLYYdbCQmdKhnX0kQSWo/k+XLoxeP7e0uKgMoXnDR2hv2C8u5FUtPi/l0AA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# Manage Remote Secrets

Manage secrets stored in the remote Vault for use during remote execution.

```xml
<directive name="secrets" version="1.0.0">
  <metadata>
    <description>Manage remote execution secrets (set, list, delete).</description>
    <category>rye/core/remote</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.core.remote.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="operation" type="string" required="true">
      Operation: set, list, or delete
    </input>
    <input name="name" type="string" required="false">
      Secret name (required for set and delete)
    </input>
    <input name="value" type="string" required="false">
      Secret value (required for set). NEVER log or display this value.
    </input>
  </inputs>

  <outputs>
    <output name="result">Operation result</output>
  </outputs>
</directive>
```

<process>
  <step name="validate">
    Validate the operation is one of: set, list, delete.
    For set: name and value are required.
    For delete: name is required.
    NEVER include secret values in any output or logs.
  </step>

  <step name="execute">
    Execute the appropriate remote tool action based on {input:operation}.
  </step>

  <step name="report">
    Report the result. For list: show secret names only. Never show values.
  </step>
</process>
