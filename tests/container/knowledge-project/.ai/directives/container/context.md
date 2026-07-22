---
name: context
category: "container"
description: "Container knowledge composition qualification fixture"
inputs:
  - name: name
    type: string
    required: true
model:
  tier: general
context:
  system:
    - "knowledge:container/important_fact"
---
Repeat whatever context was provided.
