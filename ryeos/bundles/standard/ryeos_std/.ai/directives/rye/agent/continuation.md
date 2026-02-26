<!-- rye:signed:2026-02-25T07:50:41Z:abf27a0616a1539e8e37dc935409a536473bd2230db3f8dd6dc112e54b1fb5ff:GIzj9lW_EGW3QIEvQEgfWhBwlwh_8A8TLnNTFy9lFQeVr56eGVrIPoEX1v3_N_D-42Le8w9Jp12sUXaqIlfVBQ==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-23T08:18:02Z:abf27a0616a1539e8e37dc935409a536473bd2230db3f8dd6dc112e54b1fb5ff:GIzj9lW_EGW3QIEvQEgfWhBwlwh_8A8TLnNTFy9lFQeVr56eGVrIPoEX1v3_N_D-42Le8w9Jp12sUXaqIlfVBQ==:9fbfabe975fa5a7f -->
# Continuation Prompt

Generate the seed user message for a continuation thread. Executed by `thread_directive.py` step 3.5 when a thread is handed off or resumed — the rendered body becomes the trailing user message in `resume_messages`.

```xml
<directive name="continuation" version="1.0.0">
  <metadata>
    <description>Generate the continuation prompt for a handed-off or resumed thread.</description>
    <category>rye/agent</category>
    <author>rye-os</author>
  </metadata>

  <inputs>
    <input name="original_directive" type="string" required="true">
      The directive name being continued
    </input>
    <input name="original_directive_body" type="string" required="true">
      The directive's body/instructions
    </input>
    <input name="previous_thread_id" type="string" required="true">
      The thread being continued from
    </input>
    <input name="continuation_message" type="string" required="true">
      Instruction for the continuation — user-provided message (for resume) or default handoff instruction
    </input>
  </inputs>
</directive>
```

You are continuing execution of the directive `{input:original_directive}`.

## Original Instructions

{input:original_directive_body}

## Context

This is a continuation thread. The previous thread (`{input:previous_thread_id}`) hit its context limit. The conversation history from that thread has been reconstructed and precedes this message.

## Instructions

{input:continuation_message}
