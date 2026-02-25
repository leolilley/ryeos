<!-- rye:signed:2026-02-25T07:50:41Z:a209ef5bac7841ea5c061a6c5202c09389dd2cfee37dd487674a2ee67c834c82:_e7yNYdagThzFEEjfZMnU_s3hE4rSeaxidiQaA4JvdxF3blSib5ciXpGYRggDybEwhLiOls7UfNCAU_rMubBCA==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:c57a3ae6f4b6e1cbade0ad6ce872c8646b778e110001aa47a454e97c1a8ea419:x5dHSbYurduLAsQszaH7Rn3giPPbC7gK08WwvoCCfGQyPj9VgXsIszIX40h02LUAR5FzvFdoCMoCB2BDJbE7Ag==:9fbfabe975fa5a7f -->
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
    `rye_execute(item_type="tool", item_id="rye/file-system/ls", parameters={"path": "{input:path}"})`
  </step>

  <step name="return_result">
    Return the list of files and directories.
  </step>
</process>
