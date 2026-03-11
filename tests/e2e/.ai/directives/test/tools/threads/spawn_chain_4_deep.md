<!-- rye:signed:2026-03-11T07:13:35Z:e8b1cd2d8a65ebd51518c05ad5cd4e351089d01e0eb9c278e67e6ce5de43ea1d:8RSKLYsB0BViMqXe8OWUxM9d20db5LQL1nLEX6i80xiYQWeN04gMxDvdKC8DqHMoJ8EQmMTHaLLcVKBd2y0NBQ==:4b987fd4e40303ac -->
# Spawn Chain 4 Levels Deep

End-to-end test: root → L1 → L2 → L3. Each level writes a marker file. Tests budget cascading, limit inheritance, permission inheritance, and depth tracking across 4 levels.

```xml
<directive name="spawn_chain_4_deep" version="1.0.0">
  <metadata>
    <description>Root of a 4-level spawn chain. Writes root marker, spawns level 1 child. Tests full inheritance stack.</description>
    <category>test/tools/threads</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" spend="1.00" depth="5" spawns="3" />
    <permissions>
      <execute><tool>*</tool></execute>
      <search>*</search>
    </permissions>
  </metadata>

  <outputs>
    <success>Root wrote chain_L0.txt and spawned level 1. Check chain_L1.txt, chain_L2.txt, chain_L3.txt for full chain.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_root">
    Write "Level 0 — root thread" to `chain_L0.txt`.
  </step>
  <step name="spawn_level_1">
    Spawn a child thread running directive `test/tools/threads/spawn_chain_child` with inputs level="1" and max_level="3".
  </step>
</process>
