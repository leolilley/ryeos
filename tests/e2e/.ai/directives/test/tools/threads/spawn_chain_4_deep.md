<!-- rye:signed:2026-02-24T23:52:30Z:e8b1cd2d8a65ebd51518c05ad5cd4e351089d01e0eb9c278e67e6ce5de43ea1d:o3JE5rKWHj1seShWzRMAh3ajew_yTiln8derG8qROjrRxY2uX1JJMdWcljzf41MYoaixKrk5zxpsmjlV9Ko7Dw==:9fbfabe975fa5a7f -->
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
    <description>Write root marker file.</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="chain_L0.txt" />
      <param name="content" value="Level 0 — root thread" />
      <param name="mode" value="overwrite" />
    </execute>
  </step>

  <step name="spawn_level_1">
    <description>Spawn level 1 child.</description>
    <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
      <param name="directive_name" value="test/tools/threads/spawn_chain_child" />
      <param name="inputs" value='{"level": "1", "max_level": "3"}' />
    </execute>
  </step>
</process>
