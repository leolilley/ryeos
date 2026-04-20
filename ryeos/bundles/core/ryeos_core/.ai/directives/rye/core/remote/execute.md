<!-- rye:signed:2026-04-19T09:49:53Z:9c1445af59dfb16b3a0bc8f3c721e2c9b41eb5092661181d1a679501361c3023:OnpiUQRMe0Vd/91jqFtoo9BGUwxJXjn5j6KKyrmOjHa64tmdVjyOEBhd+D3kjqtlUJex+UZyOoQiRs1kqR3FCA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
