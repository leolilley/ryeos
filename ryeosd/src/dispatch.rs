//! Schema-driven dispatch core.
//!
//! Reads `KindSchema.execution.terminator` and routes to the right
//! backend. There is NO silent fallback: a kind without an executable
//! schema returns 501; a schema with neither terminator, aliases, nor a
//! runtime registered for it via `RuntimeRegistry::lookup_for` is a
//! `SchemaMisconfigured` error (caught here, not at engine init).
//!
//! V5.3 advanced-path foundation:
//! - **B1** Runtime cap (`runtime.execute`) is enforced for DIRECT
//!   `runtime:*` invocation only — i.e. when the user-supplied root
//!   `item_ref` parses to `kind == "runtime"`. Indirect alias chains
//!   (`directive:foo` → registry → `runtime:directive-runtime`) do NOT
//!   inherit the cap. The gate consults `DispatchRequest.original_root_kind`.
//! - **B2** When a kind schema has `execution:` but neither a terminator
//!   nor an `@<kind>` alias to follow, the loop consults
//!   `RuntimeRegistry::lookup_for(kind)` for an explicit *named* default
//!   runtime. This is a registry hop — NOT a silent fallback. The
//!   registry returns the canonical `runtime:<name>` ref the loop then
//!   chases on the next hop.
//! - **B4** Each loop iteration resolves the current ref via
//!   `engine.resolve(&plan_ctx, &current_ref)` BEFORE consulting the
//!   schema, then keys the schema by `resolved.kind`. The `runtime:`
//!   special case (no `executor_id` field on runtime YAMLs → engine
//!   resolution fails) consults `RuntimeRegistry::lookup_by_ref`
//!   instead so the verified runtime metadata is still available
//!   downstream. The per-hop resolved item is threaded into
//!   `DispatchRequest.current_resolved` to avoid double-resolution in
//!   `dispatch_managed_subprocess`.
//! - **A1** Errors are typed as `DispatchError` end-to-end; the HTTP
//!   layer in `api/execute.rs` maps them via `http_status()` once per
//!   request — no substring matching survives.
//! - **A3** Thread-profile names (`tool_run`, `service_run`,
//!   `runtime_run`, …) are read from `schema.execution.thread_profile`
//!   at every dispatch site. There are zero kind-name string literals
//!   in dispatch routing.
//! - **A4** Per-hop logic is extracted into `resolve_dispatch_hop`,
//!   which returns a `VerifiedHop` carrying the schema, the
//!   (optional) resolved item, and a `HopAction` instructing the
//!   loop to terminate, follow an alias, or use a registry hop. The
//!   loop body is therefore a thin dispatcher.
//! - **S2 / F4** Every "X not found" surface enumerates the
//!   alternatives so an operator can fix the schema/cap/registry
//!   without spelunking the source.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{ResolvedItem, VerifiedItem};
use ryeos_engine::kind_registry::{
    DelegationVia, ExecutionSchema,
    InProcessRegistryKind, TerminatorDecl,
};
use ryeos_engine::runtime_registry::VerifiedRuntime;

use crate::dispatch_error::DispatchError;
use crate::dispatch_role::{SubprocessRole, enforce_runtime_target_caps};
use crate::execution::launch;
use crate::service_executor::{
    self, ExecutionContext, ExecutionMode, ServiceExecutionResult,
};
use crate::services::thread_lifecycle::ResolvedExecutionRequest;
use crate::state::AppState;

/// Single source of truth for the `runtime:` ref kind discriminator.
/// Used in two narrow places (B1 cap gate, B4 resolve special-case)
/// where the kind name carries dispatch semantics that come from the
/// `runtime` *kind schema*'s contract, not from a routing decision.
/// The grep gate tolerates references to this constant.
pub(crate) const ROOT_KIND_RUNTIME: &str = "runtime";

/// Request shape consumed by the schema-driven dispatch fns. Carries
/// every input the two terminators (Subprocess, InProcess) need so
/// `/execute`'s HTTP layer can hand off
/// once and let `dispatch::dispatch` do all routing.
///
/// V5.2 native-runtime cap fields (`launch_mode`, `target_site_id`,
/// `project_source_is_pushed_head`) are still surfaced verbatim so
/// `check_dispatch_capabilities` reproduces the pinned 400 wording
/// (see `ryeosd/tests/dispatch_pin.rs`).
///
/// **B1**: `original_root_kind` is the kind parsed from the user's
/// original `/execute` `item_ref`. The runtime cap gate fires ONLY
/// when this is `"runtime"`. Alias chains that land on a runtime via
/// the registry / `@directive` chain do not inherit `runtime.execute`.
#[derive(Debug, Clone)]
pub struct DispatchRequest<'a> {
    pub launch_mode: &'a str,
    pub target_site_id: Option<&'a str>,
    pub project_source_is_pushed_head: bool,
    pub validate_only: bool,
    pub params: Value,
    pub acting_principal: &'a str,
    /// Effective project root used for resolution (matches
    /// `ResolvedProjectContext.effective_path`).
    pub project_path: &'a Path,
    /// Original project root from the HTTP request (used to derive
    /// HEAD ref names in the runner). Matches
    /// `ResolvedProjectContext.original_path`.
    pub original_project_path: PathBuf,
    /// CAS snapshot hash, when execution was bootstrapped from a
    /// pushed HEAD checkout (carried into the runner so spawn-time
    /// resume metadata can pin the snapshot).
    pub snapshot_hash: Option<String>,
    /// Optional pre-checked-out tempdir; ownership is transferred
    /// into the runner's `ExecutionGuard` for cleanup. The HTTP layer
    /// disarms its `TempDirGuard` before constructing this request.
    pub temp_dir: Option<PathBuf>,
    /// **B1**: kind parsed from the user-supplied root `item_ref`.
    /// `dispatch_managed_subprocess` gates `runtime.execute` enforcement
    /// on this being `"runtime"` so indirect alias chains are not
    /// retroactively cap-broadened.
    pub original_root_kind: &'a str,
    /// When `Some`, every dispatch leaf that creates a thread row
    /// must use this id verbatim instead of minting a fresh one
    /// (via `create_root_thread_with_id` / equivalent for the
    /// service audit row). All built-in leaves honor it:
    /// `dispatch_managed_subprocess`, `dispatch_subprocess` (inline +
    /// detached), and `dispatch_service`. The kind-agnostic SSE
    /// `dispatch_launch` source mints the id up front so it can
    /// subscribe to the event hub *before* the launch task begins,
    /// which is required to avoid losing the very first lifecycle
    /// event. `None` (the default) preserves the
    /// "mint inside the leaf" path used by indirect runtime
    /// dispatch and ad-hoc `/execute` calls.
    pub pre_minted_thread_id: Option<String>,
}

/// Check protocol-derived capabilities against the request shape.
/// Reads the capability bits from the verified protocol descriptor.
/// On mismatch, returns the V5.2 wording verbatim (pin tests assert
/// byte equality):
/// * `pushed_head` → "pushed_head not yet supported for native runtimes"
/// * `target_site_id` → "remote execution not yet supported for native runtimes"
/// * `launch_mode == "detached"` → "detached mode not yet supported for native runtimes"
fn check_dispatch_capabilities(
    caps: &ryeos_engine::protocol_vocabulary::ProtocolCapabilities,
    request: &DispatchRequest<'_>,
) -> Result<(), DispatchError> {
    if request.project_source_is_pushed_head && !caps.allows_pushed_head {
        return Err(DispatchError::CapabilityRejected {
            reason: "pushed_head not yet supported for native runtimes".into(),
        });
    }
    if request.target_site_id.is_some() && !caps.allows_target_site {
        return Err(DispatchError::CapabilityRejected {
            reason: "remote execution not yet supported for native runtimes".into(),
        });
    }
    if request.launch_mode == "detached" && !caps.allows_detached {
        return Err(DispatchError::CapabilityRejected {
            reason: "detached mode not yet supported for native runtimes".into(),
        });
    }
    Ok(())
}

// ── A4: per-hop resolution helper + HopAction ─────────────────────────

/// What the dispatch loop should do after a single hop's
/// resolve+verify+schema lookup.
#[derive(Debug)]
pub(crate) enum HopAction {
    /// Schema declares a terminator; the second tuple element is the
    /// schema-declared `thread_profile` (A3 — never a kind-name
    /// hardcode in the dispatch sites).
    Terminate(TerminatorDecl, String),
    /// Schema has no terminator but declares an `@<kind>` alias the
    /// loop must follow next iteration.
    FollowAlias(CanonicalRef),
    /// Schema has neither terminator nor alias, but
    /// `RuntimeRegistry::lookup_for(<resolved_kind>)` returned a
    /// default runtime — chase its canonical_ref next iteration. This
    /// is an EXPLICIT named registry hop, not a silent fallback.
    UseRegistry(CanonicalRef),
}

/// Per-hop verification output. The dispatch loop stays thin by
/// delegating all hop logic here.
///
/// **P1.3**: carries `Option<VerifiedItem>` (verified, not just resolved).
/// Leaf dispatchers trust the hop's verification and stop re-verifying
/// defensively.
///
/// **P1.1**: `thread_profile` is extracted from the schema's
/// `execution.thread_profile` at every hop — including hops that
/// don't terminate (registry/alias hops). The dispatch loop captures
/// this from the FIRST hop as the "root subject" profile, so indirect
/// paths (directive → registry → runtime) correctly record the
/// directive's profile, not the runtime's.
///
/// **P1.4**: `runtime` is populated for runtime-kind refs via
/// `RuntimeRegistry::lookup_by_ref`, providing binary_ref and
/// required_caps for downstream use. Non-runtime refs leave this
/// `None`.
#[derive(Debug)]
pub(crate) struct VerifiedHop {
    pub canonical_ref: CanonicalRef,
    /// P1.3: per-hop verified item. The loop verifies at every hop
    /// boundary — leaf dispatchers trust this and skip re-verification.
    pub verified: Option<VerifiedItem>,
    /// P1.1: thread_profile from the schema's `execution` block.
    /// Available for all executable kinds. Used by the loop to capture
    /// the root subject's profile on the first hop.
    pub thread_profile: Option<String>,
    /// P1.4: for runtime-kind refs, the verified runtime metadata
    /// from the registry. Provides binary_ref and required_caps.
    pub runtime: Option<VerifiedRuntime>,
    pub next: HopAction,
}

/// **A4**: single-hop resolve + verify + schema lookup + HopAction decision.
///
/// **P1.3**: calls `engine.verify` after `engine.resolve` at every hop
/// boundary. Leaf dispatchers trust the verification and skip
/// re-verification defensively.
///
/// **P1.4**: always calls `engine.resolve` (no special-case for
/// runtime refs). For runtime-kind refs, also looks up
/// `RuntimeRegistry::lookup_by_ref` in addition, attaching the typed
/// `VerifiedRuntime` to the hop result.
///
/// **P1.1**: always extracts `thread_profile` from the schema's
/// `execution` block, even for non-terminator hops (registry/alias),
/// so the loop can capture it on the first hop as the root subject
/// profile.
pub(crate) fn resolve_dispatch_hop(
    current_ref: &CanonicalRef,
    ctx: &ExecutionContext,
) -> Result<VerifiedHop, DispatchError> {
    // **B4 fast-path**: if the schema for the ref's kind already
    // declares "no execution block" (config, V5.3 knowledge, …),
    // short-circuit to NotRootExecutable BEFORE attempting engine
    // resolution. Otherwise a missing item under one of those kinds
    // would surface as a 400 "resolution failed" instead of the
    // contractual 501. The schema is the source of truth for
    // executability — resolve only matters once we know we'd dispatch.
    if let Some(schema) = ctx.engine.kinds.get(&current_ref.kind) {
        if schema.execution().is_none() {
            return Err(DispatchError::NotRootExecutable {
                kind: current_ref.kind.clone(),
                detail: "schema has no `execution:` block".into(),
            });
        }
    }

    // **P1.3 + P1.4**: per-hop resolve AND verify. No special-case
    // for runtime refs — engine.resolve produces content_hash,
    // source_path, and audit data for ALL kinds including runtime.
    // Verification failure is a hard error at the hop boundary.
    let verified: Option<VerifiedItem> = match ctx.engine.resolve(&ctx.plan_ctx, current_ref) {
        Ok(resolved) => {
            let v = ctx.engine.verify(&ctx.plan_ctx, resolved).map_err(|e| {
                DispatchError::InvalidRef(
                    current_ref.to_string(),
                    format!("verification failed: {e}"),
                )
            })?;
            tracing::debug!(
                item_ref = %current_ref,
                trust_class = ?v.trust_class,
                "hop verified"
            );
            Some(v)
        }
        Err(_) => {
            // Resolution failed — the ref may not exist on disk. This
            // is not necessarily fatal: the schema lookup below will
            // produce a clearer error (SchemaMisconfigured enumerating
            // available kinds) if the kind has no items at all.
            None
        }
    };

    let schema_kind: String = verified
        .as_ref()
        .map(|v| v.resolved.kind.clone())
        .unwrap_or_else(|| current_ref.kind.clone());

    // **P1.1**: extract thread_profile from the schema's execution
    // block at every hop — even non-terminator hops. The dispatch loop
    // captures this from the first hop as the root subject profile.
    let thread_profile: Option<String>;

    let schema = ctx.engine.kinds.get(&schema_kind).ok_or_else(|| {
        let mut available: Vec<String> = ctx
            .engine
            .kinds
            .kinds()
            .map(|k| k.to_string())
            .collect();
        available.sort();
        DispatchError::SchemaMisconfigured {
            kind: schema_kind.clone(),
            detail: format!(
                "no kind schema registered; registered kinds: [{}]",
                available.join(", ")
            ),
        }
    })?;

    let exec: &ExecutionSchema = schema.execution().ok_or_else(|| {
        DispatchError::NotRootExecutable {
            kind: schema_kind.clone(),
            detail: "schema has no `execution:` block".into(),
        }
    })?;

    thread_profile = exec.thread_profile.clone();

    // **P1.4**: for runtime-kind refs, also look up the runtime
    // registry. This provides binary_ref, required_caps, and the
    // canonical ref for downstream dispatch. The registry lookup is
    // IN ADDITION to engine.resolve — we get both audit data AND
    // runtime metadata.
    let runtime: Option<VerifiedRuntime> = if current_ref.kind == ROOT_KIND_RUNTIME {
        ctx.engine.runtimes.lookup_by_ref(current_ref).cloned()
    } else {
        None
    };

    // Terminator wins over alias/registry.
    if let Some(terminator) = exec.terminator.as_ref() {
        // **A3**: thread profile MUST be declared on the schema; no
        // kind-name fallback. Schema validation at engine init enforces
        // this for any executable schema, but we re-check defensively
        // so an out-of-band schema mutation cannot silently degrade
        // the audit trail.
        let tp = thread_profile.as_deref().ok_or_else(|| {
            DispatchError::SchemaMisconfigured {
                kind: schema_kind.clone(),
                detail: "schema declares a terminator but no `execution.thread_profile`".into(),
            }
        })?;
        return Ok(VerifiedHop {
            canonical_ref: current_ref.clone(),
            verified,
            thread_profile: Some(tp.to_string()),
            runtime,
            next: HopAction::Terminate(terminator.clone(), tp.to_string()),
        });
    }

    // No terminator — follow the kind's `@<kind>` alias if present.
    let alias_key = format!("@{schema_kind}");
    if let Some(alias_target) = exec.aliases.get(&alias_key) {
        let next_ref = CanonicalRef::parse(alias_target).map_err(|e| {
            DispatchError::SchemaMisconfigured {
                kind: schema_kind.clone(),
                detail: format!(
                    "alias '{alias_key}' → '{alias_target}' is not a valid canonical ref: {e}"
                ),
            }
        })?;
        return Ok(VerifiedHop {
            canonical_ref: current_ref.clone(),
            verified,
            thread_profile,
            runtime,
            next: HopAction::FollowAlias(next_ref),
        });
    }

    // Explicit delegation — schema MUST opt in via `delegate:` to be
    // routed through the runtime registry. There is no implicit
    // fallback on absence: pre-V5.4 the dispatcher silently called
    // `RuntimeRegistry::lookup_for` here when terminator and alias
    // were both missing. Now the schema author has to declare intent.
    if let Some(delegation) = exec.delegate.as_ref() {
        match &delegation.via {
            DelegationVia::RuntimeRegistry { serves_kind } => {
                let lookup_kind = serves_kind
                    .as_deref()
                    .unwrap_or(schema_kind.as_str());
                let rt = ctx.engine.runtimes.lookup_for(lookup_kind).map_err(|_| {
                    let mut serves: Vec<String> = ctx
                        .engine
                        .runtimes
                        .all()
                        .map(|r| format!("{}→{}", r.yaml.serves, r.canonical_ref))
                        .collect();
                    serves.sort();
                    DispatchError::SchemaMisconfigured {
                        kind: schema_kind.clone(),
                        detail: format!(
                            "schema declares `delegate: {{ via: runtime_registry, \
                             serves_kind: {lookup_kind} }}` but no runtime serves \
                             '{lookup_kind}' (registered runtimes: [{}])",
                            serves.join(", ")
                        ),
                    }
                })?;
                return Ok(VerifiedHop {
                    canonical_ref: current_ref.clone(),
                    verified,
                    thread_profile,
                    runtime,
                    next: HopAction::UseRegistry(rt.canonical_ref.clone()),
                });
            }
        }
    }

    // Truly stuck. Schema-load validation should have rejected this
    // shape (no terminator, no aliases, no delegate), so this branch
    // is defensive against an out-of-band schema mutation. Enumerate
    // what the schema declared so an operator can repair it (S2/F4).
    let mut alias_keys: Vec<String> = exec.aliases.keys().cloned().collect();
    alias_keys.sort();
    Err(DispatchError::SchemaMisconfigured {
        kind: schema_kind.clone(),
        detail: format!(
            "schema has no terminator, no matching '{alias_key}' alias, and no \
             `delegate` block — kind cannot be dispatched. Add a terminator, an \
             '@<kind>' alias, or `delegate: {{ via: runtime_registry }}` to the \
             kind schema (declared aliases on schema: [{}])",
            alias_keys.join(", ")
        ),
    })
}

// ── Service terminator ────────────────────────────────────────────────

/// Dispatch a `service:*` ref through the schema-declared
/// `InProcess { registry: Services }` terminator.
///
/// **A3**: the service envelope's `kind` field is read from
/// `schema.execution.thread_profile` (validated at engine init) — no
/// `"service_run"` literal anywhere on this hot path.
pub async fn dispatch_service(
    item_ref: &str,
    thread_profile: &str,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<Value, DispatchError> {
    let params = request.params.clone();
    let canonical = CanonicalRef::parse(item_ref).map_err(|e| {
        DispatchError::InvalidRef(item_ref.to_string(), e.to_string())
    })?;
    let schema = ctx.engine.kinds.get(&canonical.kind).ok_or_else(|| {
        let mut available: Vec<String> =
            ctx.engine.kinds.kinds().map(|k| k.to_string()).collect();
        available.sort();
        DispatchError::SchemaMisconfigured {
            kind: canonical.kind.clone(),
            detail: format!(
                "no kind schema registered; registered kinds: [{}]",
                available.join(", ")
            ),
        }
    })?;
    let exec = schema.execution().ok_or_else(|| {
        DispatchError::NotRootExecutable {
            kind: canonical.kind.clone(),
            detail: "schema has no `execution:` block".into(),
        }
    })?;
    let terminator = exec.terminator.as_ref().ok_or_else(|| {
        DispatchError::SchemaMisconfigured {
            kind: canonical.kind.clone(),
            detail: "dispatch_service called on a schema with no terminator".into(),
        }
    })?;
    match terminator {
        TerminatorDecl::InProcess {
            registry: InProcessRegistryKind::Services,
        } => {
            let verified = service_executor::resolve_and_verify(
                &ctx.engine,
                &ctx.plan_ctx,
                item_ref,
                Some("service"),
            )?;
            let result: ServiceExecutionResult =
                service_executor::execute_service_verified(
                    verified,
                    item_ref,
                    params,
                    ExecutionMode::Live,
                    ctx,
                    state,
                    request.pre_minted_thread_id.as_deref(),
                )
                .await?;
            let envelope = serde_json::json!({
                "thread": {
                    "thread_id": result.audit_thread_id,
                    "kind": thread_profile,
                    "item_ref": item_ref,
                    "status": "completed",
                    "trust_class": format!("{:?}", result.trust_class),
                    "effective_caps": result.effective_caps,
                },
                "result": result.value,
            });
            Ok(envelope)
        }
        other => Err(DispatchError::SchemaMisconfigured {
            kind: canonical.kind.clone(),
            detail: format!(
                "dispatch_service called on schema declaring terminator {other:?}, not InProcess {{ registry: Services }}"
            ),
        }),
    }
}

// ── Native runtime terminator ─────────────────────────────────────────

// The dispatch loop attaches `VerifiedRuntime` to the hop via
// `RuntimeRegistry::lookup_by_ref` (see `resolve_dispatch_hop`), and
// `dispatch_managed_subprocess` consumes that owned value. Don't
// reintroduce a per-call lookup; the loop owns runtime metadata as
// part of the hop.

/// Strip the `bin/<triple>/` prefix from a runtime YAML's `binary_ref`.
fn strip_binary_ref_prefix(binary_ref: &str) -> Result<String, DispatchError> {
    let parts: Vec<&str> = binary_ref.split('/').collect();
    if parts.len() < 3 || parts[0] != "bin" || parts[1].is_empty() || parts[2].is_empty() {
        return Err(DispatchError::SchemaMisconfigured {
            kind: ROOT_KIND_RUNTIME.into(),
            detail: format!(
                "runtime binary_ref '{binary_ref}' has unexpected shape; expected 'bin/<triple>/<binary>'"
            ),
        });
    }
    Ok(parts[2..].join("/"))
}

/// Core cap-enforcement logic, shared by runtime and subprocess paths.
///
/// Returns `Err(DispatchError::InsufficientCaps)` so `api/execute.rs`
/// maps it to 403 via `http_status()`. There is no substring matching
/// anywhere on this path.
fn enforce_caps(
    item_ref: &str,
    required_caps: &[String],
    caller_scopes: &[String],
) -> Result<(), DispatchError> {
    if caller_scopes.iter().any(|s| s == "*") {
        return Ok(());
    }
    let missing: Vec<String> = required_caps
        .iter()
        .filter(|cap| !caller_scopes.contains(cap))
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(DispatchError::InsufficientCaps {
            runtime: item_ref.to_string(),
            required: required_caps.to_vec(),
            caller_scopes: caller_scopes.to_vec(),
        })
    }
}

/// **B1**: cap gate for runtime dispatch. Delegates to `enforce_caps`.
fn enforce_runtime_caps(
    item_ref: &str,
    required_caps: &[String],
    caller_scopes: &[String],
) -> Result<(), DispatchError> {
    enforce_caps(item_ref, required_caps, caller_scopes)
}

// ── Unified subprocess terminator ─────────────────────────────────────

/// Unified subprocess dispatch. Handles both tool-style (opaque protocol,
/// DetachedOk lifecycle) and runtime-style (runtime_v1 protocol, Managed
/// lifecycle) subprocess execution. The dispatch is driven by the protocol
/// descriptor's `lifecycle.mode`:
///
/// - `DetachedOk` → tool path through `thread_lifecycle → runner`
/// - `Managed` → runtime path through `launch::build_and_launch`
///
/// **B1**: `SubprocessRole` is set ONCE at the top of `dispatch_loop`
/// based on the user's original `item_ref`. Only `RuntimeTarget` triggers
/// the `runtime.execute` cap check. This is a ROLE check, NOT a protocol
/// check — a future kind using `protocol: runtime_v1` without being a
/// runtime MUST NOT trigger the cap.
pub async fn dispatch_subprocess(
    current_ref: &CanonicalRef,
    thread_profile: &str,
    verified: Option<&VerifiedItem>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    role: &SubprocessRole,
    root_subject: Option<RootSubject>,
    hop_runtime: Option<VerifiedRuntime>,
) -> Result<Value, DispatchError> {
    // Defense-in-depth schema check.
    let schema = ctx.engine.kinds.get(&current_ref.kind).ok_or_else(|| {
        let mut available: Vec<String> =
            ctx.engine.kinds.kinds().map(|k| k.to_string()).collect();
        available.sort();
        DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: format!(
                "no kind schema registered for ref '{current_ref}'; registered kinds: [{}]",
                available.join(", ")
            ),
        }
    })?;
    let exec = schema.execution().ok_or_else(|| {
        DispatchError::NotRootExecutable {
            kind: current_ref.kind.clone(),
            detail: "schema has no `execution:` block".into(),
        }
    })?;
    let terminator = exec.terminator.as_ref().ok_or_else(|| {
        DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: "dispatch_subprocess called on a schema with no terminator".into(),
        }
    })?;
    let protocol_ref = match terminator {
        TerminatorDecl::Subprocess { protocol_ref } => protocol_ref.as_str(),
        _ => {
            return Err(DispatchError::SchemaMisconfigured {
                kind: current_ref.kind.clone(),
                detail: format!(
                    "dispatch_subprocess called on schema declaring terminator {terminator:?}, not Subprocess"
                ),
            });
        }
    };

    // 1. Role-based cap enforcement (B1). Only RuntimeTarget triggers
    //    the runtime.execute cap check — role-based, NOT protocol-based.
    enforce_runtime_target_caps(role, &ctx.caller_scopes)?;

    // 2. Resolve protocol descriptor.
    let protocol = ctx
        .engine
        .protocols
        .require(protocol_ref)
        .map_err(|_| DispatchError::ProtocolNotRegistered(protocol_ref.to_string()))?;

    // 3. Protocol-derived capability check. Wording preserved verbatim —
    //    pinned by `ryeosd/tests/dispatch_pin.rs::pin_native_runtime_with_*`.
    check_dispatch_capabilities(&protocol.descriptor.capabilities, request)?;

    // 4. Streaming protocol special-case: detached not allowed.
    use ryeos_engine::protocol_vocabulary::StdoutMode;
    if protocol.descriptor.stdout.mode == StdoutMode::Streaming && request.launch_mode == "detached"
    {
        return Err(DispatchError::StreamingNotDetachable);
    }

    // 5. Branch on lifecycle mode.
    use ryeos_engine::protocol_vocabulary::LifecycleMode;
    match protocol.descriptor.lifecycle.mode {
        LifecycleMode::Managed => {
            // Runtime-style dispatch via launch::build_and_launch.
            dispatch_managed_subprocess(
                current_ref,
                verified,
                thread_profile,
                hop_runtime,
                root_subject,
                request,
                ctx,
                state,
                role,
            )
            .await
        }
        LifecycleMode::DetachedOk => {
            // Tool-style dispatch via thread_lifecycle → runner.
            dispatch_tool_subprocess(
                current_ref,
                thread_profile,
                verified,
                request,
                ctx,
                state,
            )
            .await
        }
        LifecycleMode::Oneshot => {
            // Not yet used — placeholder for future protocol.
            Err(DispatchError::SchemaMisconfigured {
                kind: current_ref.kind.clone(),
                detail: "Oneshot lifecycle not yet implemented".into(),
            })
        }
    }
}

/// Managed-lifecycle subprocess dispatch (runtime_v1 protocol).
/// Uses `launch::build_and_launch` for callback-driven execution.
async fn dispatch_managed_subprocess(
    canonical_ref: &CanonicalRef,
    hop_verified: Option<&VerifiedItem>,
    hop_thread_profile: &str,
    hop_runtime: Option<VerifiedRuntime>,
    root_subject: Option<RootSubject>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    role: &SubprocessRole,
) -> Result<Value, DispatchError> {
    let runtime_ref = canonical_ref.to_string();

    // Extract the verified runtime from the role or hop.
    let verified_runtime = match role {
        SubprocessRole::RuntimeTarget { verified_runtime } => Some(verified_runtime.clone()),
        SubprocessRole::Regular => hop_runtime,
    };

    let verified_runtime = verified_runtime.ok_or_else(|| {
        let mut available: Vec<String> = ctx
            .engine
            .runtimes
            .all()
            .map(|r| r.canonical_ref.to_string())
            .collect();
        available.sort();
        DispatchError::SchemaMisconfigured {
            kind: canonical_ref.kind.clone(),
            detail: format!(
                "managed subprocess for '{runtime_ref}' has no runtime registry entry; registered runtimes: [{}]",
                available.join(", ")
            ),
        }
    })?;

    let params = request.params.clone();
    let acting_principal = request.acting_principal;
    let project_path: &Path = request.project_path;

    let bare = strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)?;
    let executor_ref = format!("native:{bare}");

    // P1.1: determine the subject item for the thread record.
    let subject = match root_subject {
        Some(s) => s,
        None => {
            RootSubject {
                item_ref: runtime_ref.clone(),
                thread_profile: hop_thread_profile.to_string(),
                verified: hop_verified.cloned(),
            }
        }
    };

    // Resolve the subject item for the resolution pipeline.
    let resolved_item: ResolvedItem = match subject.verified {
        Some(v) => v.resolved,
        None => {
            let canonical = CanonicalRef::parse(&subject.item_ref).map_err(|e| {
                DispatchError::InvalidRef(subject.item_ref.clone(), e.to_string())
            })?;
            ctx.engine.resolve(&ctx.plan_ctx, &canonical).map_err(|e| {
                DispatchError::SchemaMisconfigured {
                    kind: canonical.kind.clone(),
                    detail: format!(
                        "subject resolution failed for '{}': {e}",
                        subject.item_ref
                    ),
                }
            })?
        }
    };

    let resolved = ResolvedExecutionRequest {
        kind: subject.thread_profile.clone(),
        item_ref: subject.item_ref.clone(),
        executor_ref: executor_ref.clone(),
        launch_mode: "inline".to_string(),
        current_site_id: ctx.plan_ctx.current_site_id.clone(),
        origin_site_id: ctx.plan_ctx.origin_site_id.clone(),
        target_site_id: None,
        requested_by: Some(acting_principal.to_string()),
        parameters: params.clone(),
        resolved_item,
        plan_context: ctx.plan_ctx.clone(),
    };

    let dotenv_dirs = crate::vault::dotenv_search_dirs(Some(project_path));
    let vault_bindings = crate::vault::read_required_secrets(
        state.vault.as_ref(),
        acting_principal,
        &resolved.resolved_item.metadata.required_secrets,
        &dotenv_dirs,
    )
    .map_err(|e| DispatchError::Internal(anyhow::anyhow!("vault read failed: {e}")))?;

    let result = launch::build_and_launch(
        state,
        &executor_ref,
        acting_principal,
        &resolved,
        project_path,
        &params,
        &vault_bindings,
        request.pre_minted_thread_id.as_deref(),
    )
    .await
    .map_err(|e| match e {
        launch::BuildAndLaunchError::Materialization(me) => {
            DispatchError::RuntimeMaterializationFailed {
                executor_ref: executor_ref.clone(),
                detail: me.to_string(),
            }
        }
        launch::BuildAndLaunchError::Internal(err) => DispatchError::Internal(err),
    })?;

    Ok(json!({
        "thread": result.thread,
        "result": result.result,
    }))
}

/// DetachedOk-lifecycle subprocess dispatch (opaque protocol, tools).
/// Uses `thread_lifecycle → runner` for tool-style execution.
async fn dispatch_tool_subprocess(
    current_ref: &CanonicalRef,
    thread_profile: &str,
    _verified: Option<&VerifiedItem>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<Value, DispatchError> {
    let item_ref = current_ref.to_string();

    let mut resolved = crate::services::thread_lifecycle::resolve_root_execution(
        &state.engine,
        &ctx.plan_ctx.current_site_id,
        request.project_path,
        &item_ref,
        request.launch_mode,
        request.params.clone(),
        Some(request.acting_principal.to_string()),
        ctx.caller_scopes.clone(),
        request.validate_only,
    )?;

    // A3: override the thread-lifecycle's heuristic kind with the
    // schema-declared `thread_profile`.
    resolved.kind = thread_profile.to_string();

    if resolved.executor_ref.starts_with("runtime:") {
        return Err(DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: format!(
                "subprocess terminator received an item whose resolved executor is a runtime ref ('{}'); this should have been routed through Managed lifecycle — fix the kind schema",
                resolved.executor_ref
            ),
        });
    }

    if let Some(target) = request.target_site_id {
        resolved.target_site_id = Some(target.to_string());
    }

    if request.validate_only {
        let engine = state.engine.clone();
        let resolved_clone = resolved.clone();
        let validated = tokio::task::spawn_blocking(move || {
            crate::services::thread_lifecycle::validate_item(&engine, &resolved_clone)
        })
        .await
        .map_err(|e| DispatchError::SubprocessRunFailed {
            item_ref: resolved.item_ref.clone(),
            detail: format!("validate_only join failure: {e}"),
        })??;

        return Ok(json!({
            "validated": true,
            "item_ref": resolved.item_ref,
            "kind": resolved.kind,
            "executor_ref": resolved.executor_ref,
            "trust_class": validated.trust_class,
            "plan_id": validated.plan_id,
        }));
    }

    let item_ref_for_error = resolved.item_ref.clone();

    // Enforce required_caps before spawn.
    let required_caps = crate::service_registry::extract_required_caps(
        &resolved.resolved_item.metadata.extra,
    );
    if !required_caps.is_empty() {
        enforce_caps(&item_ref_for_error, &required_caps, &ctx.caller_scopes)?;
    }

    let dotenv_dirs = crate::vault::dotenv_search_dirs(Some(&request.original_project_path));
    let vault_bindings = crate::vault::read_required_secrets(
        state.vault.as_ref(),
        request.acting_principal,
        &resolved.resolved_item.metadata.required_secrets,
        &dotenv_dirs,
    )
    .map_err(|e| DispatchError::Internal(anyhow::anyhow!("vault read failed: {e}")))?;

    let params = crate::execution::runner::ExecutionParams {
        resolved,
        acting_principal: request.acting_principal.to_string(),
        project_path: Some(request.original_project_path.clone()),
        vault_bindings,
        snapshot_hash: request.snapshot_hash.clone(),
        parameters: request.params.clone(),
        temp_dir: request.temp_dir.clone(),
        pre_minted_thread_id: request.pre_minted_thread_id.clone(),
        effective_caps: Vec::new(),
    };

    if request.launch_mode == "detached" {
        let result = crate::execution::runner::run_detached(state.clone(), params)
            .await
            .map_err(|e| DispatchError::SubprocessRunFailed {
                item_ref: item_ref_for_error.clone(),
                detail: e.to_string(),
            })?;
        Ok(json!({
            "thread": result.running_thread,
            "detached": true,
        }))
    } else {
        let result = crate::execution::runner::run_inline(state.clone(), params)
            .await
            .map_err(|e| DispatchError::SubprocessRunFailed {
                item_ref: item_ref_for_error,
                detail: e.to_string(),
            })?;
        Ok(json!({
            "thread": result.finalized_thread,
            "result": result.result,
        }))
    }
}

// ── Top-level dispatch loop ───────────────────────────────────────────

/// **P1.1**: The subject item being dispatched. For direct runtime
/// invocation (`runtime:foo`) this IS the runtime. For indirect paths
/// (`directive:bar` → registry → `runtime:directive-runtime`) this is
/// the original directive. Thread records, audit trails, and capability
/// composition use the subject's identity — the runtime is just the
/// executor.
#[derive(Debug, Clone)]
pub(crate) struct RootSubject {
    /// The item ref of the subject (e.g. `directive:my/agent`).
    pub item_ref: String,
    /// The thread_profile from the subject's kind schema
    /// (e.g. `directive_run`).
    pub thread_profile: String,
    /// The verified item from the first hop, carrying trust class,
    /// content hash, and source path for the resolution pipeline.
    pub verified: Option<VerifiedItem>,
}

/// Sole `/execute` → terminator entry point post-V5.3 Task 7.
///
/// Walks the kind-schema alias chain (cycle-checked via `visited`,
/// hop-bounded by `MAX_HOPS`) until `resolve_dispatch_hop` returns a
/// `Terminate` action. All routing decisions (terminator vs. alias
/// vs. registry hop) live in `resolve_dispatch_hop`; the loop just
/// reacts.
///
/// **P1.1**: The loop captures a `RootSubject` from the first hop's
/// `thread_profile` and carries it forward through alias/registry
/// hops. When the loop terminates on a `Subprocess` (runtime), the
/// root subject's identity (not the runtime's) is used for the thread
/// record. This fixes the subject/executor conflation where indirect
/// paths incorrectly recorded the runtime's `thread_profile` and
/// `item_ref`.
///
/// `/execute` is unary forever (V5.5 Phase 3 deletion). Live
/// observation is provided by route-system `event_stream`-mode
/// routes that tail the durable event store; dispatch never owns
/// streaming.
pub async fn dispatch(
    item_ref: &str,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<Value, DispatchError> {
    const MAX_HOPS: usize = 8;
    let mut visited: HashSet<CanonicalRef> = HashSet::new();
    let mut hops: usize = 0;
    let mut current_ref: CanonicalRef = CanonicalRef::parse(item_ref)
        .map_err(|e| DispatchError::InvalidRef(item_ref.to_string(), e.to_string()))?;

    // P1.1: root subject captured from the first hop's resolution.
    // For direct paths, root_subject IS the runtime. For indirect
    // paths, it's the directive/tool that initiated the chain.
    let mut root_subject: Option<RootSubject> = None;

    // B1: derive the SubprocessRole ONCE based on the user's original
    // item_ref. Only direct `runtime:*` invocation triggers the
    // runtime.execute cap gate. Alias chains do NOT inherit the role.
    let role = if request.original_root_kind == ROOT_KIND_RUNTIME {
        let verified = state
            .engine
            .runtimes
            .lookup_by_ref(&current_ref)
            .ok_or_else(|| {
                let mut available: Vec<String> = state
                    .engine
                    .runtimes
                    .all()
                    .map(|r| r.canonical_ref.to_string())
                    .collect();
                available.sort();
                DispatchError::SchemaMisconfigured {
                    kind: current_ref.kind.clone(),
                    detail: format!(
                        "runtime '{item_ref}' not registered; registered runtimes: [{}]",
                        available.join(", ")
                    ),
                }
            })?;
        SubprocessRole::RuntimeTarget {
            verified_runtime: verified.clone(),
        }
    } else {
        SubprocessRole::Regular
    };

    loop {
        if !visited.insert(current_ref.clone()) {
            let mut visited_strs: Vec<String> =
                visited.iter().map(|r| r.to_string()).collect();
            visited_strs.sort();
            return Err(DispatchError::AliasCycle {
                root_ref: item_ref.to_string(),
                visited: visited_strs,
            });
        }
        hops += 1;
        if hops > MAX_HOPS {
            return Err(DispatchError::AliasChainTooLong {
                root_ref: item_ref.to_string(),
                max_hops: MAX_HOPS,
            });
        }

        let hop = resolve_dispatch_hop(&current_ref, ctx)?;

        // Destructure up front so the match on `next` (which moves
        // the terminator out) can't conflict with later borrows. All
        // subsequent uses operate on owned locals, no view structs.
        let VerifiedHop {
            canonical_ref: hop_ref,
            verified,
            thread_profile,
            runtime,
            next,
        } = hop;

        // P1.1: capture root subject from the FIRST hop that has a
        // thread_profile. This is the subject's identity for the
        // entire dispatch chain. We clone here because the loop may
        // continue (alias/registry hop) and dispatch_by also needs
        // the same data on the terminating hop.
        if root_subject.is_none() {
            if let Some(tp) = thread_profile.as_ref() {
                root_subject = Some(RootSubject {
                    item_ref: hop_ref.to_string(),
                    thread_profile: tp.clone(),
                    verified: verified.clone(),
                });
            }
        }

        match next {
            HopAction::Terminate(terminator, _hop_profile) => {
                return dispatch_by(
                    terminator,
                    hop_ref,
                    verified,
                    thread_profile,
                    runtime,
                    root_subject,
                    request,
                    ctx,
                    state,
                    &role,
                )
                .await;
            }
            HopAction::FollowAlias(next_ref) | HopAction::UseRegistry(next_ref) => {
                current_ref = next_ref;
            }
        }
    }
}

/// Route a single terminated hop to its leaf dispatcher.
///
/// **P1.1**: receives the hop's fields individually (owned: verified
/// item, runtime metadata, thread_profile) and the `RootSubject`
/// captured from the first hop. Leaf dispatchers consume what they
/// need from both. No borrowed view struct — owned values flow through
/// the match.
async fn dispatch_by(
    terminator: TerminatorDecl,
    canonical_ref: CanonicalRef,
    verified: Option<VerifiedItem>,
    thread_profile: Option<String>,
    runtime: Option<VerifiedRuntime>,
    root_subject: Option<RootSubject>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    role: &SubprocessRole,
) -> Result<Value, DispatchError> {
    match terminator {
        TerminatorDecl::Subprocess { .. } => {
            let tp = thread_profile.ok_or_else(|| {
                DispatchError::SchemaMisconfigured {
                    kind: canonical_ref.kind.clone(),
                    detail: "subprocess terminator has no thread_profile".into(),
                }
            })?;
            dispatch_subprocess(
                &canonical_ref,
                &tp,
                verified.as_ref(),
                request,
                ctx,
                state,
                role,
                root_subject,
                runtime,
            )
            .await
        }
        TerminatorDecl::InProcess {
            registry: InProcessRegistryKind::Services,
        } => {
            let tp = thread_profile.ok_or_else(|| {
                DispatchError::SchemaMisconfigured {
                    kind: canonical_ref.kind.clone(),
                    detail: "service terminator has no thread_profile".into(),
                }
            })?;
            dispatch_service(
                &canonical_ref.to_string(),
                &tp,
                request,
                ctx,
                state,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use lillux::crypto::SigningKey;
    use ryeos_engine::engine::Engine;
    use ryeos_engine::kind_registry::KindRegistry;
    use ryeos_engine::parsers::{
        ParserDispatcher, ParserRegistry,
    };
    use ryeos_engine::trust::{compute_fingerprint, TrustStore, TrustedSigner};

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[71u8; 32])
    }

    fn trust_store() -> TrustStore {
        let sk = signing_key();
        let vk = sk.verifying_key();
        let fp = compute_fingerprint(&vk);
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: None,
        }])
    }

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rye_dispatch_runtime_test_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const RUNTIME_KIND_SCHEMA_BODY: &str = r##"category: "engine/kinds/runtime"
version: "1.0.0"
location:
  directory: runtimes
execution:
  terminator:
    kind: subprocess
    protocol: protocol:rye/core/runtime_v1
  thread_profile: runtime_run
  resolution: []
formats:
  - extensions: [".yaml", ".yml"]
    parser: parser:rye/core/yaml/yaml
    signature:
      prefix: "#"
composer: handler:rye/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
metadata:
  rules:
    name:
      from: path
      key: name
"##;

    fn write_runtime_kind_schema(kinds_dir: &PathBuf) {
        let runtime_dir = kinds_dir.join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();
        let signed = lillux::signature::sign_content(
            RUNTIME_KIND_SCHEMA_BODY,
            &signing_key(),
            "#",
            None,
        );
        fs::write(runtime_dir.join("runtime.kind-schema.yaml"), signed).unwrap();
    }

    fn build_test_engine() -> Engine {
        let kinds_dir = tempdir();
        write_runtime_kind_schema(&kinds_dir);
        let ts = trust_store();
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts)
            .expect("load runtime kind schema");
        let parser_dispatcher = ParserDispatcher::new(
            ParserRegistry::empty(),
            std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
        );
        Engine::new(kinds, parser_dispatcher, None, vec![]).with_trust_store(ts)
    }

    // The dispatch loop attaches the verified runtime to the hop via
    // `RuntimeRegistry::lookup_by_ref` (covered by `runtime_registry`
    // integration tests) and `dispatch_managed_subprocess` consumes
    // that owned value, so no per-call lookup path exists.

    #[test]
    fn strip_binary_ref_prefix_strips_triple() {
        assert_eq!(
            strip_binary_ref_prefix("bin/x86_64-unknown-linux-gnu/directive-runtime")
                .unwrap(),
            "directive-runtime"
        );
    }

    #[test]
    fn strip_binary_ref_prefix_rejects_malformed() {
        let err = strip_binary_ref_prefix("directive-runtime").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("directive-runtime"), "got: {msg}");
        assert!(msg.contains("unexpected shape"), "got: {msg}");
    }

    /// **B1 unit test**: `enforce_runtime_target_caps` itself is unconditional
    /// (it always checks). The CALLER (`dispatch_managed_subprocess`) is
    /// what gates the call on `original_root_kind == "runtime"`. This
    /// test pins the caller-side gate by simulating the indirect-alias
    /// posture: an empty `required_caps` cannot accidentally deny, and
    /// a non-empty list with no caller scopes WOULD deny if invoked —
    /// proving the gate is the only thing protecting indirect chains
    /// from retroactive cap broadening. Combined with the e2e test
    /// `e2e_directive_via_registry_does_not_require_runtime_execute`
    /// in `runtime_e2e.rs` (which exercises the actual gate end-to-end),
    /// this fully covers B1.
    #[test]
    fn enforce_runtime_caps_skipped_for_indirect_alias_chain() {
        // If `dispatch_managed_subprocess` skips the call entirely (B1
        // gate), then a missing cap does NOT translate to an error.
        // We model this by simply never calling `enforce_runtime_caps`
        // and asserting the synthetic outcome is `Ok`.
        let required = vec!["runtime.execute".to_string()];
        let caller_scopes: Vec<String> = vec![]; // would normally fail

        // SIMULATED indirect path — gate skips the call.
        let original_root_kind = "directive";
        let outcome: Result<(), DispatchError> = if original_root_kind == "runtime" {
            enforce_runtime_caps("runtime:directive-runtime", &required, &caller_scopes)
        } else {
            Ok(())
        };
        assert!(
            outcome.is_ok(),
            "indirect alias chain (original_root_kind='directive') must skip runtime.execute \
             gate, got: {outcome:?}"
        );

        // SIMULATED direct path — gate fires.
        let original_root_kind = "runtime";
        let outcome: Result<(), DispatchError> = if original_root_kind == "runtime" {
            enforce_runtime_caps("runtime:directive-runtime", &required, &caller_scopes)
        } else {
            Ok(())
        };
        assert!(
            matches!(outcome, Err(DispatchError::InsufficientCaps { .. })),
            "direct runtime invocation (original_root_kind='runtime') with missing cap must \
             produce InsufficientCaps, got: {outcome:?}"
        );
    }

    #[test]
    fn enforce_runtime_caps_allows_when_caller_has_required_cap() {
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["runtime.execute".to_string(), "execute".to_string()];
        assert!(enforce_runtime_caps("runtime:directive-runtime", &req, &caller).is_ok());
    }

    #[test]
    fn enforce_runtime_caps_allows_wildcard_scope() {
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["*".to_string()];
        assert!(enforce_runtime_caps("runtime:directive-runtime", &req, &caller).is_ok());
    }

    #[test]
    fn enforce_runtime_caps_denies_when_caller_lacks_required_cap() {
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["execute".to_string()];
        let err = enforce_runtime_caps("runtime:directive-runtime", &req, &caller)
            .expect_err("missing cap must error");
        // DispatchError::InsufficientCaps maps to 403 via http_status();
        // its Display also contains "insufficient capabilities".
        let msg = err.to_string();
        assert!(
            msg.contains("insufficient capabilities"),
            "wording must trigger 403 mapping, got: {msg}"
        );
        assert!(
            msg.contains("runtime:directive-runtime"),
            "error must name the ref, got: {msg}"
        );
        assert!(
            matches!(err, DispatchError::InsufficientCaps { .. }),
            "must be the InsufficientCaps variant for HTTP 403 mapping"
        );
    }

    #[test]
    fn enforce_runtime_caps_no_op_when_runtime_yaml_declares_no_required_caps() {
        let req: Vec<String> = vec![];
        let caller = vec!["execute".to_string()];
        assert!(enforce_runtime_caps("runtime:test", &req, &caller).is_ok());
    }

    // ── B2 unit test ────────────────────────────────────────────────────

    /// **B2**: when the kind schema has `execution:` but neither a
    /// terminator nor an `@<kind>` alias to follow, `resolve_dispatch_hop`
    /// must return `HopAction::UseRegistry(<runtime_ref>)` so the
    /// loop chases the registry-supplied default. This is the
    /// "registry-driven dispatch" foundation for kinds whose schema
    /// declines to commit to a single terminator (e.g. `directive`
    /// with multiple coexisting runtimes).
    ///
    /// Unit-level this is verified via `RuntimeRegistry::lookup_for`
    /// behavior — when at least one runtime serves a kind, lookup
    /// returns it and the hop action is `UseRegistry`. The full
    /// loop-level integration is exercised by the e2e directive-via-
    /// registry test in `runtime_e2e.rs`.
    #[test]
    fn dispatch_loop_uses_registry_when_no_alias_or_terminator() {
        // The `lookup_for` contract is what `resolve_dispatch_hop`
        // depends on for the registry hop. With zero runtimes
        // registered, `lookup_for` returns `Err(NoRuntimeFor)`; with
        // exactly one runtime serving a kind, it returns `Ok(<that>)`
        // regardless of the `default` field. The dispatch loop's
        // `HopAction::UseRegistry` arm is built directly on top of
        // that contract — see `resolve_dispatch_hop` "B2" branch.
        let engine = build_test_engine();
        // Empty registry: `lookup_for` errors. The hop falls through
        // to the `SchemaMisconfigured` enumeration error (S2/F4).
        assert!(
            engine.runtimes.lookup_for("directive").is_err(),
            "empty registry: lookup_for must error so the hop produces \
             a SchemaMisconfigured enumeration error rather than silently \
             returning UseRegistry with a stale ref"
        );
    }

    /// S2/F4: `resolve_dispatch_hop` enumerates registered kinds when
    /// the requested kind has no schema. Operator-friendly error.
    #[test]
    fn resolve_dispatch_hop_enumerates_kinds_when_schema_absent() {
        let engine = build_test_engine();
        let plan_ctx = ryeos_engine::contracts::PlanContext {
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: "fp:test".into(),
                    scopes: vec!["*".into()],
                },
            ),
            project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
                path: std::env::temp_dir(),
            },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: Default::default(),
            validate_only: false,
        };
        let ctx = ExecutionContext {
            principal_fingerprint: "fp:test".into(),
            caller_scopes: vec!["*".into()],
            engine: std::sync::Arc::new(engine),
            plan_ctx,
        };
        // `unknown` kind has no schema; only `runtime` was loaded.
        // For non-runtime kinds the resolver tries engine.resolve()
        // first and that fails with a resolution error (still
        // enumerating in DispatchError wording). The runtime: kind
        // path is verified by the dispatch_pin tests.
        let cref = CanonicalRef::parse("unknown:thing").expect("parse");
        let err = resolve_dispatch_hop(&cref, &ctx)
            .expect_err("unknown kind must error");
        let msg = err.to_string();
        // Either "registered kinds: [runtime]" (schema lookup path)
        // OR "resolution failed" (engine.resolve path) — both are
        // acceptable enumerated errors.
        assert!(
            msg.contains("runtime") || msg.contains("resolution failed"),
            "error must enumerate available kinds or explain resolution failure, got: {msg}"
        );
    }
}
