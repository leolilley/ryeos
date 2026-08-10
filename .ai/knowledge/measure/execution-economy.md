<!-- ryeos:signed:2026-08-10T10:33:53Z:98f5e09f55ce7cab1666a88ffa68c2a5e9536fc1e5e3b6ae0e2f94a847d190b7:RpYj5+u77CkIP9O4yvvEo7uefRExhxuufomqGKnNvtrkM6Ba+Al09RXZvjIylSM18cvanPUAifUKI10cNJb5CQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
kind: knowledge
id: measure/execution-economy
version: "1.0.0"
tags: [measure]
frame:
  subject: thread
  observations:
    - service:threads/get
    - service:items/effective
    - service:threads/receipts
  weights:
    tokens_per_completed_step: 1.0
  verdict_schema:
    completed_steps: integer
    total_output_tokens: integer
    tokens_per_completed_step: number
```

# Execution economy

This observer judges the output tokens spent per completed graph step for one
terminal thread. Lower values are better.

A completed step is a graph node that crossed its durable receipt boundary.
This includes a handled node failure when the graph deliberately continued;
the judgment measures execution work, not only successful actions.

`tokens_per_completed_step` is the raw ratio multiplied by the frame's
`weights.tokens_per_completed_step`. The initial weight is neutral. Changing
the weight changes the judgment and requires a newly signed frame digest.

The fold must refuse a non-terminal subject, a non-graph subject, an untrusted
frame, a frame whose observed raw-content digest differs from the expected
digest sealed into the fold invocation, a frame with a different schema, a
subject with no completed steps, or a subject without an authoritative
canonical output-token cost. A real zero cost remains a valid observation.

The returned observation action digests identify the three exact requests;
they are not response hashes. The fold thread's durable node-receipt
`node_result_hash` values pin the observed response bytes used by the verdict.
The fold's sealed invocation pins the expected frame digest; a verdict exists
only when the signed frame observed by `service:items/effective` has that exact
raw-content digest.
