<!-- ryeos:signed:2026-09-02T13:27:39Z:574de0e5808ff7e39a64816b53627165fd38d223716ff6f709b2944e7b510026:mxL6c1kLFV2p9xG7RUdhmx+sCOyiGVZxn6SZQzcjGwGZXOrNAkN+m+HmQ2njYTlMul8ERET4oMa45uk3fXkLCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Acceptance probe for the exact Qwen3 0.6B CPU / nproc 4096 profile."
version: "1.0.0"
model:
  provider: qwen3-0.6b-cpu-4096
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
