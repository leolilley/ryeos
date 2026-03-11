<!-- rye:signed:2026-03-11T07:13:35Z:b8c9a185ed19ac2522a181803b978305e42dd532023e3e4727591f310caf9eaa:z7UzMKQjr1jUUPnnMkqa_axDEl7qf8NjhD6OzT7OIKOKjkyW8DlRPZwrWEFrgju_ALu6bfAGeEVNzsu8h1GQAw==:4b987fd4e40303ac -->
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
