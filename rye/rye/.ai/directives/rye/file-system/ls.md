<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->
# List Directory

List files and directories at a given path.

```xml
<directive name="ls" version="1.0.0">
  <metadata>
    <description>List files and directories at a given path, defaulting to the project root.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
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
    `rye_execute(item_type="tool", item_id="rye/file-system/ls", parameters={"path": "{input:path}"})`
  </step>

  <step name="return_result">
    Return the list of files and directories.
  </step>
</process>
