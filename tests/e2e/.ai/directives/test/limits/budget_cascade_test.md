<!-- rye:signed:2026-02-18T05:43:37Z:5471455373decf6a690c0a2c3a1287349267e77d91f02d35fd121bb6dde78c1b:BDsJE_0bV5LQXpX66Hvw_aOL7b1qK86dOh-CKJXFYF06Ho8kFuNsshJKuXd-3W7yKPxlCZF7raYKsZoYwuwBAw==:440443d0858f0199 -->
# Budget Cascade Test

Test that child thread spend cascades back to parent budget. Parent has $0.50 budget, spawns a child — child's actual spend should be reflected in parent's actual_spend.

```xml
<directive name="budget_cascade_test" version="1.0.0">
  <metadata>
    <description>Test: parent spawns child, child's spend cascades back to parent budget. Check budget_ledger.db after run.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="6" tokens="4096" spend="0.50" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye/agent/threads/thread_directive</tool></execute>
    </permissions>
  </metadata>

  <process>
    <step name="write_parent">
      <description>Write a parent marker file.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="budget_parent.txt" />
        <param name="content" value="Parent budget test" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>

    <step name="spawn_budget_child">
      <description>Spawn a child that writes a file. Child's spend should cascade back.</description>
      <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
        <param name="directive_name" value="test/tools/file_system/write_file" />
        <param name="inputs" value='{"message": "Budget child output", "output_path": "budget_child.txt"}' />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Both files written. Check budget_ledger.db — parent actual_spend should include child's spend.</success>
  </outputs>
</directive>
```
