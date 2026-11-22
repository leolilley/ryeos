<!-- ryeos:signed:2026-11-17T22:09:14Z:06a3b496e8da461c1b9693dd16377ebbee7d9c092537646ea2e3dcfacfdbf133:fpcxq5bvInx4qv+bahbxdvbo9H5LoFBuLNli+/M6XHTt+pvQ/UK+l4S/4zGcPR/Qhc+DOtFPi7Q7lVD0wm1GAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Base operations directive for ryeos — runs a single operator turn in a thread."
version: "1.0.0"
inputs:
  - name: input
    type: string
    required: true
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.ryeos/core/fetch
      - ryeos.execute.tool.ryeos/core/verify
      - ryeos.execute.tool.ryeos/core/node/doctor
      - ryeos.execute.tool.ryeos/knowledge/compose
      - ryeos.execute.knowledge.*
      - ryeos.execute.tool.core/snapshot-status
      - ryeos.execute.tool.core/snapshot-show
      - ryeos.execute.tool.core/snapshot-log
      - ryeos.read.project.live
---
${inputs.input}
