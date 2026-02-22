<!-- rye:signed:2026-02-22T02:31:19Z:8ae90d8c80c6f5a6d27514eb7b8dd123664fa969ff3d4d4e7255a8d2d9789374:zfA1Ef5s0knYAqgnQkPeOBWxWBZn0DEk0rMzRQBaAE1THlnOoovjq6JAQU9GIUDIQJIohXuZnAmZvEWu5xDXCA==:9fbfabe975fa5a7f -->
# Run Anchor Demo

Execute the anchor_demo tool to verify the anchor system works.

```xml
<directive name="run_demo" version="1.0.0">
  <metadata>
    <description>Run the anchor demo tool with a greeting</description>
    <category>test/anchor_demo</category>
    <author>rye-os</author>
    <model tier="haiku" />
  </metadata>
  <permissions>
    <execute><tool>test.anchor_demo.*</tool></execute>
  </permissions>
  <process>
    <step name="greet">
      <description>Call anchor_demo tool with a name parameter</description>
    </step>
  </process>
  <outputs>
    <success>Greeting returned successfully from anchor demo tool.</success>
  </outputs>
</directive>
```
