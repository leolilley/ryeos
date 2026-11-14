<!-- ryeos:signed:2026-11-14T07:47:54Z:cda3608570f105593d8d3198cd55fa0f613b19537b16b068ece8e7aae0852459:4rBzsPd47qI6nFDbGsBWhNPt93WmJNt964d6bx2E61tuK3Zs92dbVTXozZShNJpU5BPqKMlWUEvcMOwTGj64Dg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
      - ryeos.execute.knowledge.ryeos/core/*
      - ryeos.execute.knowledge.ryeos/standard/*
      - ryeos.execute.knowledge.ryeos/ui/*
      - ryeos.execute.tool.core/snapshot-status
      - ryeos.execute.tool.core/snapshot-show
      - ryeos.execute.tool.core/snapshot-log
      - ryeos.read.project.live
---
${inputs.input}
