<!-- rye:signed:2026-02-13T07:45:52Z:793789861595b1bc49b6316e38f8a32170bf211ee237b9c95a3d3b9ab42caf13:ZTQc-g2rnWT4SZbmg25kLdAMo490h1JLngqBemMESIm6tT3bBS9qJhG8LkZrtCp3Mb0O_wDM6M7TTiy2NciyDQ==:440443d0858f0199 -->
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
