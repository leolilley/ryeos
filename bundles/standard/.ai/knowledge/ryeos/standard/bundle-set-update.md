# Stopped-node bundle-set update

`ryeos bundle set-update` is the consumer operation for independently published
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
the active selection hash, installed-set digest, and action counts.

There is intentionally no mutable-name fallback, `latest` alias, legacy ref,
or live single-bundle install path. Core is substrate-owned and keep-only.
