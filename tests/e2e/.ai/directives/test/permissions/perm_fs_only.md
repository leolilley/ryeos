<!-- ryeos:signed:2026-03-11T07:13:35Z:902c883794a4746e8cfb210d3445cf45a719e0863b797fe622f6940e0599a302:huDTDrA-QAffPNTaFc2c4SZVWf081bvkT5GfmomUieHL7p7tF9uAW9D980GFR5hyUA_1aY4C16N39oO6LLCuDA==:4b987fd4e40303ac -->
# Permission Test: FS Only

Has file-system execute permission only. Write should succeed, search should be denied.

```xml
<directive name="perm_fs_only" version="1.0.0">
  <metadata>
    <description>Test: has fs write permission, then tries search (should be denied).</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="5" tokens="2048" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>
  <outputs>
    <success>First call should succeed, second should show permission denied.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_allowed">
    Write "Permission allowed write" to `perm_test_allowed.txt` — this should succeed.
  </step>
  <step name="search_denied">
    Search for knowledge entries matching "test" — this should be denied by permissions.
  </step>
</process>
