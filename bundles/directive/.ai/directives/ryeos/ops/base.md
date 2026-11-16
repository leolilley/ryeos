<!-- ryeos:signed:2026-11-13T04:34:21Z:f80937723e73d82eb355e6cb325a6d605a47701b70253004e552ffd1e8b6ee09:zj9N70gAXPQ0FhiHCRMq2XJW6afq0SyNoX/p/qQ/7DRMCOIhg+eZlyaGbpkrEoe+eOtZNv9U1mCovwex2+wZCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
