<!-- ryeos:signed:2026-11-13T00:38:27Z:1a9dcc934ea43519f64e397af43a81b07ef75091c21bb815b1b8a8f86f55ed42:IT9NlT+v1/MIVy87h3K09NV95f32mUH1l8r6ifZ7QtgYYeoM28VIn3+0O7kTCJiP8WgB+O4qIgjiq9XhLw+XBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# Stopped-node bundle-set update

`ryeos node bundle-set update` is the consumer operation for independently published
bundles. It consumes one named, operator-configured RyeOS remote and a catalog
namespace. Selection is either a set/channel plus an optional expected catalog
publication-attestation hash, or all five hashes in the resolved immutable
coordinate.

The operation resolves the authenticated catalog head, transfers the exact
catalog, set, generation, attestation, manifest, tree, and blob closure, and
verifies it against the consumer's current bundle-publication policy and trust
epoch. It then plans the complete add/replace/remove/keep transition and admits
the whole prospective set. A missing bundle is a removal; this is not an
incremental overlay.

The operator supplies an absolute private-key path explicitly. The key signs
the prospective whole-init completion offline and is not made available to the
daemon, bundle publisher, remote, or installed bundle code. After all checks,
the command acquires stopped-node and registry locks and calls the journalled
exact-set transaction. Its result reports the immutable catalog/set coordinate,
the catalog publication sequence, the active selection hash, installed-set
digest, and action counts.

## Catalog rollback protection

The node keeps a catalog high-water for each catalog namespace: the sequence
and publication-attestation hash of the highest catalog publication it has
activated a selection from. It is stored in the active bundle-selection record
(`.ai/node/active-bundle-selection.json`, schema 2) and written in the same
transaction as the selection. It never moves down.

Every update compares the served catalog publication with that high-water:

- A higher sequence, or the same publication again, is admitted and becomes
  the high-water.
- A lower sequence is refused with `catalog_rollback_refused`. The refusal
  names both sequences. The cure is a catalog source that serves the current
  head, or a deliberate exact rollback (below).
- The same sequence with a different publication is refused with
  `catalog_equivocation_refused`: the publisher signed two publications for
  one sequence. No flag admits it; freeze the namespace in bundle-publication
  policy and review the publisher's trust.

A downgrade is sometimes the fix. To move back to an older publication, name
its exact five-hash coordinate in the request and pass
`--allow-catalog-rollback` on the command line; the request document cannot
carry the permission, and a set/channel selection never rolls back. The
high-water stays where it was. The rollback (namespace, high-water and selected
positions) is recorded in the transaction journal and in the operator-signed
deployment authorization, which is retained in the node's CAS, and the result
reports it.

The first update on a node with no active selection records the served
publication as the high-water. An active-selection record of any other schema
is refused with `active_bundle_selection_schema_refused`; with the node
stopped, remove that file and the next update re-adopts the installed set and
records the high-water.

There is intentionally no mutable-name fallback, `latest` alias, legacy ref,
or live single-bundle install path. Core is substrate-owned and keep-only.
