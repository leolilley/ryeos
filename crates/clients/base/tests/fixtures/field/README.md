# Generic field truth fixtures

These fixtures are renderer-independent contract inputs for the living
execution field. They describe a signed build/deploy project only; no domain
projection vocabulary belongs here.

## Claim/evidence table

| Claim | Required evidence |
| --- | --- |
| signed item/version exists | canonical ref plus complete signed-source hash |
| graph structure contains a node/edge | exact definition digest and signed item provenance |
| occurrence ran | typed occurrence identity plus durable event ref |
| retry/iteration ran | occurrence attempt/iteration plus durable event ref |
| thread followed/continued/branched | durable lineage/edge event ref |
| artifact was produced | thread/artifact identity plus content hash when retained |
| state anchor was recorded | durable event ref; manifest status remains independent |
| hook observation exists | daemon-authored event ref, dispatch key, response hash, typed occurrence |

Wall-clock time, labels, array position, and current mutable item bytes are not
evidence for any row above.

## Limits recorded by the contract

- graph checkpoint: 4 MiB substrate shape limit;
- runtime event payload: 256 KiB;
- normalized hook observation: 192 KiB, leaving room for its event envelope;
- performance fixture: 1,000 entities and 3,000 relations;
- a source response must report applied bounds and truncation instead of
  silently dropping facts.

The focused fixture test constructs the scale fixture deterministically rather
than checking a large generated JSON blob into the repository.

## Redaction

Fixture capture retains stable IDs, refs, hashes, event type, typed occurrence,
statuses, bounded scalar facets, and structural relations. It removes prompts,
model text, unrestricted tool error bodies, secrets, capabilities, absolute host
paths, and artifact bodies. Redaction may replace a body with a content-addressed
reference; it must not rewrite identity, ordering, or evidence hashes.
