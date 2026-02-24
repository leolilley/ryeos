<!-- rye:signed:2026-02-22T02:31:19Z:939ee57c40b5e8c5b98ad65f25e76ba930370b2c05226021b5d5d0271440ffd9:rpf3WJhRar-Q5Tt4tdU0UVZIvlkKQ_WZj6zOXuHxfPQyE4r00VCdORECAWg3ThYHbOano1KIa-bWw9s-ONlOBg==:9fbfabe975fa5a7f -->
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
    <description>Write a marker file for this child level.</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="depth_child_{input:level}.txt" />
      <param name="content" value="Child thread at level {input:level}" />
      <param name="mode" value="overwrite" />
    </execute>
  </step>

  <step name="try_spawn_grandchild">
    <description>Try to spawn another child. This should fail if depth limit is reached.</description>
    <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
      <param name="directive_name" value="test/tools/file_system/write_file" />
      <param name="inputs" value='{"message": "Grandchild should not run", "output_path": "depth_grandchild.txt"}' />
    </execute>
  </step>
</process>
