<!-- ryeos:signed:2026-07-15T07:49:16Z:83c51f70beb626b5d5bf2d94a10162c0fd3049931b3712eabdea2f1c040ca6d7:ZJ5UEn/+MfwJDKHU/JT/9IvjzdpQsTA/r1DZsI/qTn+k2vhK3jz8iRkH6Oh/DKIdTUtRzX9gaUcda8e+TMaaAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
${inputs.input}
