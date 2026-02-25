<!-- rye:signed:2026-02-25T00:16:20Z:4ad875f2dcbdae1c54eda31fa16762b8b1e5c9664935daea622136a50f05ad1d:06sdSrrcEn3ROxwGvKAWqiID6fMituK66AB2fS0IPKm_Ur6eU_E5xk7TYgrjS5sHbzIkERmvq7qHhia_Va18AA==:9fbfabe975fa5a7f -->
# Spawn With Context Test

Orchestrator that spawns the leaf_context directive as a child thread. Tests that the entire extends chain context (base system + mid before + leaf after) plus the default system context hooks (identity, behavior, tool-protocol, environment, completion) all flow correctly into the spawned thread's transcript.

```xml
<directive name="spawn_with_context" version="1.0.0">
  <metadata>
    <description>Spawns leaf_context as child thread. Verifies full context injection in spawned thread transcript.</description>
    <category>test/context</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="8" tokens="4096" spend="0.50" depth="3" spawns="2" />
    <permissions>
      <execute><tool>*</tool><directive>test/context/leaf_context</directive></execute>
      <search>*</search>
      <load>*</load>
    </permissions>
  </metadata>

  <outputs>
    <child_thread_id>Thread ID of the spawned leaf_context child</child_thread_id>
    <result>Summary of spawn result</result>
  </outputs>
</directive>
```

<process>
  <step name="spawn_leaf">
    <description>Spawn leaf_context as a synchronous child thread.</description>
    <execute item_type="directive" item_id="test/context/leaf_context">
    </execute>
  </step>

  <step name="write_result">
    <description>Write the child thread result to a file for inspection.</description>
    <execute item_type="tool" item_id="rye/file-system/write">
      <param name="path" value="outputs/context_spawn_result.txt" />
      <param name="content" value="spawn_with_context completed — child spawned" />
      <param name="create_dirs" value="true" />
    </execute>
  </step>
</process>
