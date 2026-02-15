<!-- rye:signed:2026-02-13T07:45:52Z:fba861efe63051f9a21d813065681d57b03d923de2c3323f1acb9a23bcc6e6c9:pgN875bkHxMojYpTse8MXEOuV1cIZUpmHPyGCTdPTgeZhXhVi5hlqnu5FSW_teVlJ_P1SDnH9Gpllqh9n90iAg==:440443d0858f0199 -->
# Permission Test: Wildcard

Wildcard permissions — all actions should be allowed.

```xml
<directive name="perm_wildcard" version="1.0.0">
  <metadata>
    <description>Test: wildcard permissions — all actions should be allowed.</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="5" tokens="2048" />
    <permissions>*</permissions>
  </metadata>
  <process>
    <step name="write_allowed">
      <description>Write a test file — should succeed with wildcard.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="perm_test_wildcard.txt" />
        <param name="content" value="Wildcard permission write" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>
  </process>
  <outputs>
    <success>Write should succeed.</success>
  </outputs>
</directive>
```
