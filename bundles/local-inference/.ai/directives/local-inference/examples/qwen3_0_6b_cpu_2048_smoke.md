<!-- ryeos:signed:2026-09-02T13:27:39Z:9ee6bb91e467e9c4a31f71dfd5ebfa3d64a6c66788943ad86ffb138953b42dd0:hTDX8bJ7YA4Pw/CYsRfSxT2Nl0azpfBCq7Z960RgXqDg9v7buHXUcJV9NgKLhwuBBFKN8uNQ99vu2P95DY9KCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Acceptance probe for the exact Qwen3 0.6B CPU / nproc 2048 profile."
version: "1.0.0"
model:
  provider: qwen3-0.6b-cpu-2048
  name: qwen3-0.6b
  context_window: 2048
  reasoning:
    mode: disabled
effects: recorded
limits:
  turns: 1
  tokens: 512
  spend_usd: "0.01"
continuation: false
---

Reply with exactly `OK` and nothing else.
