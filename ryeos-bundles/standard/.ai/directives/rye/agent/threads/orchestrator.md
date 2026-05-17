---
description: "Multi-thread orchestration operations: wait, cancel, status, resume, handoff, and result aggregation."
version: "1.0.0"
model_tier: general
limits:
  turns: 4
  tokens: 4096
permissions:
  execute:
    - tool:rye.agent.threads.orchestrator
---

# Orchestrator

Multi-thread orchestration — wait, cancel, status, resume, handoff.

<process>
  <step name="validate_operation">
    Validate that {input:operation} is one of the supported operations:
    wait_threads, cancel_thread, kill_thread, get_status, list_active,
    aggregate_results, get_chain, chain_search, read_transcript,
    resume_thread, handoff_thread.
    If invalid, halt with an error listing valid operations.
  </step>

  <step name="execute_operation">
    Call the orchestrator tool with the operation and relevant parameters:
    `rye_execute(item_id="rye/agent/threads/orchestrator", parameters={"operation": "{input:operation}", "thread_ids": {input:thread_ids}, "thread_id": "{input:thread_id}", "timeout": {input:timeout}, "query": "{input:query}", "message": "{input:message}, "tail_lines": {input:tail_lines}})`
    Only pass parameters that are relevant to the chosen operation.
  </step>

  <step name="return_result">
    Return the operation result. Format depends on operation:
    - wait_threads: list of thread statuses and results
    - cancel_thread/kill_thread: confirmation of cancellation
    - get_status: thread status details
    - list_active: list of active thread summaries
    - aggregate_results: combined results from multiple threads
    - get_chain/chain_search: thread chain information
    - read_transcript: transcript lines
    - resume_thread: resumed thread status
    - handoff_thread: handoff confirmation
  </step>
</process>

<success_criteria>
  <criterion>Operation is validated as a supported type</criterion>
  <criterion>Orchestrator tool called with correct parameters for the operation</criterion>
  <criterion>Result returned in appropriate format for the operation type</criterion>
</success_criteria>
