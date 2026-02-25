<!-- rye:signed:2026-02-25T07:50:41Z:f546b5f6421d47d19963c045c036f98a43070b2b55ca349751074feb7b8ed4f3:0jGl5w-Hdwataj1OaDneoMmC0vI4oHmOLirk5eTTucB8qYiDeCJZZexl4xpaQc_Z5hijUm7-VbDrm_2CfFygBg==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-23T02:07:54Z:40579d9448525d88665ab3a9daf8c9538899b3803f8905ac1c8522b6266e16fd:G5RynRkzitB5GN4GB7A9tvsUqrcAfs7KXFym8L6fxrnW1gcjHmS0zvR4Uj51RCMFaKl9qra_S82KKLg8Rg5PBw==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->
# LSP Query

Query language servers — go to definition, find references, hover info, document symbols, and more.

```xml
<directive name="lsp" version="1.0.0">
  <metadata>
    <description>Query language servers — go to definition, find references, hover info, document symbols, and more.</description>
    <category>rye/code</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.code.lsp.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="operation" type="string" required="true">LSP operation: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls</input>
    <input name="file_path" type="string" required="true">Path to the file</input>
    <input name="line" type="integer" required="true">Line number (1-based)</input>
    <input name="character" type="integer" required="true">Character offset (1-based)</input>
    <input name="timeout" type="integer" required="false">Timeout in seconds (default: 15)</input>
  </inputs>

  <outputs>
    <output name="result">LSP operation results (locations, symbols, hover info, etc.)</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:operation}, {input:file_path}, {input:line}, and {input:character} are all provided.
  </step>

  <step name="run_lsp">
    Call the LSP tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/code/lsp/lsp", parameters={"operation": "{input:operation}", "file_path": "{input:file_path}", "line": "{input:line}", "character": "{input:character}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_result">
    Return output as {output:result}.
  </step>
</process>
