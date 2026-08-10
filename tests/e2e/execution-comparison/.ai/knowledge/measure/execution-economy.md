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
