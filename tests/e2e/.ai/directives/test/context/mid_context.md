<!-- rye:signed:2026-02-24T23:52:30Z:3f3c16c36e3cc127e8c9aa7bfdbbd6948b82a92d165c553bdfa1709f0a36d3d3:fd0CypgHRqlckaALp3jXoaUqvJ2X2kvv_7ay51W4IpPPD0BHMDi1fYsy39Et5WIKDrlLSHDU73F2lvtB-R7QBw==:9fbfabe975fa5a7f -->
# Mid Context Directive

Middle layer in extends chain. Adds before-context on top of base's system context.

```xml
<directive name="mid_context" version="1.0.0" extends="test/context/base_context">
  <metadata>
    <description>Mid-layer directive extending base. Adds before-context knowledge.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <context>
      <before>test/context/mid-rules</before>
    </context>
  </metadata>

  <outputs>
    <result>Confirmation that mid-layer executed with inherited + own context</result>
  </outputs>
</directive>
```

<process>
  <step name="write_marker">
    <description>Write a marker file confirming this directive ran.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/context_mid_marker.txt" />
      <param name="content" value="mid_context executed" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
