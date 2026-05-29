<!-- ryeos:signed:2026-05-29T05:16:43Z:7c7855aea08a53e6faf73206f962937bae81e352c55e13a29838b8b89c9cd7a6:erYyl7A5++fz3c09rQ42/p4NSyGJFMhl323dApDqzuPsykQPgzdb0NoeFrLDpt/l+AY6YouzPRZXnVdOn0/pBQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: future
tags: [ryeos, security, env, secrets, architecture]
version: "1.0.0"
description: Future design for centralizing RyeOS subprocess environment construction.
---

# Future: central EnvContractBuilder for RyeOS subprocess env

## Context

RyeOS currently composes subprocess environment variables across several paths:

- `ryeos-app::process::build_spawn_env` builds the base allowlisted env and overlays declared secrets.
- `ryeos-app::thread_lifecycle::spawn_item` adds daemon callback/runtime env for plan-node subprocesses.
- `ryeos-executor::execution::launch` builds native runtime launch env, provider auth bindings, protocol env, and launch-envelope env.
- Runtime descriptor `env_config.env` can interpolate allowlisted host env via `RYEOS_TOOL_ENV_PASSTHROUGH`.
- `required_secrets` are resolved through vault / daemon host env / `.env` and injected into subprocess env.

The current scoped security fix keeps this architecture and adds protected env-name validation at the critical boundaries. That is enough for the present issue, but the long-term architecture should centralize env construction so policy is easier to reason about and audit.

## Goal

Create a single `EnvContractBuilder` that owns every source of subprocess env and enforces a single policy for precedence, collision handling, protected names, and diagnostics.

The builder should make these invariants obvious:

1. No subprocess receives blanket daemon host env.
2. Only explicitly declared or daemon-owned names are injected.
3. Application secrets cannot override daemon/runtime/protocol control env.
4. Provider secrets and `required_secrets` use the same source precedence where possible.
5. Collision behavior is explicit and tested.
6. Errors name the bad key/source without printing values.

## Proposed API shape

```rust
pub struct EnvContractBuilder {
    base: BTreeMap<String, String>,
    declared_secrets: BTreeMap<String, String>,
    provider_secrets: BTreeMap<String, String>,
    runtime_env: BTreeMap<String, String>,
    protocol_env: BTreeMap<String, String>,
    per_spawn_env: BTreeMap<String, String>,
}

impl EnvContractBuilder {
    pub fn new() -> Self;

    pub fn with_base_allowlist_from_host(self) -> Result<Self>;
    pub fn with_daemon_roots(self, roots: DaemonRoots) -> Result<Self>;
    pub fn with_declared_secrets(self, secrets: BTreeMap<String, String>) -> Result<Self>;
    pub fn with_provider_secret(self, name: &str, value: String) -> Result<Self>;
    pub fn with_runtime_descriptor_env(self, env: BTreeMap<String, String>) -> Result<Self>;
    pub fn with_protocol_env(self, env: BTreeMap<String, String>) -> Result<Self>;
    pub fn with_per_spawn_env(self, env: BTreeMap<String, String>) -> Result<Self>;

    pub fn build(self) -> Result<Vec<(String, String)>>;
}
```

The exact type names are flexible. The important point is that all callers use one implementation for validation and final merging.

## Source categories

### Base allowlist

Host env names required for process startup and network egress:

- `PATH`, `HOME`, locale/timezone vars
- proxy vars such as `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`
- certificate vars such as `SSL_CERT_FILE`, `SSL_CERT_DIR`
- logging/debug vars if deliberately supported

These names are infrastructure, not secrets. Application secrets must not collide with them.

### Daemon roots

Daemon-resolved root discovery env, e.g.:

- `USER_SPACE`
- `RYEOS_SYSTEM_SPACE_DIR`

These should be daemon-owned and should override any inherited host value. Application secrets must not collide with them.

### Declared secrets

Secrets declared by item metadata (`required_secrets`) and resolved from:

1. sealed RyeOS vault
2. daemon host env
3. `.env` overlay

Only declared names are read and injected. Names must pass the protected subprocess env-name policy.

### Provider secrets

Provider auth env vars currently flow through `preflight_inject_provider_secret`. Long term, provider auth should either:

- be folded into the declared-secret resolver before env construction, or
- enter the builder through `with_provider_secret`, using the same protected-name policy.

If provider auth remains an implicit exception to `required_secrets`, document it clearly and keep the source/collision policy identical.

### Runtime descriptor env

Runtime descriptor env (`env_config.env`) is engine/runtime-controlled and can include template output. It must not be allowed to override daemon callback/protocol env unless explicitly categorized as daemon-owned.

`RYEOS_TOOL_ENV_PASSTHROUGH` should remain distinct: it is only for descriptor template interpolation (`${VAR}`), not a general secret injection mechanism.

### Protocol / callback / per-spawn env

Daemon-owned control env such as callback tokens, thread auth tokens, thread IDs, checkpoint dirs, and project paths should be in a protected high-priority category.

These values may intentionally override base allowlist values, but no application-controlled secret/runtime env should override them.

## Merge precedence

A reasonable final merge order:

1. base allowlisted host env
2. daemon-resolved roots
3. declared application secrets
4. provider secrets
5. runtime descriptor env
6. protocol/callback/per-spawn daemon env

However, precedence alone is not enough. The builder should reject illegal collisions rather than silently relying on order. Examples:

- Declared secret `USER_SPACE` → reject.
- Declared secret `RYEOSD_THREAD_AUTH_TOKEN` → reject.
- Declared secret `HTTP_PROXY` → reject or require an explicit non-secret config path.
- Runtime descriptor attempts to set callback token env → reject.
- Protocol env overwrites protocol env from another source → reject unless same value and explicitly allowed.

## Protected name policy

Centralize this policy in one module. It should reject application-controlled secret names that are:

- on the vault blocked list (`PATH`, `HOME`, `LD_PRELOAD`, etc.)
- in the subprocess base allowlist
- daemon roots (`USER_SPACE`, `RYEOS_SYSTEM_SPACE_DIR`)
- any `RYEOS_` or `RYEOSD_` prefix unless explicitly allowlisted for a daemon-owned category
- proxy and CA names (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`, `SSL_CERT_DIR`)

The policy should distinguish categories. A name can be valid for daemon-owned protocol env but invalid for application secrets.

## Tests to include

Unit tests:

- Declared secret names like `SUPABASE_SERVICE_KEY` and `OXYLABS_PASSWORD` are accepted.
- Declared secret names like `USER_SPACE`, `RYEOS_SYSTEM_SPACE_DIR`, `RYEOSD_THREAD_AUTH_TOKEN`, `HTTP_PROXY`, and `SSL_CERT_FILE` are rejected.
- Vault > host env > `.env` precedence is preserved for declared secrets.
- Undeclared host env vars never appear in the built env.
- Application secrets cannot override base allowlist or daemon root env.
- Protocol/callback env wins only in daemon-owned categories.
- Provider secret injection uses the same protected-name policy.

Integration tests:

- A spawned subprocess sees a declared host-env secret.
- A spawned subprocess does not see an undeclared host-env secret.
- A tool declaring a protected name fails before spawn.
- Runtime descriptor `${VAR}` still requires `RYEOS_TOOL_ENV_PASSTHROUGH`; `required_secrets` does not satisfy template interpolation.
- A provider auth env var with a protected name fails closed.

## Migration plan

1. Keep current functions as wrappers:
   - `build_spawn_env`
   - `build_subprocess_envs`
   - native runtime launch env construction
2. Introduce `EnvContractBuilder` behind those wrappers.
3. Port generic plan-node spawn first.
4. Port native runtime launch env construction.
5. Port provider preflight env injection to the builder.
6. Remove duplicated validation once all paths share the builder.
7. Update docs to describe env categories and source precedence.

## Non-goals

- Do not reintroduce blanket host env inheritance.
- Do not make `RYEOS_TOOL_ENV_PASSTHROUGH` a secret mechanism.
- Do not print secret values in diagnostics.
- Do not make project `.env` higher precedence than deployment host env.

## Decision

Do not implement this refactor as part of the current Snap Track unblock. The scoped fix is sufficient now: declared secrets resolve from vault/host env/`.env`, and protected env names are rejected before injection. Implement `EnvContractBuilder` later when RyeOS next needs a broader env-construction cleanup.
