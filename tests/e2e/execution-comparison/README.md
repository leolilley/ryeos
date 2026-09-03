# Execution comparison acceptance

This directory is a project fixture for the generic execution-comparison and
authored-measurement acceptance. It is not RyeOS development content and must
not be resolved from the repository's top-level project space.

The checked-in items are deliberately unsigned. An acceptance run copies this
directory to a temporary project and signs the three items with that node's
local operator identity before validation or execution. This keeps the fixture
portable and avoids treating a developer's operator key as repository trust.

`graph:acceptance/simple` produces a deterministic, zero-effect retained run
for comparison testing. It has no provider cost, so its exact cost state is
`unavailable`, not a fabricated zero.

`graph:measure/execution-economy-fold` demonstrates a consumer-authored
measurement against a completed cost-bearing graph thread. Its invocation
seals the expected `knowledge:measure/execution-economy` raw-content digest;
the fold refuses unless the signed frame it observes has that exact digest.
