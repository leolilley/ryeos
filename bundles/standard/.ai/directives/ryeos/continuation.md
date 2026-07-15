<!-- ryeos:signed:2026-07-15T07:49:16Z:ecb4b30bf74e9bef7cb301151a7afd3616d82bab4293a539e0a19d7f9c999e30:kkSzmPaifflQsAsk/JZoBMN+kUuucyj7XmsOx9HWTmBd+MTYdaC1/zSyawqO8jvrn9XLIszdgHVOD+mHWMlODA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Generate the continuation prompt for a handed-off or resumed thread."
version: "1.0.0"
---

# Continuation Prompt

Generate the seed user message for a continuation thread. Executed by `thread_directive.py` step 3.5 when a thread is handed off or resumed — the rendered body becomes the trailing user message in `resume_messages`.

You are continuing execution of the directive `${inputs.original_directive}`.

## Original Instructions

${inputs.original_directive_body}

## Context

This is a continuation thread. The previous thread (`${inputs.previous_thread_id}`) hit its context limit. The conversation history from that thread has been reconstructed and precedes this message.

## Instructions

${inputs.continuation_message}
