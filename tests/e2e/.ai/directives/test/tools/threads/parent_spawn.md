<!-- rye:signed:2026-02-22T02:31:19Z:d3cd826dacbba4c33f66edc7a0a802d671161613b83338489dec1c43db32bab6:2UyEKoNa6uKLW8whUr3HSr5oUedHeyhs5Y-WR3brCkCkd1z9dqMLcPgQd6Wc5cDv4cN-4KoPy0TRk5qRvZd2Cw==:9fbfabe975fa5a7f -->
# Parent Spawn

Parent directive that writes its own file, then spawns a child thread to write a second file. Tests recursive thread spawning — should produce two thread folders.

```xml
<directive name="parent_spawn" version="1.1.0">
  <metadata>
    <description>Write a parent log file, then spawn a child thread (test/tools/file_system/child_write) to write a second file. Verifies both files exist.</description>
    <category>test/tools/threads</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="8" tokens="32000" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye.agent.threads.thread_directive</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="parent_message" type="string" required="true">
      Message the parent writes to its own file.
    </input>
    <input name="child_message" type="string" required="true">
      Message the child thread writes to its file.
    </input>
  </inputs>

  <outputs>
    <output name="parent_path">Path to the parent's output file</output>
    <output name="child_thread_id">Thread ID of the spawned child</output>
    <output name="child_outputs">Structured outputs returned by the child thread</output>
  </outputs>
</directive>
```

<process>
  <step name="parent_write">
    Write the parent's message to parent_output.md:
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "parent_output.md", "content": "{input:parent_message}"})`
  </step>

  <step name="spawn_child">
    Spawn a child thread running test/tools/file_system/child_write to write child_output.md:
    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "test/tools/file_system/child_write", "inputs": {"message": "{input:child_message}", "file_path": "child_output.md"}})`
  </step>

  <step name="verify_parent">
    Read back the parent output file to confirm it was written:
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"path": "parent_output.md"})`
  </step>

  <step name="return_result">
    Return the parent file path, child thread ID, and child outputs using directive_return.
  </step>
</process>
