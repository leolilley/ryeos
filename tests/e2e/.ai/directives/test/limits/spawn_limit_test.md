<!-- rye:signed:2026-02-13T08:38:46Z:b562817b23b7f5214f3907fdfd372ae699d1d87534f41dfce863a1eb5ff38fa7:1JW_nhgVpd2RLzhr0eiegh4oJocqGAH4qObJSs6_nU6iJliiF26U2Zc_yGt57V5YugP3w7u-UUNvFtMsO0mlDw==:440443d0858f0199 -->
# Spawn Limit Test

Test that spawn limit enforcement works. This directive has a low spawns limit.

```xml
<directive name="spawn_limit_test" version="1.0.0">
  <metadata>
    <description>Test: verify spawn limit enforcement prevents excessive child threads.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="3" tokens="4096" spend="0.10" spawns="2" />
  </metadata>
  <permissions>
    <execute><tool>rye.file-system.*</tool></execute>
    <execute><tool>rye.primary-tools.*</tool></execute>
  </permissions>
  <process>
    <step name="report">
      <description>Report the spawn limit configuration.</description>
    </step>
  </process>
  <outputs>
    <success>Directive should complete. Spawn limit is tested programmatically.</success>
  </outputs>
</directive>
```
