<!-- ryeos:signed:2026-11-28T14:15:53Z:7360e3b5f68e123727a790f1306cba04f9f355c5b64d800f6a564fe9961d5f74:6vUF4sl+GS+hPzefxWwFJsj+ChkobsRXOvNk0Ec0WMslSerDlO76gnF0pQ4izGDLY2y/4zZtBd9dWA/1qr5NCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Example working directive for the goal-and-judge graph: does the task and returns one deliverable. It is told only the goal, the round number and the author's repair text for criteria it failed."
version: "1.0.0"
model:
  tier: general
limits:
  turns: 12
  tokens: 120000
  spend_usd: "1"
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
