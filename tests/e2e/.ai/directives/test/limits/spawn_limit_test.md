<!-- rye:signed:2026-03-11T07:13:35Z:057faa4ca7a400ff4e681adb8d96014b0f798e22c190380455c555b91f8b648f:NGIYGqE5UvD_FOOe-y4EKBAUVtvIkmdyWUMBOvz8FlYmbaPtR3J-eqedjtNpn9LEJOwnZVWmQo8BFqaNdfjhAQ==:4b987fd4e40303ac -->
# Spawn Limit Test

Test that spawn limit enforcement works. This directive has a low spawns limit.

```xml
<directive name="spawn_limit_test" version="1.0.0">
  <metadata>
    <description>Test: verify spawn limit enforcement prevents excessive child threads.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" spend="0.10" spawns="2" />
  </metadata>
  <permissions>
    <execute><tool>rye.file-system.*</tool></execute>
    <execute><tool>rye.primary-tools.*</tool></execute>
  </permissions>
  <outputs>
    <success>Directive should complete. Spawn limit is tested programmatically.</success>
  </outputs>
</directive>
```

<process>
  <step name="report">
    <description>Report the spawn limit configuration.</description>
  </step>
</process>
