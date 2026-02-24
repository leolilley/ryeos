<!-- rye:signed:2026-02-22T02:31:19Z:c791445ba3954e9409856b707f6e778a8ad6de09a21e847e4d5ee2ee0b51904a:HvcV3ahA856p1Qhwghbn21DiVWzF94FkJNcnbwjK9cMYhpZ35KI6MP_3BX16volVqLwh8Niq1-7qXZv4mZpyDQ==:9fbfabe975fa5a7f -->
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
      <cap>rye.execute.tool.rye.file-system.*</cap>
      <cap>rye.execute.tool.rye.agent.threads.*</cap>
      <cap>rye.search.*</cap>
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
