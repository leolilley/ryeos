<!-- rye:signed:2026-04-19T09:49:53Z:d02df143cdcf8376876fdf2754c059887b4ba25dfe9ac25c9f08ca0f7b8cd243:mRFqbKA3nbk7b3nGHW56spdF9U7P+4VUeXDtZqIN45PDtJX9e2b34IflnpfFPelggyzCnauVtkXGtCdXyec9AQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
