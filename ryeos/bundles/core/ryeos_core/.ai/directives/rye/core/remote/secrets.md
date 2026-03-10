<!-- rye:signed:2026-03-09T23:48:24Z:552af36c25d246a9a3180648eab7115a69b36f3d1cfbac7a318f0332735e3b7b:mfMhACtM70iCOpflVCNDLuOE-WpjPljpsgPV39-hihRKw_IDnwSOyYTcoH3spP8VOTUoA4Moi6c5G6EAA6JBBQ==:4b987fd4e40303ac -->
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
