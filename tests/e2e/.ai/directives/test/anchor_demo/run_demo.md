<!-- rye:signed:2026-02-18T05:43:37Z:8ae90d8c80c6f5a6d27514eb7b8dd123664fa969ff3d4d4e7255a8d2d9789374:K_AaDo7Y0GW_P0cmswPIfzqRh1uM7F3iwF9BAGlP05nJ8JNde1zNLTSqmLPtYnYbjKElNu0RNxV_bUB1huNbCw==:440443d0858f0199 -->
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
