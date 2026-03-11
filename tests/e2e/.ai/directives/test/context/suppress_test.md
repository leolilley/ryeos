<!-- rye:signed:2026-03-11T07:13:35Z:35f42e186e72ce1d2f75bb9cd3caa88507a7056a02198af1f56e0ece3ab38e45:0uwfhN_hZNhivNdQcq5OxT2_5arns3Va8W7vl8le4amtKDioVxgY-db5u-s0THC-qCbNVvvG9UXQ_HF_px7eBg==:4b987fd4e40303ac -->
# Suppress Context Test

Tests the `<suppress>` tag. Extends base_context (which injects base-identity via system context),
then suppresses base-identity and replaces it with alt-identity. The thread should see
ALT_IDENTITY_PRESENT but NOT BASE_IDENTITY_PRESENT in the system prompt.

```xml
<directive name="suppress_test" version="1.0.0" extends="test/context/base_context">
  <metadata>
    <description>Tests suppress: replaces base-identity with alt-identity.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <context>
      <suppress>test/context/base-identity</suppress>
      <system>test/context/alt-identity</system>
    </context>
  </metadata>

  <outputs>
    <result>Confirmation of which identity markers are visible</result>
  </outputs>
</directive>
```

<process>
  <step name="report_context">
    <description>Report which identity markers you can see in your system context. Check for BASE_IDENTITY_PRESENT and ALT_IDENTITY_PRESENT. Write the result.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/context_suppress_result.txt" />
      <param name="content" value="Report which markers are visible" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
