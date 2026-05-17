---
description: "Generate the continuation prompt for a handed-off or resumed thread."
version: "1.0.0"
---

# Continuation Prompt

Generate the seed user message for a continuation thread. Executed by `thread_directive.py` step 3.5 when a thread is handed off or resumed — the rendered body becomes the trailing user message in `resume_messages`.

You are continuing execution of the directive `{input:original_directive}`.

## Original Instructions

{input:original_directive_body}

## Context

This is a continuation thread. The previous thread (`{input:previous_thread_id}`) hit its context limit. The conversation history from that thread has been reconstructed and precedes this message.

## Instructions

{input:continuation_message}
