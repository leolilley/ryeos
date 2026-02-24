<!-- rye:signed:2026-02-22T02:31:19Z:5691296c5e0feade2915b6b4bb3e66da20492b08cdfb99af15147024502db2c9:3cYvLmYl0jJtlK--ObcDLvMbfo7BMHap1P2vWXrql7_GZGJXwX9PfCM0MF9wzXKagDTPxzvh2xEealUjIvryDQ==:9fbfabe975fa5a7f -->
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
    <description>Write a marker file to confirm root thread ran.</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="depth_root.txt" />
      <param name="content" value="Root thread at depth 0" />
      <param name="mode" value="overwrite" />
    </execute>
  </step>

  <step name="spawn_depth_child">
    <description>Spawn test/limits/depth_child which will try to spawn its own child. Pass parent_depth=0 so child is depth 1.</description>
    <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
      <param name="directive_name" value="test/limits/depth_child" />
      <param name="inputs" value='{"level": "1"}' />
    </execute>
  </step>
</process>
