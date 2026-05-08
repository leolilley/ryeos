<!-- ryeos:signed:2026-03-11T07:13:35Z:7b1c5987ecac0e27b09fded7e0911d0a742823b510e442fc522c40172ee30259:40K6C_-CW358DSsYA-GjHS0OZe5vq2EL1qBtnLfWyQTlUqyPAY3kXRju61oUsRF0FhsS3FHxl6YqI-vv0qm7CA==:4b987fd4e40303ac -->
# Permission Test: No Permissions

No permissions block declared. All tool calls should be denied (fail-closed).

```xml
<directive name="perm_none" version="1.0.0">
  <metadata>
    <description>Test: no permissions declared — all tool calls should be denied.</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="1024" />
  </metadata>
  <outputs>
    <success>Tool call should be denied.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_denied">
    Write "Should never appear" to `perm_test_none.txt` — this should be denied since no permissions are declared.
  </step>
</process>
