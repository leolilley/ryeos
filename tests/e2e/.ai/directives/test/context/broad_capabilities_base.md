<!-- rye:signed:2026-03-11T07:13:35Z:9bd37d2f834176fcff11619ce8334821483c127db214253f5c96f43968f85393:Mgs7v5OsD4kAF3_D9cigZ741LBqkauRz--0VNvuq4xQIYg6gc9-4uJCtczPa17nDmZOZ0zli8s-sdlAWMKCSCg==:4b987fd4e40303ac -->
# Broad Capabilities Base

Context base directive with all 3 capability types: execute, fetch, sign.
Used to test that extended directives inherit the full `<capabilities>` XML.

```xml
<directive name="broad_capabilities_base" version="1.0.0">
  <metadata>
    <description>Base with all 3 capability types for inheritance testing.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" spend="0.10" />
    <context>
      <system>test/context/base-identity</system>
    </context>
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <fetch>*</fetch>
      <sign>*</sign>
    </permissions>
  </metadata>
</directive>
```
