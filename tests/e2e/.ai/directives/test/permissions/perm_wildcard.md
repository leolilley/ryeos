<!-- rye:signed:2026-02-22T02:31:19Z:475b82971017d43cd02f029eb2869622a0dd6213533cfe1ac1da45e42b62f75b:6QPrp489hsxzhq52O7H_tUC6aupR4zBL6Qj3elB_VIUEwPjQu-8dBYA6M-6s8_W1Xs5jV7aKch7Gednn_p8CBQ==:9fbfabe975fa5a7f -->
# Permission Test: Wildcard

Wildcard permissions — all actions should be allowed.

```xml
<directive name="perm_wildcard" version="1.0.0">
  <metadata>
    <description>Test: wildcard permissions — all actions should be allowed.</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="5" tokens="2048" />
    <permissions>*</permissions>
  </metadata>
  <outputs>
    <success>Write should succeed.</success>
  </outputs>
</directive>
```

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
