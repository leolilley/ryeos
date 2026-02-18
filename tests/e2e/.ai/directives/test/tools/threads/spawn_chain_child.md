<!-- rye:signed:2026-02-18T05:43:37Z:f37ca3767a19eee2ca5cdfdad310e9e85d3d5af124d50b9dbc9cb585b97b8605:ozCV0zzLB42aNRI3PSbw5Qg4dCuHZ2okOp4PKf0N-_8u3istizyJtJ2jfRvKHDJ9CiIcmeSnU4JhYvvpUoS8DA==:440443d0858f0199 -->
# Spawn Chain Child

Recursive child for spawn chain test. Writes a marker at its level, then spawns the next level if not at max.

```xml
<directive name="spawn_chain_child" version="1.0.0">
  <metadata>
    <description>Chain child: writes marker at current level, spawns next level if below max_level.</description>
    <category>test/tools/threads</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="6" tokens="4096" spend="0.30" />
    <permissions>
      <cap>rye.execute.tool.*</cap>
      <cap>rye.search.*</cap>
    </permissions>
  </metadata>

  <inputs>
    <input name="level" type="string" required="true">
      Current chain level (1, 2, 3, ...).
    </input>
    <input name="max_level" type="string" required="true">
      Maximum level to spawn to.
    </input>
  </inputs>

  <process>
    <step name="write_level_marker">
      <description>Write marker file for this level.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="chain_L{input:level}.txt" />
        <param name="content" value="Level {input:level} — child thread" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>

    <step name="maybe_spawn_next">
      <description>If level &lt; max_level, spawn the next level child. Pass level+1 and max_level through. If level >= max_level, just report completion.</description>
      <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
        <param name="directive_name" value="test/tools/threads/spawn_chain_child" />
        <param name="inputs" value='{"level": "NEXT_LEVEL", "max_level": "{input:max_level}"}' />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Level {input:level} marker written. Next level spawned if below max.</success>
  </outputs>
</directive>
```
