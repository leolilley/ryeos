<!-- ryeos:signed:2026-11-28T20:01:13Z:fe58f433f9f967630f16622821fe4bdc6318ec48e8f01ae8ddd61f5c642392a9:ER0jdwLWGlwEgv9IhtphNTCCZD7SL2GD6dyScCKBLD6WambjHUWykCtX8/8+y4Bjd+RaB78NrSdGzHb7eVhODQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Example working directive for the goal-and-judge graph: does the task and returns one deliverable. It is told only the goal, the round number and the author's repair text for criteria it failed."
version: "1.0.0"
model:
  tier: general
limits:
  turns: 12
  tokens: 120000
inputs:
  - name: goal
    type: string
    required: true
  - name: round
    type: integer
    required: true
  - name: repair
    type: string
    required: false
outputs:
  - name: deliverable
    type: string
    description: "The finished work as plain text. It is read by checks and by a judge that never sees anything else you write."
---

# Goal

${inputs.goal}

This is attempt ${inputs.round}.

${inputs.repair ?? ""}

Do the work, then call `directive_return` once with the finished work as `deliverable`. Only the deliverable is read; anything else you write is not seen.
