<!-- rye:signed:2026-02-24T23:52:30Z:e6e0685096459585d32f84f57281a068defa9e244a1c6f871301973892b693bc:elDevYacRJo0T1XAw9sQCo0NDP6BsSbVas29G7ki9A7SQ46u7z-hoTX4JMheIcntULVn6_r52FxGWvd_kWZZDg==:9fbfabe975fa5a7f -->
# Base Context Directive

Root of an extends chain. Declares system context that should propagate to all children.

```xml
<directive name="base_context" version="1.0.0">
  <metadata>
    <description>Base directive that injects system-level context via the extends chain.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <context>
      <system>test/context/base-identity</system>
    </context>
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <search>*</search>
      <load>*</load>
    </permissions>
  </metadata>

  <outputs>
    <result>Confirmation that the directive executed with context</result>
  </outputs>
</directive>
```

<process>
  <step name="write_marker">
    <description>Write a marker file confirming this directive ran.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/context_base_marker.txt" />
      <param name="content" value="base_context executed" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
