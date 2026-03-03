<!-- rye:signed:2026-03-03T23:00:19Z:db6948a1aaa4623deedaa2f6889cd4d7eaddfba97cbd20ec95674facbc5ba471:mblcgn3sbwnykCbIPLdJMWIMT8daS2cKik3s-y0Yuh1nVG_j4f79MOkp6Cuh0xiY8cOXzaKq02a5hG4_AQYIAQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

# Quality Gate Test

End-to-end test for the quality gate tool — verifies gate execution, pass/fail reporting, and config loading.

```xml
<directive name="quality_gate_test" version="1.0.0">
  <metadata>
    <description>Test the quality gate tool — run configured gates and verify structured results.</description>
    <category>test/quality</category>
    <author>rye-os</author>
    <model tier="fast" provider="zen/zen" />
    <limits turns="8" tokens="8192" spend="0.10" />
    <permissions>
      <execute>
        <tool>rye.code.quality.gate</tool>
        <tool>rye.file-system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="output_dir" type="string" required="false">Directory for test output (default: outputs)</input>
  </inputs>

  <outputs>
    <output name="result">Summary of gate test results</output>
  </outputs>
</directive>
```

<process>
  <step name="run_gates">
    Run the quality gate tool with the default config path.
    `rye_execute(item_type="tool", item_id="rye/code/quality/gate", parameters={})`
    Record the result — expect success=true, all gates passing (they are echo commands).
  </step>

  <step name="verify_structure">
    Verify the response contains:
    - "success": true
    - "gates": array with 4 entries (lint, typecheck, test, coverage)
    - "all_passed": true
    - "required_passed": true
    - Each gate entry has "name", "passed", "required", "exit_code", "output"
  </step>

  <step name="write_result">
    Write the verification results to {input:output_dir|outputs}/quality_gate_test.txt.
    Include: number of gates checked, pass/fail status, and whether the structured output matched expectations.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/quality_gate_test.txt", "content": "<verification summary>", "create_dirs": true})`
  </step>
</process>

<success_criteria>
  <criterion>Gate tool returns success=true</criterion>
  <criterion>All 4 gates (lint, typecheck, test, coverage) are present in results</criterion>
  <criterion>All gates report passed=true</criterion>
  <criterion>Output file written with verification summary</criterion>
</success_criteria>
