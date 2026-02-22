<!-- rye:signed:2026-02-22T02:31:19Z:0704c2b447e8ebaa944c0a2285dfa33819d1ccf688e0babb3246d4d0e52d30c5:YeDus6ZM55eMfTIvsHaejxPOZmt3K3nMMdLDRoQsCBc-vcr1-JrSpbIFRFNefTFPpomIvpRnhkbpgk4iudI8DA==:9fbfabe975fa5a7f -->
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
