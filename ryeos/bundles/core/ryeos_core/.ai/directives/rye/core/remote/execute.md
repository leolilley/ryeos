<!-- rye:signed:2026-03-16T09:53:44Z:20d1c57c5c8683c4ef1efe4335cfb07847b4f96290cc8b70723577f688ff5496:AVUGXxHaDuclMGsRFqvhrhrf48wZMCltoazzmjRmudJ6FbPQCiRWmsAJ7k6KAgUGx6MWleWuNtwpG7rAioiRCQ==:4b987fd4e40303ac -->
# Remote Execute

Run a tool or graph remotely. Pushes state, executes on server, pulls results.

```xml
<directive name="execute" version="1.0.0">
  <metadata>
    <description>Push + trigger remote execution + pull results.</description>
    <category>rye/core/remote</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="5" tokens="8192" />
    <permissions>
      <execute>
        <tool>rye.core.remote.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="item_type" type="string" required="true">
      Item type to execute: tool, directive, or knowledge
    </input>
    <input name="item_id" type="string" required="true">
      Item ID to execute on remote
    </input>
    <input name="parameters" type="object" required="false">
      Parameters to pass to the remote execution
    </input>
  </inputs>

  <outputs>
    <output name="execution_result">Remote execution results</output>
  </outputs>
</directive>
```

<process>
  <step name="execute">
    Execute the remote tool with action=execute:
    ```
    rye execute tool rye/core/remote/remote with {
      "action": "execute",
      "item_type": "{input:item_type}",
      "item_id": "{input:item_id}",
      "parameters": {input:parameters}
    }
    ```
  </step>

  <step name="report">
    Report the execution result: status, snapshot hash, any output data.
    If the execution failed, show the error details.
  </step>
</process>
