<!-- ryeos:signed:2026-06-10T01:06:25Z:80b423374f74d962efa3d2c67a4a33c8f1f4d0aaf2e7e0fe486747bfb391f17f:SNcR6YwW6rjsVizSe7TcBRh32thP/wdBBBFva54rRsjv+DpcXhg2V0ciXlrGDgjPgv1WxhGhwfTA2/7pSngaCg==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
# Future: Data-driven scope profiles for authorized-key grants

## Status

Deferred design note. This records the preferred RyeOS-style replacement for
using wildcard authorized-key scopes in deployment scripts and backend-client
authorization.

The immediate production issue that motivated this note was a hosted app
entrypoint attempting to authorize a backend client with wildcard scopes after
`ryeos init` succeeded. The shared authorized-key writer correctly rejected the
grant because persistent wildcard delegation is only allowed for local operator
bootstrap.

## Problem

Persistent authorized-key grants are node authority. A grant such as:

```toml
scopes = ["*"]
```

turns a client key into a skeleton key for every current and future capability
the node recognizes. That is too broad for app backends, remotes, CI jobs, and
admission flows.

At the same time, requiring every deployment script to hand-maintain long lists
of concrete capabilities is poor ergonomics and does not fit RyeOS's signed,
data-driven model.

The missing concept is a signed scope profile: bundle or node data that names a
role-like authorization profile and expands it into explicit concrete scopes at
grant time.

## Target invariant

Wildcards/selectors may appear in signed configuration and runtime policy, but
authorized-key files store only explicit materialized capabilities, except for
the local bootstrap operator key.

```text
signed scope profile
  -> validate profile and selectors
  -> expand against verified node/bundle inventory
  -> reject any materialized wildcard
  -> write authorized-key TOML with explicit scopes only
```

This keeps operator ergonomics without giving already-issued client keys
automatic access to capabilities introduced by later bundle updates.

## Proposed node-config section

Add a signed node-config section:

```text
.ai/node/scope_profiles/*.yaml
```

The section should be loadable from the system/state root and effective bundle
roots, like routes and commands. Profiles contributed by bundles are signed
bundle content; profiles contributed by the system/state root are operator/node
state.

Example profile:

```yaml
category: "auth"
section: "scope_profiles"
schema_version: "1.0.0"

id: "tv-tracker/backend"
description: "Capabilities for the TV Tracker backend service account"

grants:
  - cap: "ryeos.execute.service.objects.has"
  - cap: "ryeos.execute.service.objects.put"
  - cap: "ryeos.execute.service.objects.get"
  - cap: "ryeos.execute.service.push.head"

  - select:
      verb: "execute"
      kind: "tool"
      path_prefix: "apps/tv-tracker/api/"
```

The `cap` form is already materialized. The `select` form is a data-driven
selector that expands to concrete caps for verified items matching the selector.
The selector itself is not stored in the authorized-key grant.

## CLI shape

Extend local authorization tooling so operators can choose either explicit
scopes or a profile:

```bash
ryeos-core-tools authorize-client \
  --system-space-dir /data/core \
  --public-key "$TV_TRACKER_BACKEND_PUBLIC_KEY" \
  --label tv-tracker-backend \
  --scope-profile tv-tracker/backend
```

`--scopes` and `--scope-profile` should be mutually exclusive. JSON input should
support the same field:

```json
{
  "system_space_dir": "/data/core",
  "public_key": "ed25519:...",
  "label": "tv-tracker-backend",
  "scope_profile": "tv-tracker/backend"
}
```

The command should still call the existing authorized-key writer with wildcard
rejection enabled. Scope profiles are macro expansion, not a new privilege path.

## HTTP/API shape

The authenticated `/authorize-key` handler can support the same request shape:

```json
{
  "public_key": "ed25519:...",
  "label": "tv-tracker-backend",
  "scope_profile": "tv-tracker/backend"
}
```

Profile expansion must not bypass delegation checks. After expansion, each
materialized scope must still be permitted by the caller's scopes. A wildcard
caller may grant explicit materialized scopes, but may not grant wildcard
scopes.

## Validation rules

Profile loading should fail closed:

- node-config item must be signed and trusted;
- file must live under `.ai/node/scope_profiles/` and declare
  `section: "scope_profiles"`;
- `category` must be `"auth"`;
- `schema_version` must be supported;
- `id` must be non-empty and globally unique in the effective node-config
  snapshot;
- `grants` must be non-empty;
- `cap` entries must pass canonical scope grammar;
- `cap` entries must not contain `*`;
- selector expansion must produce at least one concrete cap unless the selector
  explicitly opts into allowing empty expansion;
- expanded caps must pass canonical scope grammar;
- expanded caps must not contain `*`;
- duplicate expanded caps are sorted and deduplicated before writing.

The first implementation may support only `cap` entries and reject `select`
with a clear error. The schema should still reserve `select` because selectors
are the data-driven replacement for wildcard grants.

## Expansion semantics

A selector is evaluated against verified node/bundle inventory, not raw files.
For the example:

```yaml
- select:
    verb: "execute"
    kind: "tool"
    path_prefix: "apps/tv-tracker/api/"
```

If the verified inventory contains:

```text
tool:apps/tv-tracker/api/sync
tool:apps/tv-tracker/api/oauth-callback
tool:apps/tv-tracker/api/refresh
```

the materialized grant is:

```toml
scopes = [
  "ryeos.execute.tool.apps/tv-tracker/api/sync",
  "ryeos.execute.tool.apps/tv-tracker/api/oauth-callback",
  "ryeos.execute.tool.apps/tv-tracker/api/refresh"
]
```

If a later bundle release adds another matching tool, existing authorized-key
TOMLs do not silently gain it. Operators must re-run profile authorization to
materialize the new scope.

## Rust implementation outline

Add a section handler:

```text
crates/daemon/ryeos-app/src/node_config/sections/scope_profile.rs
```

Core types:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeProfileRecord {
    pub category: String,
    pub section: String,
    pub schema_version: String,
    pub id: String,
    pub description: Option<String>,
    pub grants: Vec<ScopeProfileGrant>,
    #[serde(skip)]
    pub source_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum ScopeProfileGrant {
    Cap { cap: String },
    Select { select: ScopeProfileSelector },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeProfileSelector {
    pub verb: String,
    pub kind: String,
    pub path_prefix: String,
}
```

Register the section in `SectionTable::new()`:

```rust
sections.insert(
    "scope_profiles",
    Box::new(sections::scope_profile::ScopeProfileSection),
);
```

Extend `NodeConfigSnapshot`:

```rust
pub scope_profiles: Vec<ScopeProfileRecord>,
```

Extend the full loader with a `scope_profiles` branch, set `source_file`, and
check profile id uniqueness after scanning.

Add a shared resolver usable by core-tools and API handlers:

```rust
pub fn resolve_scope_profile(
    system_space_dir: &Path,
    profile_id: &str,
) -> Result<Vec<String>> {
    let snapshot = load_verified_node_config(system_space_dir)?;
    let profile = find_unique_profile(&snapshot, profile_id)?;
    let scopes = expand_profile(profile, &snapshot)?;
    validate_materialized_scopes(profile_id, scopes)
}
```

The resolver should return explicit scopes only. It should never return `*`,
`ryeos.execute.*`, or any other wildcard-containing pattern.

## Relationship to directives, graphs, and runtime permissions

This design does not ban wildcard capability patterns from RyeOS. Wildcards are
still useful inside signed runtime policy and item permissions, for example:

```text
ryeos.execute.tool.apps/tv-tracker/api/*
ryeos.execute.directive.*
```

The restriction is specifically on persistent authorized-key grants to external
principals. Directive and graph permission composition controls what an already
authorized execution may do. Authorized-key scopes control what an external key
may ask the node to do in the first place.

## Non-goals

- Do not add a deployment-friendly `--allow-wildcard` path for backend clients.
- Do not store selectors in authorized-key TOML.
- Do not let profile selection bypass caller delegation/subset checks.
- Do not let bundle-authored profiles grant authority by themselves; profiles
  are only expanded when an authorized operator/caller creates a grant.
- Do not make old grants automatically expand when bundles change.

## Migration path

1. Keep existing wildcard rejection in the shared authorized-key writer.
2. Add `scope_profiles` node-config loading and uniqueness checks.
3. Add profile expansion for `cap` grants.
4. Add `--scope-profile` to `ryeos-core-tools authorize-client`.
5. Update hosted app entrypoints to use scope profiles instead of `--scopes "*"`.
6. Add selector expansion once a clean verified inventory API is available.
7. Optionally add `scope_profile` support to the HTTP `/authorize-key` handler.
