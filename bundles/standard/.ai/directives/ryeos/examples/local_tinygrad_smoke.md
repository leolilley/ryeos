<!-- ryeos:signed:2026-08-11T02:28:27Z:5370af586b4825cdc9039981c469264a10ebda3de3f7dcb907938ad84e6f4bdd:x/8XTuaXcQgwEzX4kilxz9K2354UE3JmRGLcAUSHzk2HP/jdMRk5U5PnJ5GAcCJEMVu5JWCIvh14mvN9PJbQAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Acceptance probe for the admitted recorded local Tinygrad worker."
version: "1.0.0"
model:
  provider: local-tinygrad
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
