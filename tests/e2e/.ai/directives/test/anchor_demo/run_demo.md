<!-- rye:signed:2026-03-11T07:13:35Z:b2bf3f7541cfb0c40497888720c1d0b6c44f819f518c4eba93b16bbe82355a4a:9ZgvFAo7mS4l5kLRbFjKVh_EU1aohHRWAaIoBXp8Wr2qqFFD6qoKNzpg8p6zSDCXUiOvIPeVxeaIJJqv4N-kAg==:4b987fd4e40303ac -->
# Run Anchor Demo

Execute the anchor_demo tool to verify the anchor system works.

```xml
<directive name="run_demo" version="1.0.0">
  <metadata>
    <description>Run the anchor demo tool with a greeting</description>
    <category>test/anchor_demo</category>
    <author>rye-os</author>
    <model tier="fast" />
  </metadata>
  <permissions>
    <execute><tool>test.anchor_demo.*</tool></execute>
  </permissions>
  <outputs>
    <success>Greeting returned successfully from anchor demo tool.</success>
  </outputs>
</directive>
```

<process>
  <step name="greet">
    <description>Call anchor_demo tool with a name parameter</description>
  </step>
</process>
