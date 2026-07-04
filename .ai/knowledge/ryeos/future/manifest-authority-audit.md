```yaml
category: ryeos/future
name: manifest-authority-audit
title: Manifest Authority Audit
entry_type: implementation_guide
version: "0.1.0"
description: A `manifest audit` action diffing granted runtime authority between two signed manifests, so re-sign campaigns review as authority deltas rather than YAML diffs.
tags:
  - manifest
  - runtime-authority
  - signing
  - review
```

# Future: Manifest Authority Audit

## Status

Deferred. Re-signing a bundle manifest re-asserts every authority it
declares; today the review artifact for that is a YAML diff. This doc
records the tool to build when that stops being reviewable.

## The gap

A signed manifest's `runtime_authority` block is a grant of real power:
bundle-event kinds, vault namespaces, item-authoring patterns the daemon
will mint capabilities from. When a manifest is regenerated and re-signed,
the meaningful question is "what authority changed" — but the artifact a
reviewer gets is a whole-file diff where authority deltas drown in
formatting, ordering, and unrelated field churn.

## Design

A core-tools action (`manifest audit <old> <new>`, or one path plus the
bundle's installed predecessor) that parses both manifests and reports the
authority delta only, in grant terms:

- bundle_events: event kinds added/removed, operations widened/narrowed;
- runtime_vault: namespaces added/removed, verbs changed;
- item_authoring: kind/namespace patterns added/removed, with a loud
  marker on any new wildcard pattern;
- provides/requires/uses kind changes.

Output ordered by risk (new wildcard authoring pattern first, removed
grants last), suitable for pasting into a review.

## Trigger

A re-sign campaign that is painful to review by hand. Seven-bundle
republishes so far have not been — the manifests are small and the deltas
obvious. If manifests grow (more families, more entries) or third-party
bundles enter review, build this before approving grants by eyeball.
