<!-- ryeos:signed:2026-06-22T02:50:09Z:0f056f1af2f19a31fbf4f73a0a5d89af845169e02856c331da9782cb87b00266:7M9JrUdqBdU1FQ3tp4dMuP9RxMeUOO8aH5FInVwbMHA/SKvUNIjEGq5mKJ1c+SHK04bSKXs17Z5XGEAbIFEEAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Base operations directive for ryeos — runs a single operator turn in a thread."
version: "1.0.0"
model:
  tier: general
inputs:
  - name: input
    type: string
    required: true
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.*
      - ryeos.execute.service.*
      - ryeos.fetch.tool.*
      - ryeos.fetch.service.*
---
{input:input}
