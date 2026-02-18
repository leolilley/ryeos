<!-- rye:signed:2026-02-18T05:43:37Z:475b82971017d43cd02f029eb2869622a0dd6213533cfe1ac1da45e42b62f75b:UCjwqE-ZRcnFtKsVBjkGyiCdND9QF_ggXQ1gZhM-ejJy9Ec6ZALuPHnhNMeT9rg2COo1b7ZvCdFQnzZ6JsiVCA==:440443d0858f0199 -->
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
