<!-- rye:signed:2026-03-11T07:13:35Z:abf7d7f9168b11d0e737d9613b2ef5cb7fee09c2cdc2f3a6375310426386eb7c:d6-TWAIwRhn-mdrHLRKCh0PZhVpidDlscnmQZo6zOa_dprDhWwEIeJTep5TIeVcVCkrdCP0Va1F2E_FQIkazBw==:4b987fd4e40303ac -->
# Depth Limit Test

Test that depth limit enforcement prevents infinitely recursive thread spawning. Sets depth=1 so: root (depth=1) → child (depth=0, can run but can't spawn) → grandchild (depth=-1, rejected).

```xml
<directive name="depth_limit_test" version="1.0.0">
  <metadata>
    <description>Test: spawn a child that tries to spawn a grandchild. With depth=1, the grandchild should be rejected (depth exhausted).</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" spend="0.50" depth="1" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye/agent/threads/thread_directive</tool></execute>
    </permissions>
  </metadata>

  <outputs>
    <success>Root spawned child at depth 1. Child's attempt to spawn grandchild at depth 2 should be rejected by depth limit.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_root_marker">
    Write "Root thread at depth 0" to `depth_root.txt`.
  </step>
  <step name="spawn_depth_child">
    Spawn child directive `test/limits/depth_child` with input level="1".
  </step>
</process>
