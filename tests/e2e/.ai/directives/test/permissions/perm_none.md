<!-- rye:signed:2026-02-13T07:45:52Z:3dbd293a1338a161a348123df447c1afe43d72093fb14b0a030867fc1b5322ba:qu5IJMXNyotwKPYgNf5kIOZIIkYKjFcH7BXGmQN-B9Dz5POFhW4-XJO9pGkfgB_lgu7q6CKs5XTDox1SOMymDQ==:440443d0858f0199 -->
# Permission Test: No Permissions

No permissions block declared. All tool calls should be denied (fail-closed).

```xml
<directive name="perm_none" version="1.0.0">
  <metadata>
    <description>Test: no permissions declared — all tool calls should be denied.</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="3" tokens="1024" />
  </metadata>
  <process>
    <step name="write_denied">
      <description>Write a test file — should be denied (no permissions).</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="perm_test_none.txt" />
        <param name="content" value="Should never appear" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>
  </process>
  <outputs>
    <success>Tool call should be denied.</success>
  </outputs>
</directive>
```
