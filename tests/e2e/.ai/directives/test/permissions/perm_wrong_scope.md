<!-- rye:signed:2026-02-13T07:45:52Z:d4dfc7cb985794019f71de3e0d9b00c28a57cb84708068738ea4d6a82a307f43:_qOdUtrrALkWufD23g9Cn3fnkhIvDrNPTPp8mlcLrfXI14xMt1T5yJZPwfxpNp6TCiDKCrv94gfnczastS3yAg==:440443d0858f0199 -->
# Permission Test: Wrong Scope

Has permission for rye.core.* tools but tries to use rye/file-system — should be denied.

```xml
<directive name="perm_wrong_scope" version="1.0.0">
  <metadata>
    <description>Test: has core tool permission, tries file-system tool (should be denied).</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="3" tokens="1024" />
    <permissions>
      <execute><tool>rye.core.*</tool></execute>
    </permissions>
  </metadata>
  <process>
    <step name="write_denied">
      <description>Write a file — should be denied (wrong permission scope).</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="perm_test_wrong.txt" />
        <param name="content" value="Should never appear" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>
  </process>
  <outputs>
    <success>Tool call should show permission denied — wrong scope.</success>
  </outputs>
</directive>
```
