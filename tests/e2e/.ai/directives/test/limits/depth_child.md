<!-- rye:signed:2026-03-11T07:13:35Z:0eeb5687c87db7661d9464de6a0a02b200607b1fa8674dc75d3d38d17c22a838:OrfDnKTQnA1R-GLPiie1PxwM4poKZ0zcoenX3QHFmyPuPHvFSa_WVuOQmKfR-0DdA8HEqTxNS_84-xzQX2LdAQ==:4b987fd4e40303ac -->
# Depth Child

Child directive spawned by depth_limit_test. Writes a marker then tries to spawn its own child — which should fail if depth limit is reached.

```xml
<directive name="depth_child" version="1.0.0">
  <metadata>
    <description>Intermediate child: writes marker, then tries to spawn a grandchild. Grandchild should be rejected if depth limit reached.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" spend="0.25" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye/agent/threads/thread_directive</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="level" type="string" required="true">
      Current nesting level for logging.
    </input>
  </inputs>

  <outputs>
    <success>Child wrote marker. Grandchild spawn attempt result shows whether depth limit enforced.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_child_marker">
    Write "Child thread at level {input:level}" to `depth_child_{input:level}.txt`.
  </step>
  <step name="try_spawn_grandchild">
    Try to spawn child directive `test/tools/file_system/write_file` with message "Grandchild should not run" and output_path "depth_grandchild.txt". This should fail if depth limit is reached.
  </step>
</process>
