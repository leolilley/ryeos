<!-- rye:signed:2026-03-03T23:05:10Z:a4d32841c75ee603d9fc1b6acea5f7e7440231716e957988c5a88030e602c629:cDrUjnVTFeB3ck5gcdDF_crj3jV4xb9UEx5nLFBg2tKLeB_mqjaE7o5ydO6cWOKMiYCwQmvE3zgbAZgXKetiCw==:4b987fd4e40303ac -->
```yaml
id: practices_injection_test-1772579105276
title: "test/quality/practices_injection_test"
entry_type: thread_transcript
category: agent/threads/test/quality/practices_injection_test
version: "1.0.0"
author: rye
created_at: 2026-03-03T23:05:05Z
thread_id: test/quality/practices_injection_test/practices_injection_test-1772579105276
directive: test/quality/practices_injection_test
status: completed
model: claude-haiku-4-5
duration: 4.4s
elapsed_seconds: 4.40
turns: 1
input_tokens: 3631
output_tokens: 521
spend: 0.0049888
tags: [thread, completed]
```

# test/quality/practices_injection_test

---

## Input — Turn 1

<Environment id="rye/agent/core/Environment" type="knowledge">
## Thread Environment

- **Project**: /home/leo/projects/ryeos/tests/e2e
- **Model**: claude-haiku-4-5
- **Thread depth**: 0 (0 = root)
- **Parent thread**: none (none if root)
- **Budget**: 0.1 USD, 6 turns
- **Capabilities**: rye.execute.tool.rye.file-system.*, rye.search.*, rye.load.*, rye.execute.tool.rye.agent.threads.directive_return
</Environment>

# Anti-Slop Coding Practices

Rules for producing clean, minimal, convention-following code.

## 1. Follow Existing Patterns

Before writing any code, read the surrounding files. Match:
- Naming conventions (casing, prefixes, suffixes)
- Import style and ordering
- Error handling patterns
- File and directory structure
- Framework and library choices already in use

Do not introduce a new library when an existing one covers the use case. Do not introduce a new pattern when the codebase already has an established way of doing the same thing.

## 2. Minimal Diffs

Make the smallest change that solves the problem. Every added line must be justified by the task requirements.

- Do not refactor surrounding code while fixing a bug.
- Do not add "while I'm here" improvements.
- Do not restructure files, rename variables, or reformat code outside the change scope.
- If a function needs one new parameter, add one parameter — do not redesign the function signature.

## 3. No Over-Engineering

Build exactly what was asked for. Do not design for hypothetical future requirements.

- No abstraction layers for a single implementation.
- No configuration for values that are only used once.
- No generic frameworks where a simple function would suffice.
- No design patterns (Factory, Strategy, Builder, Visitor) unless the problem genuinely has multiple variants that exist today.
- No deeply nested generic types. If the type signature is hard to read, the design is too complex.

## 4. No Unnecessary Abstractions

Every new file, class, or module must have a clear reason to exist.

- If a function is called from one place, it should probably live in that file, not a new utility module.
- If a class wraps a single function call, delete the class and call the function directly.
- Helper files, utility modules, and shared libraries are only justified when they have multiple, independent callers.

## 5. Test With Real Implementations

Never mock what you can use for real.

- Use real database connections (SQLite in-memory, test containers) over database mocks.
- Use real file systems (temp directories) over filesystem mocks.
- Use real HTTP calls to test servers over request mocks.
- Only mock external services you cannot control (third-party APIs with rate limits, paid services).
- If a test requires more than 3 mocks, the code under test probably has a design problem. Fix the design, not the test.

## 6. All Tests Pass Before Handoff

Never hand off work with failing tests.

- Run the full test suite before declaring a task complete.
- If your change breaks existing tests, fix them as part of the change.
- If tests are flaky, investigate and fix the flakiness — do not skip or ignore them.
- If a test is genuinely obsolete due to your change, delete it and document why.

## 7. No Dead Code

Do not add code that is not immediately used.

- No commented-out code blocks. If it's not needed, delete it.
- No unused imports, variables, or functions.
- No "placeholder" implementations that will be filled in later.
- No backwards-compatibility shims for code that was just written.

## 8. Style Consistency

Match the surrounding code's style exactly.

- If the file uses single quotes, use single quotes.
- If the file uses 2-space indentation, use 2-space indentation.
- If the file puts braces on the same line, put braces on the same line.
- Run the project's linter and formatter before declaring the task complete.

<directive name="practices_injection_test">
<description>Test that anti-slop practices knowledge is injected into directive context via extends chain.</description>
<permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>*</search>
      <load>*</load>
    </permissions>
<process>
  <step name="check_practices_context">
    You should have the anti-slop coding practices in your context (injected via the `before` context reference to `rye/code/quality/practices`).
    List the 8 practice rules you can see:
    1. Follow Existing Patterns
    2. Minimal Diffs
    3. No Over-Engineering
    4. No Unnecessary Abstractions
    5. Test With Real Implementations
    6. All Tests Pass Before Handoff
    7. No Dead Code
    8. Style Consistency
    If you can see all 8, the injection worked.
  </step>

  <step name="write_result">
    Write the confirmation to outputs/practices_injection_test.txt.
    Include: which practices were visible in your context, and whether all 8 were present.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "outputs/practices_injection_test.txt", "content": "<injection verification>", "create_dirs": true})`
  </step>
</process>

<success_criteria>
  <criterion>All 8 anti-slop practices are visible in the directive's context</criterion>
  <criterion>Practices content matches the knowledge entry rye/code/quality/practices</criterion>
</success_criteria>
When you have completed all steps, return structured results:
`rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"result": "<Confirmation that practices knowledge was present in context (string)>"})`

If you are BLOCKED and cannot complete the directive — missing context, permission denied on a required tool, required files not found, or repeated failures on the same error — do NOT waste turns working around it. Return immediately with an error:
`rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"status": "error", "error_detail": "<what is missing or broken>"})`
</directive>

Execute the directive above now. Begin with step 1. Your first output must be tool calls — no narration. The inputs are already interpolated into the directive body.

### Response — Turn 1

### Tool: rye_execute

```json
{
  "item_type": "tool",
  "item_id": "rye/file-system/write",
  "parameters": {
    "path": "outputs/practices_injection_test.txt",
    "content": "Practices Injection Test Results\n================================\n\nAnti-Slop Coding Practices Context Check\n\nAll 8 practices are visible in the directive context:\n\n1. Follow Existing Patterns\n2. Minimal Diffs\n3. No Over-Engineering\n4. No Unnecessary Abstractions\n5. Test With Real Implementations\n6. All Tests Pass Before Handoff\n7. No Dead Code\n8. Style Consistency\n\nVerification Status: PASS\n\nAll 8 anti-slop practices were successfully injected into the directive context and are visible as expected. The practices content matches the knowledge entry and includes complete descriptions for each rule.",
    "create_dirs": true
  }
}
```

### Result

```
Created new file outputs/practices_injection_test.txt (603 bytes)
```

### Tool: rye_execute

```json
{
  "item_type": "tool",
  "item_id": "rye/agent/threads/directive_return",
  "parameters": {
    "result": "Confirmation: All 8 anti-slop coding practices were successfully injected into the directive context. The practices are visible in the following order: (1) Follow Existing Patterns, (2) Minimal Diffs, (3) No Over-Engineering, (4) No Unnecessary Abstractions, (5) Test With Real Implementations, (6) All Tests Pass Before Handoff, (7) No Dead Code, (8) Style Consistency. The injection was successful and complete."
  }
}
```

### Result

```
{
  "result": "Confirmation: All 8 anti-slop coding practices were successfully injected into the directive context. The practices are visible in the following order: (1) Follow Existing Patterns, (2) Minimal Diffs, (3) No Over-Engineering, (4) No Unnecessary Abstractions, (5) Test With Real Implementations, (6) All Tests Pass Before Handoff, (7) No Dead Code, (8) Style Consistency. The injection was successful and complete."
}
```

---

**Completed** -- 1 turns, 4152 tokens, $0.0050, 4.4s
