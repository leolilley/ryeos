<!-- rye:signed:2026-03-11T07:13:35Z:c6c19f29be39554fcb1ac78dbc517728dc6ae5b5f3808e060cfd157d66621d89:gWqv7uJBN4fUz22zVCrsconx3vZe0QtkORJYgkyktEMwEw0REij5IQ3J_50dLHXnl--TyxPAAYQnu55vxA_3AA==:4b987fd4e40303ac -->
# Spawn Chain Child

Recursive child for spawn chain test. Writes a marker at its level, then spawns the next level if not at max.

```xml
<directive name="spawn_chain_child" version="1.0.0">
  <metadata>
    <description>Chain child: writes marker at current level, spawns next level if below max_level.</description>
    <category>test/tools/threads</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" spend="0.30" />
    <permissions>
      <execute><tool>*</tool></execute>
      <search>*</search>
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

  <outputs>
    <success>Level {input:level} marker written. Next level spawned if below max.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_level_marker">
    Write "Level {input:level} — child thread" to `chain_L{input:level}.txt`.
  </step>
  <step name="maybe_spawn_next">
    If {input:level} is less than {input:max_level}, spawn another child thread running `test/tools/threads/spawn_chain_child` with level incremented by 1 and the same max_level. If at max_level, report completion.
  </step>
</process>
