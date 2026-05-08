<!-- ryeos:signed:2026-03-11T07:13:35Z:c879c65df35da3f925e2a09961cd6ad7110ba0cd6ac94858b35e9543fd0443e0:75LGw5xEZVi1Q1oYYELqLIMOkDCEuebSllFPNFmAGnWvsCL-LoDScY64HzepOUbqm1l3_dEbEtsgSfkEZ2pZDA==:4b987fd4e40303ac -->
# Budget Cascade Test

Test that child thread spend cascades back to parent budget. Parent has $0.50 budget, spawns a child — child's actual spend should be reflected in parent's actual_spend.

```xml
<directive name="budget_cascade_test" version="1.0.0">
  <metadata>
    <description>Test: parent spawns child, child's spend cascades back to parent budget. Check budget_ledger.db after run.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" spend="0.50" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye/agent/threads/thread_directive</tool></execute>
    </permissions>
  </metadata>

  <outputs>
    <success>Both files written. Check budget_ledger.db — parent actual_spend should include child's spend.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_parent">
    Write "Parent budget test" to `budget_parent.txt`.
  </step>
  <step name="spawn_budget_child">
    Spawn child directive `test/tools/file_system/write_file` with message "Budget child output" and output_path "budget_child.txt". The child's spend should cascade back to the parent budget.
  </step>
</process>
