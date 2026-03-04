<!-- rye:signed:2026-03-04T01:47:19Z:cbcf6ea7516a4d4f18e091f8f74bcea34baf9ed75c1a6ab1c1f1e8683e2701b3:4P3cIL47wLekeheKaPi0sbHKcxilrdXEakd_LGGqbAupWHP7s_tGva-e7QXy7kgdHrX2CoRFRwOSHont0OaiBA==:4b987fd4e40303ac -->
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
    <limits turns="8" tokens="32000" spend="0.15" />
  </metadata>

  <outputs>
    <result>Report confirming which tools were called</result>
  </outputs>
</directive>
```

<process>
  <step name="call_tools">
    <description>Call every tool in your capabilities block. List the project root, glob for *.md files, grep for "MARKER" in .ai/, read the .gitignore file, write a summary to outputs/inherited_caps_minimal.txt, and use rye_search and rye_load at least once each.</description>
  </step>
</process>
