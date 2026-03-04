<!-- rye:signed:2026-03-03T23:15:01Z:3aee626939d54416cdd0f6731ec032b8fc8cd2e005daa865b4786c0e704cb5ec:P9QW96ybR9hhWwwOVLaic78APhEEvSAi2qEpJuTXK-J99Pa09KrDeTvzh0F0Y6iHd6PTkPLAykzr1a1B71ArDg==:4b987fd4e40303ac -->
```yaml
id: build_with_review-1772579687352
title: "rye/code/quality/build_with_review"
entry_type: thread_transcript
category: agent/threads/rye/code/quality/build_with_review
version: "1.0.0"
author: rye
created_at: 2026-03-03T23:14:47Z
thread_id: rye/code/quality/build_with_review/build_with_review-1772579687352
directive: rye/code/quality/build_with_review
status: completed
model: claude-haiku-4-5
duration: 13.7s
elapsed_seconds: 13.74
turns: 5
input_tokens: 26249
output_tokens: 814
spend: 0.024255200000000005
tags: [thread, completed]
```

# rye/code/quality/build_with_review

## System Prompt (custom)

You are Rye — one agent executing across concurrent threads. Each thread is a focused
context for a specific task. You are not a chatbot. You are an execution engine.

Your tools are the Rye OS interface. Every response must contain tool calls that advance
the task. Do not narrate, explain, or ask for confirmation — execute.

You share knowledge across threads through the `.ai/knowledge/` filesystem. What one
thread learns, another can access. Write findings, decisions, and outcomes so they
persist beyond your context window.

## Behavioral Rules

1. **Execute the directive immediately**. When a `<directive>` with `<process>` steps is provided, begin executing step 1 on your very first response. Your first output must be tool calls — never narration, planning, or questions. The inputs are already interpolated into the directive body.
2. **Batch tool calls**. If multiple operations are independent, call them in parallel. Serial calls waste budget.
3. **Fail fast**. If you are blocked (permission denied, missing files, wrong parameters), return error immediately via directive_return. Do not retry the same failing approach.
4. **Stay in scope**. Only perform actions authorized by the directive's permissions. Do not attempt to work around permission denials.
5. **Be token-efficient**. Every tool call costs tokens. Read only what you need. Write concise outputs. Do not echo file contents back unless asked.

---

## Input — Turn 1

<Environment id="rye/agent/core/Environment" type="knowledge">
## Thread Environment

- **Project**: /home/leo/projects/ryeos/tests/e2e
- **Model**: claude-haiku-4-5
- **Thread depth**: 0 (0 = root)
- **Parent thread**: none (none if root)
- **Budget**: 1.0 USD, 15 turns
- **Capabilities**: rye.execute.directive.rye.code.quality.build_with_review, rye.execute.tool.rye.agent.threads.directive_return
</Environment>

### rye_execute — Run items

Execute a tool, directive, or knowledge item. This is your primary action tool.

```json
rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "/abs/path", "content": "..."})
rye_execute(item_type="directive", item_id="my/workflow", parameters={"target": "value"})
rye_execute(item_type="knowledge", item_id="project/design-spec")
```

### Returning results

When the directive declares outputs, call directive_return:

```json
rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"status": "completed", ...})
```

If blocked, return error immediately — do not waste turns:

```json
rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"status": "error", "error_detail": "what is missing"})
```

### Integrity errors

If execution fails with an IntegrityError, the error message tells you exactly what to do:
- **Unsigned item** → run `rye sign {type} {item_id}` (the exact command is in the error)
- **Content modified** → the file was edited after signing, re-sign it
- **Untrusted key** → the error lists all trusted key fingerprints

### rye_search — Discover items

Find item IDs before executing or loading them. Use when you don't know the exact ID.

```json
rye_search(scope="tool", query="file system")
rye_search(scope="knowledge", query="design spec")
rye_search(scope="directive", query="build")
rye_search(scope="tool.rye.web.*", query="*")
```

### Shadow detection

When `rye_search` returns results from multiple spaces, items may include:
- `shadows` — this item overrides the same item_id in a lower space
- `shadowed_by` — this item is overridden by a higher-precedence space

If a project tool shadows a system tool, search results make this visible.

### rye_load — Inspect items

Read raw content and metadata. Use to check a tool's parameter schema or a directive's inputs before executing.

```json
rye_load(item_type="tool", item_id="rye/file-system/write")
rye_load(item_type="directive", item_id="my/workflow")
```

### When unsure

If a directive tells you to use a tool and you don't know the exact item_id or parameters:
1. `rye_search` to find the item
2. `rye_load` to inspect its schema
3. `rye_execute` to run it

Do NOT guess parameters. Look them up.

### rye_sign — Sign items

Validate and sign items after editing. Required after any modification.

```json
rye_sign(item_type="directive", item_id="my/workflow")
rye_sign(item_type="tool", item_id="*")  // glob to batch sign
```

# Scrap and Retry

Never fix bad output. Discard it, fix the root cause, and rerun from scratch.

## The Principle

When an agent produces low-quality output — wrong approach, poor structure, slop patterns, failing tests — do not attempt to patch it. The agent that produced bad output will produce bad patches for the same reasons. Instead:

1. **Diagnose** — Identify why the output was bad. Missing context? Wrong directive? Bad input data? Ambiguous requirements?
2. **Discard** — Throw away the entire output. Do not salvage partial results.
3. **Fix** — Address the root cause. Inject missing context, clarify the directive, fix the input, narrow the scope.
4. **Rerun** — Execute the directive again from scratch with the fix applied.

## Why Not Patch

- Patching bad output compounds errors. Each fix introduces new assumptions that may conflict with the original bad assumptions.
- The agent's context is already polluted by the bad reasoning that led to the bad output. Asking it to fix its own work means reasoning on top of flawed reasoning.
- A clean rerun with the root cause fixed produces better results in fewer total tokens than iterative patching.

## For Orchestrators

When coordinating build-then-review cycles:

- If the review rejects the output, do not send the rejection feedback to the same thread. That thread's context is contaminated.
- Spawn a new thread with the original directive plus the review feedback injected as additional context.
- Cap the retry count. If the same directive fails 3 times with different root-cause fixes, the problem is in the directive or the requirements — escalate to the user.
- Each retry should include: the original task, the review verdict from the previous attempt, and the specific root-cause fix applied. The new thread should understand what went wrong without seeing the bad output itself.

## When to Escalate Instead

Not every failure is retriable. Escalate when:

- The directive itself is ambiguous or contradictory.
- The required information does not exist in the codebase or provided context.
- The task requires capabilities the agent does not have (e.g., needs write access but only has read-only).
- The same failure pattern repeats across retries despite different root-cause fixes.

<directive name="build_with_review">
<description>Orchestrate build → review → retry cycle with scrap-and-retry on rejection</description>
<permissions>
      <execute>
        <directive>rye.code.quality.review</directive>
        <directive>*</directive>
        <tool>rye.code.quality.gate</tool>
        <tool>rye.code.git.git</tool>
        <knowledge>*</knowledge>
      </execute>
      <search>*</search>
      <load>*</load>
    </permissions>
<process>
  <step name="initialize">
    Set max_retries to 1 (default: 2).
    Set attempt = 0.
    Set review_feedback = null.
  </step>

  <step name="build_loop">
    While attempt &lt; max_retries + 1:

    1. Increment attempt.

    2. Build the input parameters for the build directive.
       Start with {} (or empty object if not provided).
       If review_feedback is not null, add it to the parameters as "review_feedback":
       this should include the previous review's verdict, reasoning, and specific issues
       so the build agent understands what to avoid.

    3. Spawn the build directive as a thread:
       `rye_execute(item_type="directive", item_id="test/quality/quality_gate_test", parameters={"thread": true, ...built_params})`
       Wait for the thread to complete. Record the thread_id.

    4. Get the list of changed files from git:
       `rye_execute(item_type="tool", item_id="rye/code/git/git", parameters={"action": "status"})`

    5. Spawn the review directive:
       `rye_execute(item_type="directive", item_id="rye/code/quality/review", parameters={"thread": true, "thread_id": "<build_thread_id>", "changed_files": <changed_files>})`
       Wait for the review thread to complete.

    6. Check the review verdict:
       - If "approve": break the loop, return success.
       - If "reject": set review_feedback to the review's reasoning and issues.
         Following the scrap-and-retry principle, do NOT attempt to patch the output.
         The next iteration spawns a fresh build thread with the feedback injected.
       - If error: break the loop, return error.
  </step>

  <step name="return_results">
    Return the final status:
    - If the last review approved: status = "approved"
    - If all retries exhausted with rejections: status = "rejected"
    - Include {output:build_thread_id}, {output:review_verdict}, {output:attempts}, and {output:review_reasoning}.
  </step>
</process>
When you have completed all steps, return structured results:
`rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"status": "<final status: approved, rejected, or error (string)>", "build_thread_id": "<Thread ID of the final build attempt (string)>", "review_verdict": "<The review verdict from the last review cycle (string)>", "attempts": "<Number of build attempts made (string)>", "review_reasoning": "<Reasoning from the final review (string)>"})`

If you are BLOCKED and cannot complete the directive — missing context, permission denied on a required tool, required files not found, or repeated failures on the same error — do NOT waste turns working around it. Return immediately with an error:
`rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"status": "error", "error_detail": "<what is missing or broken>"})`
</directive>

Execute the directive above now. Begin with step 1. Your first output must be tool calls — no narration. The inputs are already interpolated into the directive body.

### Response — Turn 1

### Tool: rye_execute

```json
{
  "item_type": "directive",
  "item_id": "rye/code/quality/build_with_review",
  "parameters": {}
}
```

### Result

```
{
  "status": "error",
  "error": "Missing required inputs: directive_id",
  "item_id": "rye/code/quality/build_with_review",
  "declared_inputs": [
    {
      "name": "directive_id",
      "type": "string",
      "required": true,
      "description": "Directive to execute as the build step"
    },
    {
      "name": "parameters",
      "type": "object",
      "required": false,
      "description": "Input parameters to pass to the build directive"
    },
    {
      "name": "max_retries",
      "type": "integer",
      "required": false,
      "description": "Maximum retry attempts on rejection (default: 2)"
    }
  ]
}
```

### Response — Turn 2

Load the directive to understand its input schema:

### Tool: rye_load

```json
{
  "item_type": "directive",
  "item_id": "rye/code/quality/build_with_review"
}
```

### Error

Permission denied: 'rye.load.directive.rye.code.quality.build_with_review' not covered by capabilities ['rye.execute.directive.rye.code.quality.build_with_review', 'rye.execute.tool.rye.agent.threads.directive_return']

### Response — Turn 3

Permission denied on load. The directive requires a `directive_id` parameter. Based on the environment (testing e2e, quality gate context), I need to search for the build directive to use:

### Tool: rye_search

```json
{
  "scope": "directive",
  "query": "quality gate test build"
}
```

### Error

Permission denied: 'rye.search.tool' not covered by capabilities ['rye.execute.directive.rye.code.quality.build_with_review', 'rye.execute.tool.rye.agent.threads.directive_return']

### Response — Turn 4

Search is blocked. The permissions indicate I can only execute the build_with_review directive and return. The environment mentions this is in `.ai/tests/e2e` and the context shows I need to provide a `directive_id`. Looking at the environment and the error, this thread is scoped to execute a specific build_with_review orchestration, but it requires the caller to specify which directive to build.

Return error due to missing required input:

### Tool: rye_execute

```json
{
  "item_type": "tool",
  "item_id": "rye/agent/threads/directive_return",
  "parameters": {
    "status": "error",
    "error_detail": "Missing required input: directive_id. The build_with_review orchestration requires a 'directive_id' parameter specifying which directive to build and review. Capabilities restrict search and load operations, so the caller must provide the directive ID."
  }
}
```

### Error

Missing required output fields: build_thread_id, review_verdict, attempts, review_reasoning. Call directive_return again with all required fields.

### Response — Turn 5

Return with all required fields:

### Tool: rye_execute

```json
{
  "item_type": "tool",
  "item_id": "rye/agent/threads/directive_return",
  "parameters": {
    "status": "error",
    "error_detail": "Missing required input: directive_id. The build_with_review orchestration requires a 'directive_id' parameter specifying which directive to build and review. Capabilities restrict search and load operations, so the caller must provide the directive ID.",
    "build_thread_id": "none",
    "review_verdict": "none",
    "attempts": "0",
    "review_reasoning": "Blocked on missing required input"
  }
}
```

### Result

```
{
  "status": "error",
  "error_detail": "Missing required input: directive_id. The build_with_review orchestration requires a 'directive_id' parameter specifying which directive to build and review. Capabilities restrict search and load operations, so the caller must provide the directive ID.",
  "build_thread_id": "none",
  "review_verdict": "none",
  "attempts": "0",
  "review_reasoning": "Blocked on missing required input"
}
```

---

**Completed** -- 5 turns, 27063 tokens, $0.0243, 13.7s
