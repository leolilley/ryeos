<!-- rye:signed:2026-04-19T09:49:53Z:e846e10d831140dbc4a32a772ebe0d1e9ca6351c355e3b7a30c09ab97a2e4caa:2t0oDP+Lp9mSmC4iBEMyi9mcY08VVYB7AAi1aFWRP4yVtC0gU6831vtavskP3k9oYM8QWsV9T3zy2JI8UhLdCA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# List Directory

List files and directories at a given path.

```xml
<directive name="ls" version="1.0.0">
  <metadata>
    <description>List files and directories at a given path, defaulting to the project root.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.file-system.ls</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="path" type="string" required="false">
      Directory path to list (default: project root)
    </input>
  </inputs>

  <outputs>
    <output name="entries">List of files and directories at the given path</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Default {input:path} to the project root if not provided.
  </step>

  <step name="call_ls">
    List the directory:
    `rye_execute(item_id="rye/file-system/ls", parameters={"path": "{input:path}"})`
  </step>

  <step name="return_result">
    Return the list of files and directories.
  </step>
</process>
