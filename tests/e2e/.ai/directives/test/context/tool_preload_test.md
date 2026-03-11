<!-- rye:signed:2026-03-11T07:13:35Z:24b001c419370159335af4ce92de0f49a28d1e7543a19ed76f78a80a1a73b354:WVfLuWBxOR77pK6xjSjTYIDhe4yyuOAe_R36chJ9QF8haxnbdS95BN8YjbJRgQD7hqBxa7e2HX9HdTBjMAYxBA==:4b987fd4e40303ac -->
# Tool Schema Preload Test

Tests Layer 1 — permissions-driven tool schema preload. This directive grants
specific file-system tool permissions. The thread should see the CONFIG_SCHEMA
for rye/file-system/read and rye/file-system/grep preloaded in the before-context,
but should NOT see schemas for rye/bash/bash (not permitted).

```xml
<directive name="tool_preload_test" version="1.0.0">
  <metadata>
    <description>Tests tool schema preload — only permitted tool schemas should appear.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <permissions>
      <execute>
        <tool>rye.file-system.read</tool>
        <tool>rye.file-system.grep</tool>
        <tool>rye.file-system.write</tool>
      </execute>
    </permissions>
  </metadata>

  <outputs>
    <result>Report of which tool schemas were preloaded in context</result>
  </outputs>
</directive>
```

<process>
  <step name="report_schemas">
    <description>Look at the tool schemas that were injected into your context. Report which tool item_ids have schemas visible (e.g. rye/file-system/read, rye/file-system/grep, rye/file-system/write). Confirm that rye/bash/bash is NOT present. Write the result to a file.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/context_tool_preload_result.txt" />
      <param name="content" value="Report which tool schemas are visible" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
