<!-- rye:signed:2026-03-29T06:19:02Z:4fae68cc242bce6c597db239d569c0455d31e74dfe4dd713d1c0f84896f93e3b:F741VNwYr5mwwUTTokBsSgPbSSpfXbKgacjkcW28RLKISN2VRrIUOZ1fObbSb3juk-QFltl32p_sXKh_0h-YCQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->
# Quality Review

Review code changes for quality issues — runs gates then uses LLM analysis against anti-slop practices.

```xml
<directive name="review" version="1.0.0" extends="rye/agent/core/base_review">
  <metadata>
    <description>Review code changes for quality — deterministic gates + semantic LLM analysis</description>
    <category>rye/code/quality</category>
    <author>rye-os</author>
    <model tier="general" />
    <limits turns="10" tokens="32000" />
    <context>
      <before>rye/code/quality/practices</before>
    </context>
    <permissions>
      <execute>
        <tool>rye.code.quality.gate</tool>
        <tool>rye.code.git.git</tool>
        <tool>rye.file-system.read</tool>
        <tool>rye.file-system.glob</tool>
        <tool>rye.file-system.grep</tool>
        <knowledge>*</knowledge>
      </execute>
      <fetch>*</fetch>
    </permissions>
  </metadata>

  <inputs>
    <input name="thread_id" type="string" required="false">Thread ID of the producing thread to review</input>
    <input name="changed_files" type="array" required="false">List of changed file paths to review</input>
    <input name="base_ref" type="string" required="false">Git ref to diff against (default: HEAD~1)</input>
  </inputs>

  <outputs>
    <output name="verdict">approve or reject</output>
    <output name="reasoning">Explanation of the verdict with specific issues found</output>
    <output name="gate_results">Structured pass/fail results from quality gates</output>
    <output name="issues">List of specific quality issues found in the code changes</output>
  </outputs>
</directive>
```

<process>
  <step name="run_quality_gates">
    Run the project's quality gates for deterministic pass/fail signals.
    `rye_execute(item_type="tool", item_id="rye/code/quality/gate", parameters={})`
    Record the gate results. If any required gate fails, this is strong evidence toward rejection.
  </step>

  <step name="get_diff">
    Get the diff of changed files. If {input:changed_files} is provided, diff those files specifically.
    Otherwise, diff against {input:base_ref} (defaulting to HEAD~1).
    `rye_execute(item_type="tool", item_id="rye/code/git/git", parameters={"action": "diff", "args": ["{input:base_ref}"]})`
    If {input:changed_files} is provided:
    `rye_execute(item_type="tool", item_id="rye/code/git/git", parameters={"action": "diff", "args": ["{input:base_ref}", "--"] })`
    with the changed files appended to args.
  </step>

  <step name="read_changed_files">
    For each changed file, read the full file content and its surrounding context.
    Use `rye.file-system.read` to read each file.
    Also read neighboring files in the same directory to understand existing patterns and conventions.
  </step>

  <step name="analyze_changes">
    With the anti-slop practices (injected via context) as your evaluation criteria, analyze the diff and file contents for:

    1. **Unnecessary abstractions** — New files or classes that have only a single caller. Utility modules without multiple independent users. Wrapper classes around single function calls.
    2. **Excessive mocking** — Tests that mock more than they test. Mocks of things that could be real (databases, filesystems, internal modules). More than 3 mocks in a single test.
    3. **Over-engineering** — Design patterns without justification (Factory/Strategy/Builder for single implementations). Configuration for single-use values. Generic frameworks where a function would suffice.
    4. **Dead code** — Commented-out code blocks. Unused imports, variables, or functions. Placeholder implementations.
    5. **Style drift** — Inconsistencies with surrounding code: indentation, quote style, naming conventions, import ordering, error handling patterns.
    6. **Scope creep** — Changes outside the task scope. Refactoring mixed with feature work. "While I'm here" improvements.

    For each issue found, note the file, the specific lines, and why it violates the practices.
  </step>

  <step name="render_verdict">
    Determine the verdict:
    - **reject** if any required quality gate failed, OR if there are warning-severity or higher issues that indicate the changes introduce slop.
    - **approve** if all required gates pass AND the changes follow the anti-slop practices, OR if issues found are minor (info-severity only).

    Return structured results via {output:verdict}, {output:reasoning}, {output:gate_results}, and {output:issues}.
  </step>
</process>
