<!-- rye:signed:2026-06-03T03:31:26Z:90311c108d8b9def06dc694970c7156f7dc4931f43d274cfba6c4711467c701c:iPhzqfEROHLyZ7m3l5yZYyKNxCpo0n4qFF6EpusBHVFNjvQ9YAAyvCKmmxC1FAy1-GhRvIsLwEbTLNGZfwDLAw:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: ryeos-ui-local-project-registry-and-multitenancy
title: Future Principal Spaces, Project/World Registries, and Hosted Boundaries
entry_type: implementation_guide
version: "0.2.0"
author: amp
created_at: 2026-05-30T00:00:00Z
updated_at: 2026-06-03T00:00:00Z
description: Future boundary design for principal spaces, project/world registries, hosted node boundaries, and multi-tenant constraints beyond the current local/hosted-principal RyeOS UI substrate.
tags:
  - ryeos-ui
  - cockpit
  - project-registry
  - world-registry
  - principal-spaces
  - hosted-node
  - multi-tenancy
  - future-work
```

# Future Principal Spaces, Project/World Registries, and Hosted Boundaries

## Purpose

This note describes the future boundary model beyond the current local/hosted-principal RyeOS UI substrate.

The goal is to grow from local project registration into Cockpit-managed principal spaces, hosted project/world registries, portal discovery, and eventually multi-principal hosted operation without turning hosted providers or app-local auth into RyeOS identity authorities.

## Baseline assumed

This future work assumes the current substrate already provides:

- local principal/user-space state;
- principal-aware hosted user-space seams;
- RyeOS UI/Cockpit project registration and launch flows;
- hosted-node as the bundle for always-on hosted RyeOS behavior;
- central-auth as optional app-local realm auth.

Those are not restated here as implementation history. This doc is about what remains ahead.

## Ownership model

Keep these boundaries stable:

```text
node space
  answers: what is this daemon/node, what does it serve, who can call it?

principal space
  answers: what does this RyeOS principal know, prefer, own, pin, and carry?

project space
  answers: what AI items/config belong to this project checkout or project object?

world space
  answers: what signed state, frame policy, portals, dimensions, and heads define this world?

runtime state
  answers: what did RyeOS recently observe, cache, index, run, or project?
```

Do not put user preferences, project registries, recent portals, or personal profile state under node authority. Do not put node identity, auth grants, route registrations, or hosted admission policy under principal preferences.

## Future project/world registry

The current local project registry answers a local question:

> Which projects does this local principal know about, and where are they on this machine?

The future registry should answer richer questions:

- Which projects/worlds does this principal own?
- Which projects/worlds are pinned, followed, hosted, mirrored, or recently opened?
- Which portals point into those worlds?
- Which hosted nodes make them reachable?
- Which project/world policies govern collaboration?
- Which local paths correspond to portable project/world identities on this machine?

Future registry entries should distinguish:

| Field type | Meaning |
|---|---|
| Local locator | machine-specific path or launch URL |
| Portable identity | signed project/world object, ref, or policy hash |
| Display metadata | local label, tags, ordering, pinned state |
| Hosted presence | node descriptors/portals that keep it reachable |
| Derived state | caches, recents, health, indexes, sync status |

Absolute local paths must stay local. They are locators, not portable identity.

## Project and world identity

Future portable identity should come from signed objects, not local paths.

Possible objects:

```text
project-policy/v1
world-policy/v1
portal/v1
frame-policy/v1
signed-ref-update/v1
node-descriptor/v1
```

Use local registry entries to map those portable identities to local roots, checked-out workspaces, cached closures, or hosted portals.

Do not make a project's absolute path part of signed project identity. A project can move machines. A world can be hosted by many nodes. A portal can be mirrored.

## Principal spaces

A principal space is the durable local/hosted state associated with a RyeOS principal.

It may eventually contain:

- known projects;
- known worlds;
- pinned portals;
- trusted node descriptors;
- profile/display metadata;
- local preferences;
- remotes/reachability;
- object pins;
- hosted leases;
- recent activity;
- app-local realm linkages where useful.

Principal space should remain portable and inspectable. If a provider hosts it, the provider stores/syncs signed state; it does not become the source of identity.

## Hosted boundaries

A hosted node can make a principal's portals and worlds reachable while their local machine is offline.

Hosted node responsibilities may include:

- serving object closures;
- hosting portal entrypoints;
- publishing accepted heads;
- running subscriptions;
- running admitted jobs;
- maintaining indexes/search;
- mirroring pinned worlds;
- storing hosted principal-space state.

Hosted node responsibilities should not include:

- global RyeOS identity;
- central execution authority;
- hidden ownership of world truth;
- uninspectable provider-only project state.

A hosted node is reachability and presence. Authority remains in signed objects, pinned descriptors, node-local grants, and policy.

## central-auth boundary

`central-auth` is app-local realm auth.

It may be used by a portal app to decide whether a human browser session can enter or interact with that app realm.

It must not be treated as:

- RyeOS global login;
- RyeOS principal identity;
- protocol execution authority;
- hosted-node trust root;
- world ownership source.

Useful distinction:

```text
central-auth: can this browser visitor enter this app realm?
RyeOS principal: who signed this world/policy/execution object?
node-local grant: will this node admit or execute this request?
world policy: is this change accepted into this world?
```

## Multi-tenancy path

True hosted multi-tenancy should be treated as advanced work.

Prefer this near-term shape:

```text
one hosted node / isolated hosted space per operator or small trust boundary
```

before this shape:

```text
many unrelated principals sharing one daemon with strong isolation
```

Shared-daemon hosting requires:

- principal-scoped storage;
- principal-scoped vault reads;
- quotas and accounting;
- durable audit logs;
- strict auth scope enforcement;
- replay protection persistence;
- per-principal object ownership/GC;
- policy-aware admission/execution;
- safe browser/session lifecycle;
- clear tenant data export/deletion semantics.

Do not accidentally ship shared-daemon multi-tenancy because it was convenient for the UI.

## Vault and secrets

Multi-principal hosted operation eventually needs principal-scoped vault behavior.

Future vault reads should answer:

```text
which principal is executing?
which declared secret does this item request?
which vault entry grants that principal access?
which node is decrypting it?
which policy allowed this execution?
```

Until that exists, public or unrelated-tenant hosted execution should avoid sharing one secret store across principals.

## Remote execution and jobs

Future hosted project/world registries will need durable jobs:

- validation jobs;
- portal launch jobs;
- simulation ticks;
- directive/entity runs;
- object closure sync;
- hosted build/render jobs;
- admission/moderation workflows.

These should become signed/durable job and result objects with visible provenance, not hidden provider tasks.

Cockpit should eventually show:

- initiating principal;
- target node;
- input object hashes;
- policy/grant used;
- job status;
- output object hashes;
- result signature;
- event stream/replay.

## Registry and discovery stance

Registries are indexes, not roots of truth.

They can provide:

- search;
- human-friendly names;
- featured portals;
- world indexes;
- hosted availability;
- namespace convenience;
- cached metadata.

But verification remains local:

- object hashes;
- signatures;
- signed policies;
- signed node descriptors;
- pinned trust decisions.

Human-friendly names are social/convenience objects. Self-certifying identifiers remain the cryptographic base.

## Cockpit UX future

The Cockpit should eventually show principal space as a living map:

- local projects;
- portable projects/worlds;
- pinned portals;
- hosted nodes;
- trust pins;
- app realms;
- recent worlds;
- sync status;
- running jobs;
- object availability;
- unresolved policy/trust decisions.

The user should understand the difference between:

- local-only project;
- portable signed project/world;
- hosted portal;
- app-local realm;
- trusted hosted node;
- mirrored object closure;
- admitted world head.

## Trigger list

Implement the advanced pieces when these pressures appear:

| Trigger | Future work |
|---|---|
| Same project/world moves across machines | signed project/world identity and local locator mapping |
| Portals need always-on reachability | hosted node descriptors and hosted portal objects |
| Visitors enter a portal without RyeOS keys | app-local realm auth boundary |
| Builders sign world changes | project/world policy as signed CAS |
| Multiple unrelated users share a daemon | true multi-tenancy isolation |
| Secrets are used in hosted jobs | principal-scoped vault reads |
| Long jobs outlive requests | durable signed job/result objects |
| Worlds need mirrors/discovery | registries as indexes and peer sync |
| Hosted fleets become operationally complex | fleet enrollment/hardware attestation as advanced path |

## Guiding rule

Keep local paths local, app auth app-local, hosted nodes non-authoritative, and RyeOS authority signed.
