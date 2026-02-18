<!-- rye:signed:2026-02-18T05:43:37Z:8671a0e8b4409c78f79b455c6df844e58ef8ffee5d1baaf1423b5fa63f9f1d9c:Fajxuml3cNoXdEtXzM4mAdvRukSd6MaYaoHl_6GmGn4MjxcYI7oW6Z_aDcmMyAAmxEbm-qtyQOZGvdH_CiBbBA==:440443d0858f0199 -->
# Permission Test: FS Only

Has file-system execute permission only. Write should succeed, search should be denied.

```xml
<directive name="perm_fs_only" version="1.0.0">
  <metadata>
    <description>Test: has fs write permission, then tries search (should be denied).</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="5" tokens="2048" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>
  <process>
    <step name="write_allowed">
      <description>Write a test file — this should succeed.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="perm_test_allowed.txt" />
        <param name="content" value="Permission allowed write" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>
    <step name="search_denied">
      <description>Search for knowledge — this should be denied by permissions.</description>
      <search item_type="knowledge" query="test">
      </search>
    </step>
  </process>
  <outputs>
    <success>First call should succeed, second should show permission denied.</success>
  </outputs>
</directive>
```
