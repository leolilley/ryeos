<!-- rye:signed:2026-03-04T03:35:33Z:9dd9b8d29872ceb33f3c84d559695017c0a655894e65adcfe14d18dbc3649810:JdDbylu808dH8KlirZ37gKs_JZ3KtOQTYsJ28KZ-GrylCGxfGaORmgzBXtK_nwXu_yRGgT-MQi6KfqAxWfizCw==:4b987fd4e40303ac -->
# Inherited Capabilities Minimal Test

Minimal-guidance version of inherited_capabilities_test. The LLM must
figure out tool names and parameters from the `<capabilities>` block alone.

```xml
<directive name="inherited_capabilities_minimal" version="1.0.0" extends="test/context/broad_capabilities_base">
  <metadata>
    <description>Minimal guidance — LLM must infer tool usage from capabilities block only.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="12" tokens="32000" spend="0.15" />
  </metadata>

  <outputs>
    <output name="result" type="string" required="true">Report confirming which tools were called</output>
    <output name="tools_used" type="string" required="true">Comma-separated list of tool names that were called</output>
  </outputs>
</directive>
```

<process>
  <step name="call_tools">
    <description>Call every tool in your capabilities block. List the project root, glob for *.md files, grep for "MARKER" in .ai/, read the .gitignore file, write a summary to outputs/inherited_caps_minimal.txt, and use rye_search and rye_load at least once each.</description>
  </step>
</process>
