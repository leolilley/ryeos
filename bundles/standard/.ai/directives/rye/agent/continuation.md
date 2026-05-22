<!-- ryeos:signed:2026-05-22T07:21:28Z:0bf6e6cf86c668f023acdb777b86af4932e20753ec85b237e8b34d8b7f06120c:5ZZjbVYaFRf+LomU6HYBnj2LedAWxobseKTUuqP2anpgFW2BfQsc0vHMSdPKB7T2RcxRRN2KAhfZ8iH3m9HdBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
