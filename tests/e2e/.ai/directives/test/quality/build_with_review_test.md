<!-- rye:signed:2026-03-03T23:00:19Z:ee640f7a92b486f5b3af012adb7b4eed0eaf49ede284aa806d942b150381b24f:OlMjMVU04VtGi1OmTfnBYIASR3vigRawleWX-d9yJ0eQZL7A9ZimZwZuu4IU8Tp40JdlxFdN4YWJWVv6WhbFBQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

# Build With Review Test

End-to-end test for the build_with_review orchestrator — verifies the full build → review → retry cycle.

```xml
<directive name="build_with_review_test" version="1.0.0">
  <metadata>
    <description>Test the build_with_review orchestrator — spawn a build+review cycle and verify the retry loop.</description>
    <category>test/quality</category>
    <author>rye-os</author>
    <model tier="fast" provider="zen/zen" />
    <limits turns="15" tokens="64000" spend="1.00" spawns="10" depth="5" />
    <permissions>
      <execute>
        <directive>rye.code.quality.build_with_review</directive>
        <tool>rye.file-system.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="output_dir" type="string" required="false">Directory for test output (default: outputs)</input>
  </inputs>

  <outputs>
    <output name="result">Summary of build_with_review test results</output>
  </outputs>
</directive>
```

<process>
  <step name="spawn_build_with_review">
    Spawn the build_with_review orchestrator directive.
    Use a simple build directive (test/quality/quality_gate_test) as the target — this directive writes a file and runs gates, giving the reviewer something to evaluate.
    `rye_execute(item_type="directive", item_id="rye/code/quality/build_with_review", parameters={"thread": true, "directive_id": "test/quality/quality_gate_test", "parameters": {}, "max_retries": 1})`
    Wait for the orchestrator to complete.
  </step>

  <step name="verify_orchestrator_output">
    Check the orchestrator's output:
    - status: should be "approved" or "rejected"
    - build_thread_id: should be a valid thread ID string
    - review_verdict: should be "approve" or "reject"
    - attempts: should be 1 or 2 (depending on whether a retry occurred)
  </step>

  <step name="write_result">
    Write the test results to {input:output_dir|outputs}/build_with_review_test.txt.
    Include: orchestrator status, number of attempts, review verdict, and whether the scrap-and-retry knowledge was present in context.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/build_with_review_test.txt", "content": "<orchestrator test summary>", "create_dirs": true})`
  </step>
</process>

<success_criteria>
  <criterion>Orchestrator thread completes without error</criterion>
  <criterion>At least one build thread was spawned</criterion>
  <criterion>At least one review thread was spawned</criterion>
  <criterion>Final status is either "approved" or "rejected"</criterion>
  <criterion>Scrap-and-retry knowledge was injected into orchestrator context</criterion>
</success_criteria>
