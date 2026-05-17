<!-- ryeos:signed:2026-05-17T21:44:36Z:e484465796b3b07ec205174cfce59120c112478e2c542726e3d2826f618b40c7:zdoznHfcudHVhoxgScWMzDHGejwNw1CnANz9d1sxmUvhwAg60BpASho/mDmUwG2UzPCLc3xHBR3FCcPLcIOwDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Execute a directive in a managed thread with an LLM loop. Main user-facing directive for thread execution."
version: "1.0.0"
model_tier: general
limits:
  turns: 4
  tokens: 4096
permissions:
  execute:
    - tool:rye.agent.threads.thread_directive
---

# Thread Directive

Execute a directive in a managed thread with an LLM loop.

<process>
  <step name="validate_input">
    Validate that {input:directive_id} is non-empty and well-formed.
    If empty, halt with an error.
  </step>

  <step name="execute_thread">
    Execute the directive in a managed thread:
    `rye_execute(item_id="rye/agent/threads/thread_directive", parameters={"directive_id": "{input:directive_id}", "async": {input:async}, "inputs": {input:inputs}, "model": "{input:model}", "limit_overrides": {input:limit_overrides}})`
  </step>

  <step name="return_result">
    Return the thread result containing thread_id, status, cost, and result text.
    If async was true, return immediately with the thread_id and status "running".
  </step>
</process>

<success_criteria>
  <criterion>Directive name is validated as non-empty</criterion>
  <criterion>Thread execution tool called with correct parameters</criterion>
  <criterion>Result includes thread_id, status, cost, and result text</criterion>
</success_criteria>
