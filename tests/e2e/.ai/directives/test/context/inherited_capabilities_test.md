<!-- ryeos:signed:2026-03-29T06:13:34Z:1a0f0f5431473e1b6d8f158fcecb7d48d949ae2a48105ae88ff280f55266b9bc:e9XZDHCB8N0RoqzBWF54Bs5hLtp8vERtpkgKaMiy7yXBpkHsEYxMuvkxm1p1YZ8EjWd_8E7GX9RpY8bygBXeDg==:4b987fd4e40303ac -->
# Inherited Capabilities Test

Tests that capabilities from an extended directive are inherited into the leaf's
`<capabilities>` XML. This directive has NO own permissions — it relies entirely
on inheriting all 3 capability types (execute, fetch, sign) from
broad_capabilities_base via the extends chain.

Expected `<capabilities>` output:
- All 6 rye/file-system/* tool schemas
- rye/primary/rye_execute, rye_fetch, rye_sign schemas
- No rye/bash/bash

```xml
<directive name="inherited_capabilities_test" version="1.0.0" extends="test/context/broad_capabilities_base">
  <metadata>
    <description>Tests capability inheritance — leaf has no permissions, inherits all 3 types from parent.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="8" tokens="32000" spend="0.15" />
  </metadata>

  <outputs>
    <result>Report confirming all tools were called successfully</result>
  </outputs>
</directive>
```

<process>
  <step name="call_all_tools">
    <description>You MUST call every tool listed in your capabilities block. For each tool, make a real call:
1. rye/file-system/read — read file "README.md"
2. rye/file-system/ls — list the project root directory
3. rye/file-system/grep — search for "MARKER" in the project
4. rye/file-system/glob — find all .md files
5. rye/file-system/write — write the results summary to outputs/inherited_caps_all_tools.txt
6. rye/primary/rye_fetch — search for directives with query "*" scope "directive"
7. rye/primary/rye_fetch — load knowledge item "test/context/base-identity"

After all calls, write a final summary confirming which tools succeeded and which failed.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/inherited_caps_all_tools.txt" />
      <param name="content" value="Summary of all tool calls" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
