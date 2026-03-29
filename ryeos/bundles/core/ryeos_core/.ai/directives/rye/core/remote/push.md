<!-- rye:signed:2026-03-29T06:38:41Z:35784e158b341bb726f161cd3d48a5de4c55d2963823a6a1437f61319ad8891a:Zif3uAMLoomeUtZTbCSvgxCNfJwxeuv8K9TTsR_p5qkW2cZ8os8hvswI2PE3gbAS2uIsiJaffye4-VYE3oYGDg==:4b987fd4e40303ac -->
# Push to Remote

Sync local project and user state to the remote CAS server.

```xml
<directive name="push" version="1.0.0">
  <metadata>
    <description>Build manifests and sync missing objects to remote.</description>
    <category>rye/core/remote</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.core.remote.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="sync_result">Push results including objects synced count</output>
  </outputs>
</directive>
```

<process>
  <step name="push">
    Execute the remote tool with action=push:
    ```
    rye execute tool rye/core/remote/remote with {"action": "push"}
    ```
  </step>

  <step name="report">
    Report the sync result: how many objects were synced, manifest hashes.
  </step>
</process>
