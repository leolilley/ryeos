<!-- rye:signed:2026-02-20T01:09:07Z:8731bc830f8d3bcca80fb348eff029fac4babc8eb69bb5bc3c18ccfb7d0fe825:NyJ2cd5cP5uTVVda9NU3JY_HInpgOj4Gv6kG2yCJvRxmTPKk28RR5S4wdo-76tQhHWz6iZrKG8sJyYUu_2ORCQ==:440443d0858f0199 -->
# Registry Whoami

Show the currently authenticated user.

```xml
<directive name="whoami" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=whoami to show the authenticated user.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.registry.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="user">Authenticated user details</output>
  </outputs>
</directive>
```

<process>
  <step name="call_registry_whoami">
    Call the registry tool with action=whoami.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "whoami"})`
  </step>

  <step name="return_result">
    Return the user details to the user.
  </step>
</process>
