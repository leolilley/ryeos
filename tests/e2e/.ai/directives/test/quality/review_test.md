<!-- rye:signed:2026-03-11T07:13:35Z:808dc195b0a80b36be82bffdbebcd9c65ae4b84ee368ddc832aa640f1aef85bc:kv470ao9JK1mtFDX4peiEsNt7AdEj2GFBdRAvucoKXOTw-gf-t0BhmFmUYNa9OZOekKshkPtWdKfba_TlglsDw==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

# Quality Review Test

End-to-end test for the review directive — verifies the full review flow with LLM analysis against anti-slop practices.

```xml
<directive name="review_test" version="1.0.0">
  <metadata>
    <description>Test the quality review directive — spawn a review thread and verify it produces a structured verdict.</description>
    <category>test/quality</category>
    <author>rye-os</author>
    <model tier="fast" provider="zen/zen" />
    <limits turns="12" tokens="32000" spend="2.00" spawns="3" />
    <permissions>
      <execute>
        <directive>rye.code.quality.review</directive>
        <tool>rye.file-system.*</tool>
        <tool>rye.code.git.git</tool>
        <tool>rye.code.quality.gate</tool>
        <knowledge>*</knowledge>
      </execute>
      <fetch>*</fetch>
    </permissions>
  </metadata>

  <inputs>
    <input name="output_dir" type="string" required="false">Directory for test output (default: outputs)</input>
  </inputs>

  <outputs>
    <output name="result">Summary of review test results including verdict</output>
  </outputs>
</directive>
```

<process>
  <step name="create_test_file">
    Write a small test Python file that the review will analyze.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/review_target.py", "content": "def add(a, b):\n    return a + b\n\ndef subtract(a, b):\n    return a - b\n", "create_dirs": true})`
  </step>

  <step name="spawn_review">
    Spawn the review directive as a managed thread, passing the test file as a changed file.
    `rye_execute(item_type="directive", item_id="rye/code/quality/review", parameters={"thread": "fork", "changed_files": ["{input:output_dir|outputs}/review_target.py"]})`
    Wait for the thread to complete.
  </step>

  <step name="verify_review_output">
    Check that the review thread completed and produced structured outputs:
    - A verdict (approve or reject)
    - Reasoning explaining the verdict
    - Gate results from quality gate execution
    - Issues list (may be empty for clean code)
  </step>

  <step name="write_result">
    Write the review test results to {input:output_dir|outputs}/review_test.txt.
    Include: review verdict, reasoning summary, number of issues found, and whether all expected output fields were present.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:output_dir|outputs}/review_test.txt", "content": "<review test summary>", "create_dirs": true})`
  </step>
</process>

<success_criteria>
  <criterion>Review directive thread completes without error</criterion>
  <criterion>Review produces a verdict (approve or reject)</criterion>
  <criterion>Review produces reasoning text</criterion>
  <criterion>Gate results are included in the output</criterion>
  <criterion>Practices knowledge was injected into the review context</criterion>
</success_criteria>
