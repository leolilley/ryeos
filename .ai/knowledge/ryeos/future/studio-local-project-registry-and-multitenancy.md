<!-- ryeos:signed:2026-05-31T01:55:10Z:fcf152103d8b8381d8984e53da68d60d58d866f2cadab7b1522f52e588934238:58CrYYGQ00QcCLkTgcstvnVsLgvM7M3JYnKG3xPxkdVfLae3BS6jESQ902aoyvZ36sABKekheEqERHBGeet8Bw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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

## Implementation status as of 2026-05-31

The first local-registry implementation pass has landed.

Committed implementation milestones:

- `011775ce` — hardened local user-space YAML persistence and registry/config handlers:
  - atomic sibling-temp writes with parent-directory fsync;
  - private user-space directory/file permissions;
  - process-local YAML mutation lock;
  - schema-version checks for `projects.yaml`, `studio.yaml`, and `recent.yaml`;
  - browser-session authorization on read/write handlers;
  - safer project forget/config-update semantics.
- `94d7af51` — backend project-open session binding and tenant seam:
  - `UserSpaceResolver`, `LocalUserSpaceResolver`, and `LOCAL_PRINCIPAL_ID`;
  - `BrowserSessionStore::set_project_root`;
  - `ui.studio.projects.open` service/route;
  - project open canonicalizes the stored root, rebinds the browser session, and touches recents.
- `697c8c53` — Studio client project-open wiring:
  - Projects view list/open flow;
  - `POST /ui/api/studio/projects/open` browser effect;
  - stale project-bound pending effect invalidation after session rebinding;
  - Projects launcher/route/focused-row activation.
- `be6aab0f` — Studio client current-project registration:
  - Projects view shows `Register current project` when the current session project is not registered;
  - existing `ui.studio.projects.add` is used directly; no button-specific backend service was added;
  - successful registration refetches the project list.

Current state:

- local project registry is backend-complete for list/add/open/forget/resolve/config/recent basics;
- Studio can register the current project, list known projects, and open a project through session rebinding;
- local persistence uses `<user_root>/.ai/config/projects.yaml`, `<user_root>/.ai/config/studio.yaml`, and `<user_root>/.ai/state/studio/recent.yaml`;
- the tenancy seam exists, but handlers still resolve the synthetic local principal.

Not yet implemented:

- real principal extraction from UI/API requests;
- principal-scoped local user-space layout;
- hosted tenant storage;
- account/org/workspace membership or authorization policy;
- project-local identity.

Use this note now as both the record of what landed and the checklist for the remaining principal/multi-tenant work.

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

Implemented service surface includes operations equivalent to:

```text
ui.studio.projects.list
ui.studio.projects.add
ui.studio.projects.forget
ui.studio.projects.resolve
ui.studio.projects.open
ui.studio.recent.touch
ui.studio.recent.list
ui.studio.config.get
ui.studio.config.update
```

Do not add one endpoint per UI button. Prefer resource-style services (`projects.add`, `projects.open`, `projects.forget`) and let client actions compose those services. For example, Studio's `Register current project` UI action calls the existing `ui.studio.projects.add` service with the current session root; it does not introduce `projects.add_current`.

Use a tool instead of a service only when the operation is an executable workflow rather than daemon/UI state management. Good future tool candidates include project scanning, metadata import, health checks, migrations, or AI-generated summaries. The project registry itself is daemon-mediated user-space state, so it remains a service surface.

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

Current implementation notes:

- `projects.add` canonicalizes accessible absolute roots and updates an existing entry by canonical root;
- `projects.open` canonicalizes the stored root, requires a writable browser session, rebinds that session, and touches recents;
- read handlers require a valid browser session;
- write handlers reject read-only sessions;
- `local_id` wins for forget semantics, so missing/moved roots can still be removed.

## User-space path helper seam

Preserve future hosted compatibility by centralizing user-space path construction now.

Do not let Studio handlers hardcode `~/.ryeos` or manually concatenate user paths everywhere.

Implemented helper shape:

```rust
pub struct UserSpacePaths {
    pub root: PathBuf,
}

impl UserSpacePaths {
    pub fn config(&self, rel: impl AsRef<Path>) -> PathBuf { ... }
    pub fn state(&self, rel: impl AsRef<Path>) -> PathBuf { ... }
}
```

The app layer now also exposes a resolver seam:

```rust
pub trait UserSpaceResolver {
    fn resolve(&self, principal_id: &str) -> Result<UserSpacePaths>;
}

pub struct LocalUserSpaceResolver;
pub const LOCAL_PRINCIPAL_ID: &str = "local";
```

Studio handlers currently call this seam through `resolve_user_space_paths(...)` and still pass the synthetic local principal. That is the intended local-mode transitional state.

Use stable logical relative paths:

```text
config/projects.yaml
config/studio.yaml
state/studio/recent.yaml
state/projects/index.yaml
```

Next, this should become a principal-aware resolver without changing every Studio handler:

```rust
pub trait UserSpaceResolver {
    fn resolve(&self, principal_id: &str) -> Result<UserSpacePaths>;
}
```

Do not implement hosted tenant storage now.

Remaining Level-1 principal work:

1. Decide the local principal-scoped physical layout.
2. Add a principal extraction function for Studio/UI handler contexts.
3. Replace `LOCAL_PRINCIPAL_ID` at the handler boundary with the extracted local principal.
4. Preserve backwards compatibility for the current singleton local files, either by treating the local operator as the singleton principal or by adding a one-time migration/copy strategy.
5. Add tests proving two distinct principals get separate `projects.yaml`, `studio.yaml`, and `recent.yaml` data through the same service handlers.

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

## Completed local implementation boundary

The first implementation pass focused on and completed:

1. Define `UserSpacePaths` and a resolver seam for user config/state paths.
2. Add atomic read/write helpers for:
   - `config/projects.yaml`;
   - `config/studio.yaml`;
   - `state/studio/recent.yaml`.
3. Add Studio/UI services for project list/add/forget/resolve/open and config/recent reads/writes.
4. Wire Studio startup/project picker/register-current/open flow to those services.
5. Keep browser file access rooted in a canonical selected project.
6. Do not add project-local identity yet.
7. Do not implement hosted multi-tenancy yet.

## Remaining roadmap to full principal support

### Level 1: principal-aware local user space

Goal: prove local user-space isolation without implementing hosted accounts or tenant storage.

Work items:

1. Choose the local principal ID source.
   - Candidate: the verified browser/CLI key fingerprint already available through signed launch/session creation.
   - Local compatibility option: keep the current singleton local root for the default local operator, and only use scoped roots when a non-default principal is present.
2. Add principal information to browser session state.
   - Store a `principal_id` or equivalent verified caller identity on `BrowserSession`.
   - Ensure session minting has enough signed-request context to set it.
3. Add typed principal extraction for handlers.
   - Avoid loose JSON `_caller_fingerprint`-style fields.
   - Prefer a typed context/session accessor used by all Studio user-space handlers.
4. Route `resolve_user_space_paths(ctx)` through that principal.
   - Remove direct `LOCAL_PRINCIPAL_ID` usage from the handler boundary.
   - Keep `LocalUserSpaceResolver` as the local filesystem resolver.
5. Add isolation tests.
   - Same daemon, two principals, separate project registries.
   - Separate `studio.yaml` preferences.
   - Separate `recent.yaml` recents.
   - Existing singleton local behavior still works.

This level should not add orgs, quotas, billing, per-principal vault partitions, or hosted storage.

### Level 2: authenticated principal sessions

Goal: make browser/API sessions explicitly bound to a real verified principal.

Work items:

1. Define the local UI session principal contract.
   - Which key signs launch/mint requests?
   - Which principal does that key represent?
   - How does read-only mode interact with principal identity?
2. Extend `BrowserSession` and `BrowserSessionStore` with principal metadata.
3. Thread principal metadata into `HandlerContext` or an adjacent typed accessor.
4. Audit all UI handlers that mutate user/node/project state and decide which identity they require.
5. Add tests for expired/invalid/read-only sessions and principal mismatch behavior.

### Level 3: hosted multi-tenancy

Goal: support multiple unrelated principals sharing hosted infrastructure.

Work items:

1. Key-based login challenge/verify flow.
2. Tenant-backed logical user-space storage behind `UserSpaceResolver`.
3. Account/org/workspace membership and authorization policy.
4. Per-principal or per-tenant vault partitioning if secrets are hosted.
5. Quotas, audit trails, revocation, logout, and recovery flows.
6. Project identity only if sync/federation/attestation needs path-independent identity.

Do not begin Level 3 until there is a concrete hosted product requirement.
