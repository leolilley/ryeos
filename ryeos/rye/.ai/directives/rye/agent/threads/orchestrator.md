<!-- rye:signed:2026-02-22T02:31:19Z:b44e07fe54c9f16c845e9b64c88658b25ace7cda78be4b96f9ca9d0d1628ec2a:sRoshOZXU8FNWNzjwA87zLR_juPI00rmZF-g441k_fPdaMYKn-tbBaen-dI-6xFcPunj_WnH3W5raPTD8MpJAw==:9fbfabe975fa5a7f -->
# Orchestrator

Multi-thread orchestration — wait, cancel, status, resume, handoff.

```xml
<directive name="orchestrator" version="1.0.0">
  <metadata>
    <description>Multi-thread orchestration operations: wait, cancel, status, resume, handoff, and result aggregation.</description>
    <category>rye/agent/threads</category>
    <author>rye-os</author>
    <model tier="general" />
    <limits max_turns="4" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.agent.threads.orchestrator</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="operation" type="string" required="true">
      Orchestration operation (enum: wait_threads, cancel_thread, kill_thread, get_status, list_active, aggregate_results, get_chain, chain_search, read_transcript, resume_thread, handoff_thread)
    </input>
    <input name="thread_ids" type="array" required="false">
      List of thread IDs to operate on (for wait_threads, aggregate_results)
    </input>
    <input name="thread_id" type="string" required="false">
      Single thread ID (for cancel, kill, status, resume, handoff, read_transcript, get_chain)
    </input>
    <input name="timeout" type="number" required="false">
      Timeout in seconds for wait operations
    </input>
    <input name="query" type="string" required="false">
      Search query for chain_search operation
    </input>
    <input name="message" type="string" required="false">
      Message to inject for handoff_thread or resume_thread
    </input>
    <input name="tail_lines" type="integer" required="false">
      Number of transcript lines to return for read_transcript
    </input>
  </inputs>

  <outputs>
    <output name="result">Operation result (varies by operation type)</output>
  </outputs>
</directive>
```

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
    `rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator", parameters={"operation": "{input:operation}", "thread_ids": {input:thread_ids}, "thread_id": "{input:thread_id}", "timeout": {input:timeout}, "query": "{input:query}", "message": "{input:message}", "tail_lines": {input:tail_lines}})`
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
