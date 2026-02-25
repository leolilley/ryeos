<!-- rye:signed:2026-02-23T02:07:54Z:ab5702ff15cbe3d0fc0580877015354630925502ec1225328b6fe00f32601d5b:dMrxeIk7Ao3Pgs1h6giwD_-ngdXH86oFIzM7uJlVtA8LuJaif5kPS9GqogbDRYZrI5wBaGa7sqw3B9ekPhRUDA==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->
# Web Browser

Control a browser via playwright-cli — open pages, take screenshots, interact with elements, manage sessions.

```xml
<directive name="browser" version="1.0.0">
  <metadata>
    <description>Control a browser via playwright-cli — open pages, take screenshots, interact with elements, manage sessions.</description>
    <category>rye/web</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="6" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.web.browser.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="command" type="string" required="true">playwright-cli command — open, goto, screenshot, snapshot, click, fill, type, select, hover, resize, console, network, eval, press, tab-list, tab-new, tab-select, tab-close, close, close-all</input>
    <input name="args" type="array" required="false">Positional arguments (URL for open/goto, element ref for click, etc.)</input>
    <input name="flags" type="object" required="false">Named flags (e.g. headed: true, filename: 'page.png')</input>
    <input name="session" type="string" required="false">Named session for browser isolation (default: rye)</input>
    <input name="timeout" type="integer" required="false">Command timeout in seconds (default: 30)</input>
  </inputs>

  <outputs>
    <output name="result">Command output including any screenshot/snapshot paths</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:command} is non-empty. If not, halt with an error.
  </step>

  <step name="execute_command">
    Call the browser tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/web/browser/browser", parameters={"command": "{input:command}", "args": "{input:args}", "flags": "{input:flags}", "session": "{input:session}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_result">
    Return the command output as {output:result}.
  </step>
</process>
