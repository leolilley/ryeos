<!-- ryeos:signed:2026-03-11T07:13:35Z:8978e13e86ec2e3b4a89ce752ec6767d1f526e30f6059b5e94313d7525477f7b:bzgw1HEM0WQxoXcWvJjUdG_pJsHKQIhVPT8Rpjcddvhl2CjgUm0b_w9qobnvsFO6mg9SBTLcyz2sfZZqo9F_AQ==:4b987fd4e40303ac -->
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
    Write "Wildcard permission write" to `perm_test_wildcard.txt` — this should succeed with wildcard permissions.
  </step>
</process>
