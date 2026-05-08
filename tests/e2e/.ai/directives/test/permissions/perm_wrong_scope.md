<!-- ryeos:signed:2026-03-11T07:13:35Z:0f0536204d7ba1d8c45a894f6dae3bc3479440908f2f8a25eef1d279cedd9471:gOzcJlFlq_IWZ0YABH4EtQuXVqKvXBz7Ze1wozJBFZP8w3xorls1YZzu4ywI5xkWtneFaEHoLRsVX3wIbE5tDg==:4b987fd4e40303ac -->
# Permission Test: Wrong Scope

Has permission for rye.core.* tools but tries to use rye/file-system — should be denied.

```xml
<directive name="perm_wrong_scope" version="1.0.0">
  <metadata>
    <description>Test: has core tool permission, tries file-system tool (should be denied).</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="1024" />
    <permissions>
      <execute><tool>rye.core.*</tool></execute>
    </permissions>
  </metadata>
  <outputs>
    <success>Tool call should show permission denied — wrong scope.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_denied">
    Write "Should never appear" to `perm_test_wrong.txt` — this should be denied since only rye.core.* permission is granted, not rye.file-system.*.
  </step>
</process>
