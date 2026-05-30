<!-- rye:signed:2026-05-30T04:39:09Z:0668ae4257fbe5688b500a1157ad8ac5198382509ae9c72cd645833e518f5a7a:e841rQssGRrzuBeI710Qifkd42zjk3tsSF5xZAfSg5Dt6uIk9vABZLz8yJLAFR4a2S5mMMb2nwpQFaoO7lUuCQ:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: studio-local-project-registry-and-multitenancy
title: Studio Local Project Registry and Future Multi-Tenancy
entry_type: implementation_guide
version: "0.1.0"
author: amp
created_at: 2026-05-30T00:00:00Z
description: Layout and implementation direction for local Studio project discovery in regular RyeOS installs, with deferred hosted multi-tenant compatibility constraints.
tags:
  - studio
  - project-registry
  - user-space
  - node-space
  - multi-tenancy
  - future-work
```

# Studio Local Project Registry and Future Multi-Tenancy

## Purpose

This note records the agreed direction for adding local project discovery and user-facing Studio state to RyeOS without over-splitting the regular install or prematurely implementing hosted multi-tenancy.

The regular RyeOS install should feel like a complete local OS:

- node identity and daemon operation;
- user/principal signing key and user-space config;
- Studio UI;
- known local projects;
- recent/open project state;
- project-scoped files/items/threads/remotes views.

The multi-tenant/cloud layer remains a future extension. Do not make local project discovery a separate product/bundle layer just to preserve future hosting optionality.

## Decision summary

Regular local RyeOS should include Studio and local project support by default. The only major optional future layer is hosted/multi-tenant/cloud behavior.

Do not introduce a new `.ai/user/` directory for this work. Extend the existing user-space `.ai/config` and `.ai/state` split:

```text
<user_root>/.ai/config/  durable user/principal-local facts
<user_root>/.ai/state/   RyeOS/Studio runtime, projection, recent, and cache state
```

Keep `.ai/node/` strictly node/daemon-owned:

```text
<system_space>/.ai/node/ daemon identity, auth, bundle registrations, routes, verbs, aliases, node policies
```

Defer project-local identity until there is a clear trigger. Start with a user-space known-project registry only.

If project-local identity is later needed, prefer:

```text
<project_root>/.ai/config/project.yaml
```

not:

```text
<project_root>/.ai/project.yaml
```

## Ownership model

Use this as the implementation invariant:

```text
system/node space
  answers: what is this daemon/node, what does it serve, who can call it?

user/principal space
  answers: what does this local RyeOS principal know, prefer, and carry between installs?

project space
  answers: what AI items/config belong to this project checkout?

runtime state
  answers: what did RyeOS recently observe, cache, index, or run?
```

This keeps local UX state out of node authority and avoids turning node internals into user preferences.

## Concrete layout

### System/node space

`<system_space>/.ai/node/` remains node-owned.

Current and near-term layout:

```text
<system_space>/.ai/node/
  config.yaml
  identity/
    private_key.pem
    public_identity.json
  auth/
    authorized_keys/
      <fingerprint>.toml
  bundles/
  routes/
  verbs/
  aliases/
```

Future node-owned additions may include:

```text
<system_space>/.ai/node/
  peers/
  policies/
```

Do not put the local Studio project registry, recent projects, Studio preferences, or profile data under `.ai/node/`.

### User/principal config

Durable user/principal-local facts live under:

```text
<user_root>/.ai/config/
```

Target layout:

```text
<user_root>/.ai/config/
  keys/
    signing/
      private_key.pem
  projects.yaml
  studio.yaml
  profile.yaml        # optional/later
  remotes/
    remotes.yaml
```

`config` is for user-intent and durable local facts. It may be edited, backed up, or carried to another local install.

### User/principal state

RyeOS/Studio-maintained runtime and projection data lives under:

```text
<user_root>/.ai/state/
```

Target layout:

```text
<user_root>/.ai/state/
  studio/
    recent.yaml
    sessions/         # only if safe, expiring, and non-secret
  projects/
    index.yaml        # optional derived cache/index
```

`state` is disposable and rebuildable. Deleting it should lose only recents, caches, projections, or recoverable runtime/session metadata — not user intent.

### Project space

Existing project-local AI content remains under the project `.ai` directory:

```text
<project_root>/.ai/
  config/
  directives/
  tools/
  knowledge/
```

No project-local identity file is required for the first local Studio project registry implementation.

If a project-local identity document is later needed, use:

```text
<project_root>/.ai/config/project.yaml
```

Keep absolute local paths out of any signed project identity payload. Paths are locators and belong in the user-space project registry.

## `projects.yaml` contract

`<user_root>/.ai/config/projects.yaml` is the durable known-project registry for the local principal/user space.

It should represent user intent:

- projects the user explicitly added;
- projects Studio auto-added because the user explicitly opened them;
- durable display metadata such as name/tags;
- local root locator for the current machine.

It should not become a passive scan cache.

Initial shape:

```yaml
version: 1
projects:
  - local_id: "prj_01h..."
    name: "ryeos-next"
    root: "/home/leo/projects/ryeos-next"
    added_at: "2026-05-30T00:00:00Z"
    tags: []
```

Field guidance:

| Field | Meaning |
|---|---|
| `version` | Schema version. Start at `1`. |
| `local_id` | Opaque local registry ID. Not a global/signed project identity. |
| `name` | User-facing project label. |
| `root` | Local project root path for this machine/user space. |
| `added_at` | When this local registry entry was created. |
| `tags` | Optional durable user labels. |

Keep transient or derived data out of this file:

- last opened timestamps;
- recent files;
- scan status;
- health/check status;
- file counts;
- detected languages;
- indexed item summaries.

Put that under user state instead, for example:

```text
<user_root>/.ai/state/projects/index.yaml
<user_root>/.ai/state/studio/recent.yaml
```

## `studio.yaml` contract

`<user_root>/.ai/config/studio.yaml` stores durable Studio preferences.

Initial possible shape:

```yaml
version: 1
theme: system
landing_view: projects
default_open_mode: normal
```

Use this file for stable preferences such as:

- theme;
- preferred landing view;
- default open/read-only behavior;
- durable layout preferences;
- user-visible feature toggles.

Do not store recent projects, transient tabs, daemon session tokens, or scan results in `studio.yaml`.

## Studio state contract

Use:

```text
<user_root>/.ai/state/studio/recent.yaml
```

for recent, rebuildable Studio state, such as:

```yaml
version: 1
recent_projects:
  - local_id: "prj_01h..."
    opened_at: "2026-05-30T00:00:00Z"
```

Use:

```text
<user_root>/.ai/state/studio/sessions/
```

only if persistent Studio sessions become necessary.

Guardrail: do not persist powerful browser auth tokens or long-lived launch/session secrets without an explicit auth lifecycle design:

- TTL;
- revocation;
- logout semantics;
- file permissions;
- recovery behavior after daemon restart.

The current in-memory browser session model is safer until session restore is a real requirement.

## Project-local identity: deferred

Do not require project-local identity for the first Studio project registry implementation.

The local registry can answer the immediate UI question:

> Which projects does this local RyeOS user space know about, and where are they on disk?

Introduce project-local identity only when one of these triggers appears:

- stable identity across machines is needed;
- cloud sync must distinguish “same project at moved path” from “different project with same name”;
- project-specific signed remotes or policies are needed;
- collaboration requires project-level ownership/attestation;
- admission/federation needs project identity independent of local path.

When introduced, prefer:

```text
<project_root>/.ai/config/project.yaml
```

Possible future shape:

```yaml
version: 1
project_id: "proj_01h..."
name: "ryeos-next"
created_at: "2026-05-30T00:00:00Z"
owner_principal: "fp:<fingerprint>"
```

If signed later, do not include absolute local paths in the signed payload.

## API/service direction

Expose the local registry through Studio/UI-oriented daemon services rather than making the browser read files directly.

Initial service surface should include operations equivalent to:

```text
studio.projects.list
studio.projects.add
studio.projects.forget
studio.projects.resolve
studio.projects.touch_recent
studio.config.get
studio.config.update
studio.recent.list
```

Naming guidance:

- Use `studio` or `ui` for new names.
- Do not introduce new `cockpit` names.
- Existing compatibility names may remain temporarily if needed, but should not be the source of truth for new contracts.

Behavioral requirements:

- canonicalize project roots when registering;
- reject non-absolute roots unless a CLI/front-end resolver canonicalizes first;
- handle missing/moved projects gracefully;
- write YAML atomically;
- avoid passive home-directory scanning by default;
- keep file APIs scoped to a selected/registered project root.

## User-space path helper seam

Preserve future hosted compatibility by centralizing user-space path construction now.

Do not let Studio handlers hardcode `~/.ryeos` or manually concatenate user paths everywhere.

Near-term helper shape:

```rust
pub struct UserSpacePaths {
    pub root: PathBuf,
}

impl UserSpacePaths {
    pub fn config(&self, rel: impl AsRef<Path>) -> PathBuf { ... }
    pub fn state(&self, rel: impl AsRef<Path>) -> PathBuf { ... }
}
```

Use stable logical relative paths:

```text
config/projects.yaml
config/studio.yaml
state/studio/recent.yaml
state/projects/index.yaml
```

Later, this can become a principal-aware resolver without changing every Studio handler:

```rust
pub trait UserSpaceResolver {
    fn resolve(&self, principal_id: &str) -> Result<UserSpacePaths>;
}
```

Do not implement hosted tenant storage now.

## Normal install scope

Regular RyeOS local install should include:

- core/standard execution substrate;
- node identity and user signing key support;
- Studio UI;
- local project registry support;
- Studio config/recent state support.

Do not split local project registry into a separate user-visible “workspace” product layer.

Transitional implementation may keep physically separate internal bundle directories if that reduces churn, but normal install should always include/register Studio and its local project support.

The only major optional future layer is hosted/multi-tenant/cloud behavior.

## Deferred hosted/multi-tenant layer

Hosted multi-tenancy should be deferred until a real product requirement appears, such as:

- multiple unrelated principals share one daemon/node concurrently;
- browser login must work without a trusted local launcher;
- user spaces need non-filesystem or tenant-backed storage;
- per-principal vault partitioning is required;
- quotas/billing/audit require tenant isolation;
- cloud sync needs durable project identity and principal-scoped policy;
- browser sessions need server-side persistence across daemon restarts.

When triggered, hosted mode should virtualize the same logical user-space files behind a resolver, not invent a different local contract:

```text
tenant:<principal>/config/projects.yaml
tenant:<principal>/config/studio.yaml
tenant:<principal>/state/studio/recent.yaml
```

Physical hosted layout is intentionally deferred. A possible future shape:

```text
<system_space>/.ai/tenants/
  <principal-id>/
    .ai/
      config/
        projects.yaml
        studio.yaml
      state/
        studio/
          recent.yaml
```

or:

```text
<system_space>/.ai/node/tenants/
  <principal-id>/
    user-space/
      .ai/
        config/
        state/
```

Do not choose this physical layout now. The important near-term constraint is that code uses logical user-space paths through a helper/resolver seam.

## Future hosted auth direction

Future hosted RyeOS should use key-based principal login rather than making email/password the primary identity root.

Reserve this contract direction:

```text
POST /auth/challenge
POST /auth/verify
```

The client signs a nonce/audience challenge with a RyeOS principal key. The server verifies the key and mints a UI/API session bound to the verified principal and scopes.

This belongs to the future hosted/cloud layer, not the immediate local Studio registry implementation.

## Guardrails

### Do not pollute `.ai/node`

`.ai/node` is for node/daemon authority. Do not place local Studio preferences, known projects, recent projects, or user profile data there.

### Keep config from becoming a cache

If deleting a file loses user intent, it belongs in `config`. If deleting it only causes rebuild/recent-history/cache loss, it belongs in `state`.

### Keep paths local

Absolute local paths may appear in local `config/projects.yaml`. Do not put absolute paths in signed project identity, distributed refs, or admission objects.

### Keep Studio file access scoped

Studio file APIs should operate on canonical selected/registered project roots, not arbitrary browser-provided absolute paths.

### Keep session persistence conservative

Prefer in-memory browser sessions until persistent sessions have a complete security lifecycle.

### Avoid new `cockpit` naming

Use `studio` / `ui` for new services, schemas, routes, docs, and user-facing labels.

### Avoid premature tenant design

Use a path helper/resolver seam. Do not implement tenant directories, per-principal vaults, orgs, quotas, or billing until hosted mode requires them.

## Immediate implementation boundary

The first implementation pass should focus on:

1. Define `UserSpacePaths` or equivalent helper for user config/state paths.
2. Add read/write helpers for:
   - `config/projects.yaml`;
   - `config/studio.yaml`;
   - `state/studio/recent.yaml`.
3. Add Studio/UI services for project list/add/forget/resolve and config/recent reads.
4. Wire Studio startup/project picker to those services.
5. Keep browser file access rooted in a canonical selected project.
6. Do not add project-local identity yet.
7. Do not implement hosted multi-tenancy yet.

After local registry and Studio wiring are stable, revisit project-local identity only if sync/federation/attestation work needs path-independent project identity.
