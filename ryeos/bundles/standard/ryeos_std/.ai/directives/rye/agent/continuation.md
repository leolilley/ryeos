<!-- rye:signed:2026-04-19T09:49:53Z:23ec5e6072c545bbd3f08e726925fe7c850a3c942792a978dc7a186b8bacebc8:RE08TgbrKtCnSoAiHnGHu0nR0K8g2KoPIYvJ1x7xu5cUEHLPiRmr9U7jt3vIjIpnJjZlZY3bZ5f3Vzrv2J5uBg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
