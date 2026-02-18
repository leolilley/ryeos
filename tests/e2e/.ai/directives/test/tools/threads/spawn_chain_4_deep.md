<!-- rye:signed:2026-02-18T05:43:37Z:4881331fd7cfcb25e90cf09ab54524740d55a49bacdf2e87faf32d9ef2f26db6:Z_osJg9H7GqkGsseQkRsdWAOtT2y1NNGDtarA-RvMn6GRKhi8In0tipuZJgdPkye8ajmYSlIgzHkMb8OPE2NCQ==:440443d0858f0199 -->
# Spawn Chain 4 Levels Deep

End-to-end test: root → L1 → L2 → L3. Each level writes a marker file. Tests budget cascading, limit inheritance, permission inheritance, and depth tracking across 4 levels.

```xml
<directive name="spawn_chain_4_deep" version="1.0.0">
  <metadata>
    <description>Root of a 4-level spawn chain. Writes root marker, spawns level 1 child. Tests full inheritance stack.</description>
    <category>test/tools/threads</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="6" tokens="4096" spend="1.00" depth="5" spawns="3" />
    <permissions>
      <cap>rye.execute.tool.*</cap>
      <cap>rye.search.*</cap>
    </permissions>
  </metadata>

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

  <outputs>
    <success>Root wrote chain_L0.txt and spawned level 1. Check chain_L1.txt, chain_L2.txt, chain_L3.txt for full chain.</success>
  </outputs>
</directive>
```
