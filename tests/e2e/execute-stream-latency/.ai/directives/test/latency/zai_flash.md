<!-- ryeos:signed:2026-07-24T09:32:54Z:2e0acf7bf6d3442c8cb68a9b24e01c3e52d87f21b9a24e150cffe15c92b949bf:kB5I0crNT+BwRMsytFLjahFL+kDSaKvAO+vumAGmbXeFD+1Mx1vOqPV8QwQsrvMqBACEREUmmXPfMDQiFydiDg==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
description: "Minimal E2E directive for execute-stream latency measurements through Z.AI."
version: "1.0.0"
model:
  provider: zai
  name: glm-4.7-flash
  context_window: 200000
limits:
  turns: 1
  tokens: 1024
continuation: false
inputs:
  - name: message
    type: string
    required: true
  - name: history
    type: string
    required: false
  - name: db_context
    type: string
    required: false
  - name: workspace_state
    type: string
    required: false
---

${inputs.message}
