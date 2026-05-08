<!-- ryeos:signed:2026-03-11T07:13:35Z:c231c5b231bbda5e6a5818e772f186ffa39ab7fcc25ccda84f1dd2c34a430907:OPfRfuqZ8rfNaFcT01PpomBsgSoLqMcH8eyfkh1q9UHT3YjQ9RvQqi8ierl3nSVJKy3kqwUklJmd3TVc_vxSAg==:4b987fd4e40303ac -->
# Hook-Routed Test

Tests resolve_extends hook routing (Layer 2). This directive has NO explicit `extends`.
A project hook catches directives in category "test/hookroute" and routes them
into test/context/hook_routed_base, which injects base-identity (system) and
hook-routed-rules (before). The thread should see both HOOK_ROUTED_RULES_PRESENT
and BASE_IDENTITY_PRESENT markers.

```xml
<directive name="hook_routed_test" version="1.0.0">
  <metadata>
    <description>Tests resolve_extends hook routing — no explicit extends, routed by hook.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <fetch>*</fetch>
    </permissions>
  </metadata>

  <outputs>
    <result>Confirmation that hook-routed context markers are visible</result>
  </outputs>
</directive>
```

<process>
  <step name="report_context">
    <description>Report which context markers you can see. Check for BASE_IDENTITY_PRESENT and HOOK_ROUTED_RULES_PRESENT. Write the result.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/context_hook_routed_result.txt" />
      <param name="content" value="Report which markers are visible" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
