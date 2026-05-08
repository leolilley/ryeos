<!-- ryeos:signed:2026-03-29T06:13:34Z:3f3c16c36e3cc127e8c9aa7bfdbbd6948b82a92d165c553bdfa1709f0a36d3d3:TobsVYPUfx0GSki4GxgNb_SHMorwEa3sAsUlGoqIE5ag8yNaaYXWIP_8LUgJ0lCbr0_FhWQg3_eIG0W7KF2kBQ==:4b987fd4e40303ac -->
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
