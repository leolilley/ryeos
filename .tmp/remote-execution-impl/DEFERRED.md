# Remote Execution — Deferred Items (canonical status)

Every item has one status:

- **Shipped** — absorbed into Phases 1–3 of the canonical plan.
- **Excluded** — explicitly out of this effort per Phase 4; re-trigger condition recorded.
- **Deferred** — remains a watchlist item per Phase 5; re-trigger condition recorded.
- **Triggered** — a deferred item whose re-trigger condition has been met but whose full resolution is not yet complete. Tracked alongside deferred items with an explicit action-required note.

The phase docs (`PHASE-0` through `PHASE-5`) are the canonical plan.
This document is the registry of what is and is not in scope.

---

## 1. Architecture-level deferrals

### 1.1 Registry with namespace claims
- **Status:** Deferred (Phase 5 watchlist)
- **What:** Central registry where publishers claim namespaces (e.g.
  `ryeos.core`) and consumers discover/pull bundles by name.
- **Why deferred:** No multi-publisher use case. Single-operator nodes
  push to each other directly.
- **Re-trigger:** Multiple teams publishing bundles that need to be
  discoverable without prior arrangement.

### 1.2 Daemon-to-daemon forwarding
- **Status:** Excluded (architectural — never)
- **What:** Node A receives an execute request intended for Node C and
  forwards it.
- **Why:** Against the project model. CLI is the orchestrator; each
  node is autonomous.
- **Re-trigger:** None — deliberate exclusion.

### 1.3 Chunked object transfer
- **Status:** Deferred (Phase 5 watchlist)
- **What:** Split large CAS blobs into chunks for upload/download with
  resume.
- **Why deferred:** Current object sizes don't warrant it.
- **Re-trigger:** Blobs regularly exceed ~100 MB or operators report
  push/pull failures on unreliable links.

### 1.4 mTLS / TLS pinning
- **Status:** Deferred (Phase 5 watchlist)
- **What:** Mutual TLS where the client verifies the server's cert pin
  and vice versa.
- **Why deferred:** HTTPS + TOFU (`remote configure` pins
  `principal_id`) + signed requests is sufficient. The signing key is
  the identity, not the TLS cert.
- **Re-trigger:** Compliance requirements or persistent TOFU failures.

### 1.5 Per-fingerprint CAS manifests
- **Status:** Deferred (Phase 5 watchlist)
- **What:** Track which principal uploaded which blobs for quota
  enforcement and per-principal GC.
- **Why deferred:** Per the **v1 trust boundary** — CAS is shared/global
  within a node; capability checks protect access, not storage
  partitioning. Audit trail exists in logs.
- **Re-trigger:** Multi-tenant nodes needing principal isolation,
  quota, or billing.

### 1.6 Request-scoped project trust overlay
- **Status:** Deferred (Phase 5 watchlist)
- **What:** Pushed content includes trust docs loaded for the duration
  of that request, without modifying the boot-time trust store.
- **Why deferred:** Engine trust store is fixed at construction.
  Boot-time trust (`ryeos trust pin`) is the deployment prerequisite.
- **Re-trigger:** CI systems generating new keys per pipeline and
  unable to run `ryeos trust pin` per key change.

### 1.7 Per-fingerprint persistent workspace
- **Status:** Deferred (Phase 5 watchlist)
- **What:** Each remote principal gets a long-lived checkout directory
  instead of temp dirs per execution.
- **Why deferred:** Temp-dir CAS checkout → execute → fold-back →
  cleanup cycle works.
- **Re-trigger:** Checkout cost dominates remote execution runtime.

### 1.8 Typed `HandlerContext`
- **Status:** Deferred (Phase 5 watchlist)
- **What:** Replace `_caller_fingerprint` / `_caller_scopes` injection
  with a typed `HandlerContext { state, principal_id, scopes }`.
- **Why deferred:** Current injection works; typed context requires
  changing every handler signature.
- **Re-trigger:** Principal-aware handler count grows and the
  `_caller_*` naming becomes error-prone.

---

## 2. P2 functionality (from original INVESTIGATION.md)

### 2.1 Bundle sync handlers
- **Status:** Excluded (Phase 4)
- **What:** `bundle_remote_install` (CAS-sourced) and `bundle_export`
  (serves bundle content as CAS objects).
- **Why excluded now:** Local `bundle_install` works; remote bundle
  install is a deployment convenience, not a remote-execution
  correctness item.
- **Re-trigger:** Operators routinely deploying updated bundles
  cross-node and finding the manual stop-copy-start cycle painful.

### 2.2 Node layout move
- **Status:** Excluded (Phase 4)
- **What:** Split `node/config.yaml` into `node/runtime/settings.yaml`
  + `node/profiles/thread-kinds.yaml`.
- **Why excluded now:** Non-blocking refactor.
- **Re-trigger:** Post-shipping cleanup pass.

### 2.3 Engine `kind_registry` refactor
- **Status:** Excluded (Phase 4)
- **What:** Split the 3,139-line `kind_registry.rs` into focused
  modules.
- **Why excluded now:** Infrastructure cleanup with no functional
  impact on remote execution.
- **Re-trigger:** New kind types added that make the monolith
  unwieldy.

### 2.4 Per-principal vault partitioning
- **Status:** Excluded (Phase 4) — directly conflicts with v1 trust boundary
- **What:** Store secrets per-fingerprint at
  `<vault_dir>/secrets/<fp>/<name>.toml`.
- **Why excluded now:** Per the **v1 trust boundary** — vault is a
  single shared store; capability checks protect mutation/listing, not
  per-principal isolation.
- **Re-trigger:** Multi-tenant nodes where principal A must not see
  principal B's secrets, or per-principal secret rotation.

### 2.5 `ryeosd → ryeos-tools` dependency cleanup
- **Status:** Triggered — `ryeos-api → ryeos-tools` edge now exists
- **What:** `ryeos-api` depends on `ryeos-tools` for
  `install::preflight_verify_bundle` (used in `bundle_install.rs`).
  The original `ryeosd → ryeos-tools` edges for vault and authorize
  were resolved during restructuring, but the API crate now pulls in
  the tools crate for the bundle-install handler.
- **Why still deferred:** The dep is pre-existing (not introduced by
  remote execution work). Moving `preflight_verify_bundle` to
  `ryeos-app` or `ryeos-state` is a correctness-neutral refactor.
- **Action required:** Before next release, either move the function
  or accept the dep edge and document it.

### 2.6 `AppState` carries full `Config`
- **Status:** Deferred
- **What:** `AppState` holds the full daemon `Config` struct rather
  than the minimal subset needed by service handlers. This is a
  pre-existing architectural decision from before the crate split.
- **Why deferred:** Refactoring `AppState` to hold only the fields
  handlers need (system_space_dir, vault, identity, authorizer, etc.)
  would touch every handler and is a correctness-neutral cleanup.
- **Re-trigger:** Handler count grows enough that the broad `Config`
  access encourages coupling to implementation details.

### 2.7 `ServiceRegistry` lives in `ryeos-app`
- **Status:** Deferred
- **What:** The `ServiceRegistry` (descriptor table + dispatch) is in
  `ryeos-app` rather than `ryeos-api` where handlers are defined.
  This means `ryeos-api` cannot self-register its handlers.
- **Why deferred:** The split was intentional (the registry depends on
  `AppState` which is in `ryeos-app`). Moving it would require either
  a trait-based registry or inverting the dependency.
- **Re-trigger:** A third consumer crate needs to register handlers
  without depending on `ryeos-app`.

---

## 3. Implicitly deferred items

### 3.1 `remote threads` / `remote thread-status` CLI verbs
- **Status:** Shipped (Phase 3.4)
- **What:** CLI commands to list/inspect threads on a remote node.
- **How it shipped:** New signed GET routes `/threads` and
  `/threads/{id}` reuse Phase 1 hardened handlers; new local CLI verbs
  `remote threads` / `remote thread-status` wrap `RemoteClient`.

### 3.2 `remote pull` (arbitrary hash fetch)
- **Status:** Excluded (Phase 4)
- **What:** `ryeos remote pull --remote <name> --hashes h1,h2,...`
- **Why excluded now:** `RemoteClient::objects_get()` already works; a
  CLI verb is operator convenience for CAS debugging.
- **Re-trigger:** Repeated operator CAS-debugging workflows.

### 3.3 `remote seal` (client-side sealing)
- **Status:** Excluded (Phase 4) — conflicts with Phase 0 decision 3
- **What:** Locally encrypt a secret with the remote's vault X25519
  public key.
- **Why excluded now:** v1 vault writes are server-side sealing; the
  full vault public key is intentionally not exposed.
- **Re-trigger:** Compliance requirement that the server never sees
  plaintext.

### 3.4 Webhook / HMAC invoker for remote triggers
- **Status:** Excluded (Phase 4)
- **What:** Integration tests for webhook-triggered execution across
  nodes.
- **Why excluded now:** Webhook invoker already exists; not
  specifically a remote-execution feature.
- **Re-trigger:** Webhook-driven remote orchestration becomes a
  supported v1 user flow.

### 3.5 Async remote execute polling
- **Status:** Excluded (Phase 4) — Phase 0 decision 1 locks v1 as synchronous
- **What:** `ryeos remote execute <ref> --remote <name> --async`
  returning `{ thread_id }`, with client polling `threads_get` until
  terminal.
- **Why excluded now:** Half-shipping async polling without a
  persisted push-base journal is incorrect — conflict detection in
  pull/apply needs the in-memory pushed manifest.
- **Re-trigger:** Long-lived remote executions exceed synchronous CLI
  tolerance — at which point a real async job/result model is
  designed.

### 3.6 `signed_get()` on `RemoteClient`
- **Status:** Shipped (Phase 3.2)
- **What:** Authenticated GET support.
- **How it shipped:** Implemented once on `RemoteClient`; reused by
  `vault_list`, `threads_list`, `threads_get`.

### 3.7 Trust prerequisite docs
- **Status:** Shipped (Phase 3.5)
- **What:** Operator-facing documentation for trust setup, key
  rotation, and v1 trust boundary.
- **How it shipped:** New `docs/operations/remote-execution-v1.md`
  covering prerequisites, trust-boundary statement, wildcard-delegation
  policy, node-key rotation procedure, ignore-cache operational note,
  and end-to-end workflow.

### 3.8 `ryeos remote configure` — ingest-ignore caching
- **Status:** Shipped (Phase 2.3)
- **What:** Fetch and cache the remote's ingest-ignore rules during
  `remote configure`.
- **How it shipped:** New `get_ingest_ignore()` client method;
  `RemoteConfig.ingest_ignore` field; `remote_push` uses the cached
  remote rules.

---

## 4. Status reconciliation (kickoff opener)

| Item                                                | Status                            |
| --------------------------------------------------- | --------------------------------- |
| Remaining remote server-side handlers               | Shipped (Phases 1.4, 2.3, 3.1)    |
| Bundle sync handlers                                | Excluded (§2.1)                   |
| Node layout move                                    | Excluded (§2.2)                   |
| Engine `kind_registry` refactor                     | Excluded (§2.3)                   |
| Trust prerequisite docs                             | Shipped (§3.7 → Phase 3.5)        |
| `ryeosd → ryeos-tools` cleanup                      | Triggered (§2.5)                  |

---

## Summary table

| #   | Item                                       | Status   | Lands in / Re-trigger                                  |
| --- | ------------------------------------------ | -------- | ------------------------------------------------------ |
| 1.1 | Registry with namespace claims             | Deferred | Multi-publisher use case                               |
| 1.2 | Daemon-to-daemon forwarding                | Excluded | Never (architectural)                                  |
| 1.3 | Chunked object transfer                    | Deferred | Large binary assets / unreliable links                 |
| 1.4 | mTLS / TLS pinning                         | Deferred | Compliance / persistent TOFU failures                  |
| 1.5 | Per-fingerprint CAS manifests              | Deferred | Multi-tenancy / quota / billing                        |
| 1.6 | Request-scoped project trust overlay       | Deferred | Frequent key rotation in CI                            |
| 1.7 | Per-fingerprint persistent workspace       | Deferred | Checkout cost dominates runtime                        |
| 1.8 | Typed `HandlerContext`                     | Deferred | Principal-aware handler count grows                    |
| 2.1 | Bundle sync handlers                       | Excluded | Cross-node bundle deploy pain                          |
| 2.2 | Node layout move                           | Excluded | Post-shipping cleanup pass                             |
| 2.3 | Engine `kind_registry` refactor            | Excluded | New kind types make monolith unwieldy                  |
| 2.4 | Per-principal vault partitioning           | Excluded | Multi-tenant secret isolation                          |
| 2.5 | `ryeosd → ryeos-tools` cleanup             | Triggered | `ryeos-api → ryeos-tools` edge exists; pre-existing dep |
| 2.6 | `AppState` carries full `Config`            | Deferred  | Handler coupling to `Config` implementation details     |
| 2.7 | `ServiceRegistry` in `ryeos-app`            | Deferred  | Third consumer crate needs handler registration         |
| 3.1 | Remote threads / thread-status verbs       | Shipped  | Phase 3.4                                              |
| 3.2 | Remote pull (arbitrary hashes)             | Excluded | Repeated CAS debugging workflows                       |
| 3.3 | Remote seal (client-side sealing)          | Excluded | Server-never-sees-plaintext compliance                 |
| 3.4 | Webhook remote trigger integration tests   | Excluded | Webhook flow becomes supported v1 surface              |
| 3.5 | Async remote execute polling               | Excluded | Long-lived executions exceed sync CLI tolerance        |
| 3.6 | `RemoteClient::signed_get()`               | Shipped  | Phase 3.2                                              |
| 3.7 | Trust prerequisite docs                    | Shipped  | Phase 3.5                                              |
| 3.8 | Configure-time ignore caching              | Shipped  | Phase 2.3                                              |
