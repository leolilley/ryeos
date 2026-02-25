<!-- rye:signed:2026-02-24T23:52:30Z:b8c9a185ed19ac2522a181803b978305e42dd532023e3e4727591f310caf9eaa:woUbnOa0-XTZkY7gxjogyM_JifT92_b6_TaJ8e13Q-jibajnbum0m1yE3A1yyaboUlnN9B8lZC36Fputqn7TDw==:9fbfabe975fa5a7f -->
# Permission Inheritance Test

Test that child threads inherit parent capabilities when child declares none. Parent has fs+thread permissions. Child (write_file) has its own fs caps — those should be used. But a child with NO caps should inherit parent's.

```xml
<directive name="perm_inheritance_test" version="1.0.0">
  <metadata>
    <description>Test: parent caps propagate to child. Parent has fs+thread caps. Spawns child that has its own caps (should use child's own).</description>
    <category>test/permissions</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" spend="0.30" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
        <tool>rye.agent.threads.*</tool>
      </execute>
      <search>*</search>
    </permissions>
  </metadata>
  <outputs>
    <success>Child used its own caps (fs only). Parent caps were available as fallback but not needed.</success>
  </outputs>
</directive>
```

<process>
  <step name="spawn_child_with_own_caps">
    <description>Spawn write_file — it has its own caps (fs only). Child should use its own caps, not inherit parent's.</description>
    <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
      <param name="directive_name" value="test/tools/file_system/write_file" />
      <param name="inputs" value='{"message": "Perm inheritance test", "output_path": "perm_inherited.txt"}' />
    </execute>
  </step>
</process>
