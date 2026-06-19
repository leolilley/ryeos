<!-- ryeos:signed:2026-06-19T02:44:46Z:f18b78e4d418a54ea7e9398ba9f4d9b46ad7bf332f57e5e6447c95fdca53f01c:2Afo1VIfl0R6waibEOHnrLo3GLlj/Q4c9eYaoN+N2+VUyiAPsq3q6mqCDD1cJhzsr804o0oSL+3OHtBv/Tm2BQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Base operations directive for ryeos — runs a single operator turn in a thread."
version: "1.0.0"
model:
  tier: general
inputs:
  - name: input
    type: string
    required: true
permissions:
  execute:
    - ryeos.execute.tool.*
    - ryeos.execute.service.*
  fetch:
    - ryeos.fetch.tool.*
    - ryeos.fetch.service.*
---
{input:input}
