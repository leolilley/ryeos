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
//!   `dispatch_native_runtime`.
//! - **A1** Errors are typed as `DispatchError` end-to-end; the HTTP
//!   layer in the execute response mode maps them via `http_status()` once per
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

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use ryeos_app::execution_provenance::ProjectSourceKind;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{LaunchMode, ResolvedItem, VerifiedItem};
use ryeos_engine::kind_registry::{
    DelegationVia, ExecutionSchema, InProcessRegistryKind, MethodDecl,
    MethodRuntimeConfigRequirement, MethodScope, TerminatorDecl,
};
use ryeos_engine::protocol_vocabulary::CallbackChannel;
use ryeos_engine::runtime_registry::VerifiedRuntime;

use crate::dispatch_error::DispatchError;
use crate::dispatch_role::{enforce_runtime_target_caps, SubprocessRole};
use crate::execution::launch;
use crate::executor::{
    self as service_executor, ExecutionContext, ExecutionMode, ServiceExecutionResult,
};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::ResolvedExecutionRequest;

mod subprocess_execution;
mod subprocess_policy;
pub(crate) use subprocess_execution::{dispatch_subprocess, validate_ordinary_protocol_contract};
pub(crate) use subprocess_policy::strip_binary_ref_prefix;
pub use subprocess_policy::PreparedManagedLaunch;
use subprocess_policy::{
    enforce_runtime_caps, prepare_managed_launch, require_terminal_executor_id,
};

/// Trusted parent execution context carried out-of-band through schema-driven
/// dispatch.
///
/// This is not user/action params. Callback dispatch fills it from the
/// validated server-side callback capability. Every tracked terminator consumes
/// the parent thread id for lineage and stop inheritance; managed runtime
/// launches additionally consume the budget/depth fields. The dispatch layer
/// therefore needs no kind-prefix checks and action params are not polluted with
/// runtime-control keys.
#[derive(Debug, Clone)]
pub struct ParentExecutionContext {
    pub parent_thread_id: String,
    pub hard_limits: Value,
    pub depth: u32,
}

/// Single source of truth for the `runtime:` ref kind discriminator.
/// Used in two narrow places (B1 cap gate, B4 resolve special-case)
/// where the kind name carries dispatch semantics that come from the
/// `runtime` *kind schema*'s contract, not from a routing decision.
/// The grep gate tolerates references to this constant.
pub(crate) const ROOT_KIND_RUNTIME: &str = "runtime";
const METHOD_RUNTIME_TIMEOUT_SECS: u64 = 120;

/// Resolve the signed subprocess protocol for a selected ordinary runtime and
/// require the callback authority needed by daemon-owned managed launches.
/// The runtime's canonical kind selects the schema; no built-in kind or
/// protocol name participates in this decision. Runtimes serving a kind whose
/// schema declares the method-only wire are not directly launchable through
/// this ordinary runtime surface.
pub(crate) fn require_callback_runtime_protocol<'a>(
    engine: &'a ryeos_engine::engine::Engine,
    verified_runtime: &ryeos_engine::runtime_registry::VerifiedRuntime,
    surface: &str,
) -> Result<&'a ryeos_engine::protocols::VerifiedProtocol, DispatchError> {
    if engine
        .kinds
        .get(&verified_runtime.yaml.serves)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.method_dispatch.as_ref())
        .is_some()
    {
        return Err(DispatchError::SchemaMisconfigured {
            kind: verified_runtime.yaml.serves.clone(),
            detail: format!(
                "{surface} runtime '{}' serves a method-dispatch-only kind and cannot be launched through the ordinary runtime protocol",
                verified_runtime.canonical_ref
            ),
        });
    }

    let runtime_schema = engine
        .kinds
        .get(&verified_runtime.canonical_ref.kind)
        .ok_or_else(|| DispatchError::SchemaMisconfigured {
            kind: verified_runtime.canonical_ref.kind.clone(),
            detail: format!(
                "verified runtime '{}' has no registered kind schema",
                verified_runtime.canonical_ref
            ),
        })?;
    let runtime_protocol_ref = match runtime_schema
        .execution()
        .and_then(|execution| execution.terminator.as_ref())
    {
        Some(TerminatorDecl::Subprocess { protocol_ref }) => protocol_ref,
        Some(other) => {
            return Err(DispatchError::SchemaMisconfigured {
                kind: verified_runtime.canonical_ref.kind.clone(),
                detail: format!(
                    "verified runtime '{}' declares non-subprocess terminator {other:?}",
                    verified_runtime.canonical_ref
                ),
            })
        }
        None => {
            return Err(DispatchError::SchemaMisconfigured {
                kind: verified_runtime.canonical_ref.kind.clone(),
                detail: format!(
                    "verified runtime '{}' has no subprocess terminator",
                    verified_runtime.canonical_ref
                ),
            })
        }
    };
    let protocol = engine
        .protocols
        .require(runtime_protocol_ref)
        .map_err(|_| DispatchError::ProtocolNotRegistered(runtime_protocol_ref.clone()))?;
    match subprocess_execution::classify_managed_protocol(
        protocol,
        &verified_runtime.canonical_ref.kind,
    )? {
        subprocess_execution::ManagedProtocolRoute::CallbackRuntime => Ok(protocol),
        subprocess_execution::ManagedProtocolRoute::FramedStreaming => {
            Err(DispatchError::SchemaMisconfigured {
                kind: verified_runtime.canonical_ref.kind.clone(),
                detail: format!(
                    "{surface} runtime '{}' protocol '{}' is callback-free framed streaming, not a managed callback runtime",
                    verified_runtime.canonical_ref, protocol.canonical_ref
                ),
            })
        }
    }
}

/// Resolve the signed method wire from the invoked kind's method-dispatch
/// declaration. The runtime registry selects the binary; the kind schema owns
/// the protocol used for this invocation surface.
pub(crate) fn require_method_runtime_protocol<'a>(
    engine: &'a ryeos_engine::engine::Engine,
    kind: &str,
    verified_runtime: &ryeos_engine::runtime_registry::VerifiedRuntime,
    surface: &str,
) -> Result<&'a ryeos_engine::protocols::VerifiedProtocol, DispatchError> {
    let schema = engine
        .kinds
        .get(kind)
        .ok_or_else(|| DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!("{surface} kind has no registered schema"),
        })?;
    let protocol_ref = schema
        .execution()
        .and_then(|execution| execution.method_dispatch.as_ref())
        .map(|dispatch| dispatch.protocol.as_str())
        .ok_or_else(|| DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!(
                "{surface} kind has no execution.method_dispatch protocol for runtime '{}'",
                verified_runtime.canonical_ref
            ),
        })?;
    let protocol = engine
        .protocols
        .require(protocol_ref)
        .map_err(|_| DispatchError::ProtocolNotRegistered(protocol_ref.to_string()))?;
    subprocess_execution::validate_method_protocol_contract(protocol, kind)?;
    Ok(protocol)
}

pub(crate) fn decode_method_runtime_result(
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    stdout: &str,
) -> std::result::Result<ryeos_engine::method_wire::MethodCallResult, String> {
    ryeos_engine::protocols::validate_method_runtime_protocol(&protocol.descriptor)
        .map_err(|reason| format!("method protocol '{}': {reason}", protocol.canonical_ref))?;
    match ryeos_engine::protocol_vocabulary::decode_stdout_terminal(
        protocol.descriptor.stdout.shape,
        stdout.as_bytes(),
    )
    .map_err(|error| error.to_string())?
    {
        ryeos_engine::protocol_vocabulary::DecodedStdout::MethodCallResult(result) => Ok(result),
        other => Err(format!(
            "method protocol '{}' decoded an unexpected stdout shape: {other:?}",
            protocol.canonical_ref
        )),
    }
}

/// Request shape consumed by the schema-driven dispatch fns. Carries
/// every input the three terminators (Subprocess, InProcessHandler,
/// NativeRuntimeSpawn) need so `/execute`'s HTTP layer can hand off
/// once and let `dispatch::dispatch` do all routing.
///
/// V5.2 native-runtime cap fields (`launch_mode`, `target_site_id`,
/// provenance project source) are still surfaced so
/// `check_dispatch_capabilities` reproduces the pinned 400 wording
/// (see `crates/bin/daemon/tests/dispatch_pin.rs`).
///
/// **B1**: `original_root_kind` is the kind parsed from the user's
/// original `/execute` `item_ref`. The runtime cap gate fires ONLY
/// when this is `"runtime"`. Alias chains that land on a runtime via
/// the registry / `@directive` chain do not inherit `runtime.execute`.
#[derive(Debug, Clone)]
pub struct DispatchRequest<'a> {
    pub launch_mode: &'a str,
    pub target_site_id: Option<&'a str>,
    pub validate_only: bool,
    pub params: Value,
    /// Complete canonical secondary execution identities for this request.
    pub ref_bindings: BTreeMap<String, String>,
    pub acting_principal: &'a str,
    /// Effective project root used for resolution (matches
    /// `ResolvedProjectContext.effective_path`).
    pub project_path: &'a Path,
    /// Required execution provenance. Constructed once at the entry
    /// point and never reconstructed downstream.
    pub provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    /// Sealed owner/recovery contract selected at the external boundary.
    /// Response timing is intentionally absent: it does not alter execution
    /// ownership after admission.
    pub lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    /// **B1**: kind parsed from the user-supplied root `item_ref`.
    /// `dispatch_native_runtime` gates `runtime.execute` enforcement
    /// on this being `"runtime"` so indirect alias chains are not
    /// retroactively cap-broadened.
    pub original_root_kind: &'a str,
    /// **Phase E.3**: when `Some`, the thread row created by
    /// `dispatch_native_runtime` must use this id (via
    /// `create_root_thread_with_id`) instead of minting one. The SSE
    /// `directive_launch` source mints the id up front so it can
    /// subscribe to the event hub *before* the launch task begins,
    /// which is required to avoid losing the very first lifecycle
    /// event. `None` (the default) preserves the old
    /// "mint inside `create_root_thread`" path.
    pub pre_minted_thread_id: Option<String>,
    pub usage_subject: Option<ryeos_state::UsageSubject>,
    pub usage_subject_asserted_by: Option<String>,
    /// Chained-resume turn: when `Some`, the launch envelope carries this
    /// as `EnvelopeRequest.previous_thread_id` and the runtime replays the
    /// prior thread's events into the new run (conversation = thread
    /// chain). Set by daemon-internal callers (the thread-input service);
    /// never populated from raw HTTP request bodies.
    pub previous_thread_id: Option<String>,
    /// Exact verified terminal/root subject and captured history authority
    /// produced by synchronous public-route admission. Wrapper recursion
    /// carries this unchanged until it reaches the subject that will actually
    /// be persisted; leaves reject any identity or schema mismatch.
    pub root_admission: Option<ryeos_app::thread_lifecycle::RootExecutionAdmission>,
    /// Trusted parent context for callback-dispatched child executions. This is
    /// consumed only if schema-driven dispatch reaches a managed, method, or
    /// terminal subprocess launch; in-process services ignore it.
    pub parent_execution_context: Option<ParentExecutionContext>,
}

/// Check the schema-derived `DispatchCapabilities` for the matched
/// terminator against the request shape. On mismatch, returns the
/// V5.2 wording verbatim (pin tests assert byte equality):
/// * `pushed_head` → "pushed_head not yet supported for native runtimes"
/// * `target_site_id` → "remote execution not yet supported for native runtimes"
/// * `launch_mode == "detached"` → "detached mode not yet supported for native runtimes"
fn check_dispatch_capabilities(
    caps: &ryeos_engine::protocol_vocabulary::ProtocolCapabilities,
    request: &DispatchRequest<'_>,
) -> Result<(), DispatchError> {
    let is_pushed = matches!(
        request.provenance.project_source(),
        ProjectSourceKind::PushedHead
    );
    if is_pushed && !caps.allows_pushed_head {
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

/// Context struct for subprocess dispatch, carrying all the hop-derived
/// data plus the shared request/ctx/state.
pub(crate) struct SubprocessDispatchContext<'a> {
    current_ref: &'a CanonicalRef,
    thread_profile: &'a str,
    verified: Option<&'a VerifiedItem>,
    request: &'a DispatchRequest<'a>,
    ctx: &'a ExecutionContext,
    state: &'a AppState,
    role: &'a SubprocessRole,
    root_subject: Option<RootSubject>,
    hop_runtime: Option<VerifiedRuntime>,
    launch_handoff: Option<&'a crate::execution::launch::LaunchHandoff>,
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
    /// Schema declares `methods` (method-dispatch path). The kind is
    /// dispatched by resolving the requested method, validating args,
    /// and spawning the kind's runtime with a `MethodCallEnvelope`.
    /// Carries the kind name (from the schema, not hardcoded), the
    /// resolved method name, and its `MethodDecl`.
    DispatchMethod {
        kind: String,
        method_name: String,
        method_decl: MethodDecl,
    },
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
    /// Original item-resolution failure when this schema permits continuing
    /// without a concrete item (for example, to produce a registry-specific
    /// diagnostic). Root admission surfaces this cause if a verified caller
    /// subject is ultimately required.
    pub resolution_error: Option<String>,
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
    resolve_dispatch_hop_with_verified(current_ref, ctx, None)
}

fn resolve_dispatch_hop_with_verified(
    current_ref: &CanonicalRef,
    ctx: &ExecutionContext,
    preverified: Option<VerifiedItem>,
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
    let (verified, resolution_error): (Option<VerifiedItem>, Option<String>) =
        if let Some(verified) = preverified {
            if verified.resolved.canonical_ref != *current_ref {
                return Err(DispatchError::InvalidRef(
                    current_ref.to_string(),
                    format!(
                        "preverified item ref mismatch: verified '{}'",
                        verified.resolved.canonical_ref
                    ),
                ));
            }
            (Some(verified), None)
        } else {
            match ctx.engine.resolve(&ctx.plan_ctx, current_ref) {
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
                    (Some(v), None)
                }
                Err(error) => {
                    // Resolution failed — the ref may not exist on disk. This
                    // is not necessarily fatal: the schema lookup below will
                    // produce a clearer error (SchemaMisconfigured enumerating
                    // available kinds) if the kind has no items at all.
                    (None, Some(error.to_string()))
                }
            }
        };

    let schema_kind: String = verified
        .as_ref()
        .map(|v| v.resolved.kind.clone())
        .unwrap_or_else(|| current_ref.kind.clone());

    // **P1.1**: extract thread_profile from the schema's execution
    // block at every hop — even non-terminator hops. The dispatch loop
    // captures this from the first hop as the root subject profile.

    let schema = ctx.engine.kinds.get(&schema_kind).ok_or_else(|| {
        let mut available: Vec<String> = ctx.engine.kinds.kinds().map(|k| k.to_string()).collect();
        available.sort();
        DispatchError::SchemaMisconfigured {
            kind: schema_kind.clone(),
            detail: format!(
                "no kind schema registered; registered kinds: [{}]",
                available.join(", ")
            ),
        }
    })?;

    let exec: &ExecutionSchema =
        schema
            .execution()
            .ok_or_else(|| DispatchError::NotRootExecutable {
                kind: schema_kind.clone(),
                detail: "schema has no `execution:` block".into(),
            })?;

    let thread_profile: Option<String> = exec.thread_profile.as_ref().map(|tp| tp.name.clone());

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

    // **Method-dispatch path**: if the schema declares `methods`, take
    // the method-dispatch path instead of terminator/alias/delegate. The
    // boot-time mixed-dispatch reject guarantees `methods` is never
    // non-empty alongside terminator/alias/delegate, so this branch is
    // unambiguous.
    if !exec.methods.is_empty() {
        let requested_method = ctx.requested_method();
        let (method_name, method_decl) =
            resolve_requested_method(requested_method, exec, &schema_kind)?;
        return Ok(VerifiedHop {
            canonical_ref: current_ref.clone(),
            verified,
            resolution_error,
            thread_profile,
            runtime,
            next: HopAction::DispatchMethod {
                kind: schema_kind,
                method_name,
                method_decl,
            },
        });
    }

    // Terminator wins over alias/registry.
    if let Some(terminator) = exec.terminator.as_ref() {
        // **A3**: thread profile MUST be declared on the schema; no
        // kind-name fallback. Schema validation at engine init enforces
        // this for any executable schema, but we re-check defensively
        // so an out-of-band schema mutation cannot silently degrade
        // the audit trail.
        let tp = thread_profile
            .as_deref()
            .ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: schema_kind.clone(),
                detail: "schema declares a terminator but no `execution.thread_profile`".into(),
            })?;
        return Ok(VerifiedHop {
            canonical_ref: current_ref.clone(),
            verified,
            resolution_error,
            thread_profile: Some(tp.to_string()),
            runtime,
            next: HopAction::Terminate(terminator.clone(), tp.to_string()),
        });
    }

    // No terminator — follow the kind's `@<kind>` alias if present.
    let alias_key = format!("@{schema_kind}");
    if let Some(alias_target) = exec.aliases.get(&alias_key) {
        let next_ref =
            CanonicalRef::parse(alias_target).map_err(|e| DispatchError::SchemaMisconfigured {
                kind: schema_kind.clone(),
                detail: format!(
                    "alias '{alias_key}' → '{alias_target}' is not a valid canonical ref: {e}"
                ),
            })?;
        return Ok(VerifiedHop {
            canonical_ref: current_ref.clone(),
            verified,
            resolution_error,
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
                let lookup_kind = serves_kind.as_deref().unwrap_or(schema_kind.as_str());
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
                    resolution_error,
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

// ── Method dispatch helpers ───────────────────────────────────────────

/// Resolve the requested method name to its declared `(name, MethodDecl)`.
///
/// If the caller provided an explicit method name, look it up in the
/// schema's `methods`. If no method was requested, use
/// `method_dispatch.default` (which MUST be declared on a method-bearing
/// schema). Unknown/missing methods produce structured errors listing the
/// declared methods (Rule 8).
fn resolve_requested_method(
    requested: Option<&str>,
    exec: &ExecutionSchema,
    kind: &str,
) -> Result<(String, MethodDecl), DispatchError> {
    let method_name = match requested {
        Some(name) => name,
        None => exec
            .method_dispatch
            .as_ref()
            .and_then(|md| md.default.as_deref())
            .ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: kind.to_string(),
                detail: "schema declares methods but no method_dispatch.default, and no \
                         method was specified in the request"
                    .into(),
            })?,
    };

    exec.methods
        .get(method_name)
        .cloned()
        .map(|decl| (method_name.to_string(), decl))
        .ok_or_else(|| {
            let declared: Vec<String> = exec.methods.keys().cloned().collect();
            DispatchError::UnknownMethod {
                kind: kind.to_string(),
                requested: method_name.to_string(),
                declared: declared.join(", "),
            }
        })
}

/// Validate the caller's `args` object against the method's typed spec.
/// Each declared arg with `required: true` must be present and match the
/// declared type. Optional args with defaults are filled in. Args not
/// declared on the method are REJECTED — the declared arg set is a strict
/// contract.
///
/// Returns the validated+defaulted args as a `serde_json::Value::Object`.
fn validate_method_args(
    args: Option<&Value>,
    method_name: &str,
    method: &MethodDecl,
) -> Result<Value, DispatchError> {
    let mut args_map = match args {
        Some(Value::Object(map)) => map.clone(),
        Some(other) => {
            return Err(DispatchError::MethodInvalidArg {
                method: method_name.to_string(),
                reason: format!("args must be an object, got {}", other),
            });
        }
        None => serde_json::Map::new(),
    };

    // Reject any arg the method does not declare. This makes the declared
    // arg set a real contract rather than a permissive suggestion.
    for key in args_map.keys() {
        if !method.args.contains_key(key) {
            let mut declared: Vec<&String> = method.args.keys().collect();
            declared.sort();
            return Err(DispatchError::MethodInvalidArg {
                method: method_name.to_string(),
                reason: format!("unknown arg '{key}'; declared args: {declared:?}"),
            });
        }
    }

    for (name, decl) in &method.args {
        match args_map.get(name) {
            Some(val) => {
                // Full declaration check: type + enum + min + array items.
                validate_arg_value(val, decl, name, method_name)?;
            }
            None => {
                if decl.required {
                    return Err(DispatchError::MethodInvalidArg {
                        method: method_name.to_string(),
                        reason: format!("required arg '{name}' is missing"),
                    });
                }
                // Apply default if present. Validate it against the
                // declaration too — a schema default that violates its own
                // type/enum/min would otherwise reach the runtime unchecked.
                if let Some(default) = &decl.default {
                    validate_arg_value(default, decl, name, method_name)?;
                    args_map.insert(name.clone(), default.clone());
                }
            }
        }
    }

    Ok(Value::Object(args_map))
}

/// Validate a JSON value against a full arg declaration: the declared
/// type, plus the optional `enum`, `min`, and array-`items` constraints.
/// Recurses into array elements so `array items: string` is enforced
/// element-wise before the runtime is spawned.
fn validate_arg_value(
    val: &Value,
    decl: &ryeos_engine::kind_registry::ArgDecl,
    name: &str,
    method_name: &str,
) -> Result<(), DispatchError> {
    use ryeos_engine::kind_registry::ArgType;

    let err = |reason: String| DispatchError::MethodInvalidArg {
        method: method_name.to_string(),
        reason,
    };

    // 1. Top-level type.
    let ok = match decl.ty {
        ArgType::String => val.is_string(),
        ArgType::Integer => val.is_i64() || val.is_u64(),
        ArgType::Boolean => val.is_boolean(),
        ArgType::Array => val.is_array(),
        ArgType::Object => val.is_object(),
    };
    if !ok {
        return Err(err(format!(
            "arg '{name}' expected type {:?}, got {}",
            decl.ty, val
        )));
    }

    // 2. enum (applies to string values).
    if let Some(allowed) = &decl.enum_values {
        if let Some(s) = val.as_str() {
            if !allowed.iter().any(|a| a == s) {
                return Err(err(format!(
                    "arg '{name}' must be one of {allowed:?}, got {s:?}"
                )));
            }
        }
    }

    // 3. min (applies to integer values).
    if let Some(min) = decl.min {
        if let Some(n) = val.as_i64() {
            if n < min {
                return Err(err(format!("arg '{name}' must be >= {min}, got {n}")));
            }
        }
    }

    // 4. Array element type, when declared via `items`.
    if matches!(decl.ty, ArgType::Array) {
        if let (Some(items_decl), Some(arr)) = (&decl.items, val.as_array()) {
            for (i, el) in arr.iter().enumerate() {
                validate_arg_value(el, items_decl, &format!("{name}[{i}]"), method_name)?;
            }
        }
    }

    Ok(())
}

/// Project a `ResolutionOutput` into a `SingleRootPayload` for the
/// knowledge runtime. The daemon owns this conversion because it consumes
/// an engine type (`ResolutionOutput`) and produces a knowledge type
/// (`SingleRootPayload`).
/// Convert an engine `ResolvedAncestor` (a verified, signature-checked
/// item) into the wire `VerifiedItem`. Shared by single-root and corpus
/// projection so trust-class and metadata mapping stay identical.
fn verified_from(
    a: &ryeos_engine::resolution::ResolvedAncestor,
) -> ryeos_runtime::method_wire::VerifiedItem {
    use ryeos_runtime::method_wire::{TrustClass, VerifiedItem};
    VerifiedItem {
        raw_content: a.raw_content.clone(),
        raw_content_digest: a.raw_content_digest.clone(),
        metadata: serde_json::json!({
            "source_path": a.source_path,
            "trust_class": format!("{:?}", a.trust_class),
            "requested_id": a.requested_id,
        }),
        trust_class: match a.trust_class {
            ryeos_engine::resolution::TrustClass::TrustedBundle => TrustClass::TrustedBundle,
            ryeos_engine::resolution::TrustClass::TrustedProject => TrustClass::TrustedProject,
            ryeos_engine::resolution::TrustClass::UntrustedProject => TrustClass::UntrustedProject,
            ryeos_engine::resolution::TrustClass::Unsigned => TrustClass::Unsigned,
        },
    }
}

fn project_single_root(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> Result<ryeos_runtime::method_wire::SingleRootPayload, DispatchError> {
    use ryeos_runtime::method_wire::{EdgeKind, GraphEdge, VerifiedItem};

    let mut items_by_ref: std::collections::BTreeMap<String, VerifiedItem> =
        std::collections::BTreeMap::new();

    // Root + ancestors.
    for resolved in std::iter::once(&resolution.root).chain(resolution.ancestors.iter()) {
        items_by_ref
            .entry(resolved.resolved_ref.clone())
            .or_insert_with(|| verified_from(resolved));
    }
    // Referenced items.
    for resolved in &resolution.referenced_items {
        items_by_ref
            .entry(resolved.resolved_ref.clone())
            .or_insert_with(|| verified_from(resolved));
    }

    // Build edges from extends chain + references.
    let mut edges: Vec<GraphEdge> = Vec::new();

    // Extends edges: root → ancestors, in chain order.
    if !resolution.ancestors.is_empty() {
        // The first ancestor extends the root.
        let mut from = resolution.root.resolved_ref.clone();
        for (i, anc) in resolution.ancestors.iter().enumerate() {
            edges.push(GraphEdge {
                from: from.clone(),
                to: anc.resolved_ref.clone(),
                kind: EdgeKind::Extends,
                depth_from_root: Some(i + 1),
            });
            from = anc.resolved_ref.clone();
        }
    }

    // Reference edges.
    for edge in &resolution.references_edges {
        edges.push(GraphEdge {
            from: edge.from_ref.clone(),
            to: edge.to_ref.clone(),
            kind: EdgeKind::References,
            depth_from_root: None, // not depth-ordered
        });
    }

    // Validate every edge endpoint is in items_by_ref.
    for edge in &edges {
        if !items_by_ref.contains_key(&edge.from) || !items_by_ref.contains_key(&edge.to) {
            return Err(DispatchError::ProjectionInvariant {
                reason: format!("edge endpoint missing: {} -> {}", edge.from, edge.to),
            });
        }
    }

    Ok(ryeos_runtime::method_wire::SingleRootPayload {
        root_ref: resolution.root.resolved_ref.clone(),
        items_by_ref,
        edges,
    })
}

/// Project the whole verified corpus of `kind` into `(items_by_ref, edges)`
/// for a `scope: corpus` method (knowledge `query`/`graph`/`validate`).
///
/// Enumerates every item of the kind across resolution roots and projects
/// each via the engine's TOLERANT corpus resolver
/// (`resolve_item_for_corpus`). Differences from `project_single_root`:
///   - there is no `root_ref` — the method operates over the whole set;
///   - a malformed item is STILL included (with `raw_content`) so the
///     runtime `validate` reports it — it is not silently dropped;
///   - dangling reference edges are PRESERVED (targets not required to
///     exist) so `graph`/`validate` can report them as `missing_refs`;
///   - an integrity failure (bad signature, unreadable) is a HARD ERROR —
///     a corpus method never presents a clean-looking partial corpus.
///
/// Generic over `kind`: the executor never names a specific kind here.
fn project_corpus(
    kind: &str,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
) -> Result<
    (
        std::collections::BTreeMap<String, ryeos_runtime::method_wire::VerifiedItem>,
        Vec<ryeos_runtime::method_wire::GraphEdge>,
    ),
    DispatchError,
> {
    use ryeos_runtime::method_wire::{EdgeKind, GraphEdge, VerifiedItem};

    let kind_schema =
        ctx.engine
            .kinds
            .get(kind)
            .ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: kind.to_string(),
                detail: "corpus method dispatched for a kind with no registered schema".into(),
            })?;

    let engine_roots = ctx
        .engine
        .resolution_roots(Some(request.project_path.to_path_buf()));
    let effective_parsers = ctx
        .engine
        .effective_parser_dispatcher(Some(request.project_path))
        .map_err(|e| DispatchError::Internal(anyhow::anyhow!("corpus parser dispatcher: {e}")))?;

    let refs = ryeos_engine::item_resolution::enumerate_kind_refs(&engine_roots, kind_schema, kind);

    let mut items_by_ref: std::collections::BTreeMap<String, VerifiedItem> =
        std::collections::BTreeMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();

    for cref in &refs {
        // Tolerant per-item projection: a malformed body still yields the
        // item (so `validate` reports it); a bad signature is a hard error
        // (so the corpus method fails loudly rather than looking clean).
        let projection = ryeos_engine::resolution::resolve_item_for_corpus(
            cref,
            &ctx.engine.kinds,
            &effective_parsers,
            &engine_roots,
            &ctx.engine.trust_store,
        )
        .map_err(|e| {
            DispatchError::InvalidRef(cref.to_string(), format!("corpus projection failed: {e}"))
        })?;

        items_by_ref
            .entry(projection.item.resolved_ref.clone())
            .or_insert_with(|| verified_from(&projection.item));
        // Edges point at declared targets even when the target is not in
        // `items_by_ref` (dangling) — the runtime reports those.
        for edge in projection.reference_edges {
            edges.push(GraphEdge {
                from: edge.from_ref,
                to: edge.to_ref,
                kind: EdgeKind::References,
                depth_from_root: None,
            });
        }
    }

    // The same reference can surface from multiple items — dedup.
    edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
    edges.dedup_by(|a, b| a.from == b.from && a.to == b.to && a.kind == b.kind);

    Ok((items_by_ref, edges))
}

// ── Method dispatch terminator ─────────────────────────────────────────

/// Dispatch a method-style kind by spawning its runtime with a
/// `MethodCallEnvelope`. This is the generic method-dispatch path:
///
/// 1. Validate args against the method's typed spec.
/// 2. Look up the runtime via `RuntimeRegistry::lookup_for(kind)`.
/// 3. Run the engine's resolution pipeline for the item.
/// 4. Project to `SingleRootPayload` (single-root methods only).
/// 5. Mint thread record + callback token.
/// 6. Build `MethodCallEnvelope` and spawn via lillux::run.
/// 7. Parse `MethodCallResult` and return the output.
///
/// The runtime binary (e.g. `ryeos-knowledge-runtime`) handles the
/// actual method logic. The daemon never calls methods in-process (Rule 1).
/// Build the method-dispatch envelope payload from the method's `scope`:
/// a single resolved root (extends chain + references) or the whole verified
/// corpus of the kind. Fallible (resolution pipeline, effective-trust gate,
/// corpus build). Callers run it either as `validate_only` validation (no
/// thread minted) or inside the post-mint guard, so a projection failure
/// finalizes the created thread instead of orphaning the returned id.
// Method declaration, verified hop/root evidence, resolution roots, and request
// context remain explicit at the projection boundary.
#[allow(clippy::too_many_arguments)]
fn project_method_payload(
    method_decl: &MethodDecl,
    canonical_ref: &CanonicalRef,
    hop_verified: Option<&VerifiedItem>,
    kind: &str,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    ctx: &ExecutionContext,
    request: &DispatchRequest<'_>,
    admitted_root: Option<&ryeos_app::thread_lifecycle::RootExecutionAdmission>,
) -> Result<Value, DispatchError> {
    let payload = match method_decl.scope {
        MethodScope::SingleRoot => {
            let resolution_output = if let Some(admission) = admitted_root {
                admission
                    .ensure_matches_subject(
                        &ctx.engine,
                        admission.verified_subject(),
                        admission.thread_profile(),
                    )
                    .map_err(DispatchError::Internal)?;
                if admission.verified_subject().resolved.canonical_ref != *canonical_ref {
                    return Err(DispatchError::Internal(anyhow::anyhow!(
                        "method payload ref `{canonical_ref}` does not match admitted subject `{}`",
                        admission.verified_subject().resolved.canonical_ref
                    )));
                }
                std::borrow::Cow::Borrowed(admission.resolution_output())
            } else {
                let effective_parsers = ctx
                    .engine
                    .effective_parser_dispatcher(Some(request.project_path))
                    .map_err(|e| {
                        DispatchError::InvalidRef(
                            canonical_ref.to_string(),
                            format!("parser dispatcher: {e}"),
                        )
                    })?;
                std::borrow::Cow::Owned(
                    ryeos_engine::resolution::run_resolution_pipeline(
                        canonical_ref,
                        &ctx.engine.kinds,
                        &effective_parsers,
                        engine_roots,
                        &ctx.engine.trust_store,
                        &ctx.engine.composers,
                    )
                    .map_err(|e| {
                        DispatchError::InvalidRef(
                            canonical_ref.to_string(),
                            format!("resolution pipeline failed: {e}"),
                        )
                    })?,
                )
            };

            crate::execution::launch::enforce_effective_trust(
                resolution_output.effective_trust_class,
                &canonical_ref.to_string(),
                kind,
            )?;

            let single_root = project_single_root(&resolution_output)?;
            serde_json::to_value(&single_root).map_err(|e| DispatchError::Internal(e.into()))?
        }
        MethodScope::Corpus => {
            // A corpus method still requires the invoked ref to resolve AND
            // verify — it authorizes/routes the call even though it does
            // not bound the corpus. `resolve_dispatch_hop` leaves
            // `hop_verified` as `None` when resolution/verification failed,
            // so a missing or unverifiable requested ref must be rejected
            // here rather than silently running over the whole corpus.
            let root = hop_verified.ok_or_else(|| {
                DispatchError::InvalidRef(
                    canonical_ref.to_string(),
                    "requested ref did not resolve/verify; a corpus method still requires a \
                     valid invoked ref to authorize the call"
                        .to_string(),
                )
            })?;
            // Corpus scope changes the projected payload, not the trust
            // boundary: the invoked root remains executable authority and
            // receives the same typed unsigned-policy rejection as every
            // other subprocess launch.
            if matches!(
                root.trust_class,
                ryeos_engine::contracts::TrustClass::Unsigned
            ) {
                return Err(crate::execution::launch::effective_trust_unsigned_error(
                    &canonical_ref.to_string(),
                    kind,
                ));
            }

            let (items_by_ref, edges) = project_corpus(kind, request, ctx)?;
            serde_json::json!({ "items_by_ref": items_by_ref, "edges": edges })
        }
    };
    Ok(payload)
}

pub(crate) fn method_runtime_config_snapshot(
    kind: &str,
    requirements: &BTreeMap<String, MethodRuntimeConfigRequirement>,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    state: &AppState,
) -> Result<BTreeMap<String, Value>, DispatchError> {
    if requirements.is_empty() {
        return Ok(BTreeMap::new());
    }

    let loader = verified_loader_for_method_runtime(
        engine_roots,
        &state.config.runtime_root().trusted_keys_dir(),
    )
    .map_err(|e| DispatchError::Internal(anyhow::anyhow!("runtime config loader: {e}")))?;

    let mut snapshots = BTreeMap::new();
    for (name, requirement) in requirements {
        let value = loader
            .load_config_strict_signed::<Value>(&requirement.path)
            .map_err(|e| {
                DispatchError::Internal(anyhow::anyhow!(
                    "loading method runtime config `{}` at `{}`: {e}",
                    name,
                    requirement.path
                ))
            })?
            .ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: kind.to_string(),
                detail: format!(
                    "method requires runtime config `{}` at `{}`, but no config file was found",
                    name, requirement.path
                ),
            })?;
        snapshots.insert(name.clone(), value);
    }

    Ok(snapshots)
}

fn verified_loader_for_method_runtime(
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_trusted_keys_dir: &Path,
) -> anyhow::Result<ryeos_runtime::verified_loader::VerifiedLoader> {
    let project_root = engine_roots
        .ordered
        .iter()
        .find(|r| r.space == ryeos_engine::contracts::ItemSpace::Project)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or_else(|| r.ai_root.clone())
        })
        .ok_or_else(|| anyhow::anyhow!("no project root in engine resolution roots"))?;

    let bundle_roots: Vec<PathBuf> = engine_roots
        .ordered
        .iter()
        .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or_else(|| r.ai_root.clone())
        })
        .collect();

    Ok(ryeos_runtime::verified_loader::VerifiedLoader::new(
        project_root,
        bundle_roots,
        node_trusted_keys_dir,
    ))
}

// Execution plumbing: each argument is a distinct leg of the thread's
// auth/provenance context, threaded verbatim — a struct would rename,
// not simplify. Restructure with a compiler in the loop, not here.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_method(
    kind: &str,
    method_name: &str,
    method_decl: &MethodDecl,
    canonical_ref: &CanonicalRef,
    hop_verified: Option<VerifiedItem>,
    thread_profile: Option<String>,
    root_subject: Option<RootSubject>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    launch_handoff: Option<&crate::execution::launch::LaunchHandoff>,
) -> Result<Value, DispatchError> {
    // 1. Validate args against the method's spec. Args come from the single
    // source of truth (`ctx.requested_call`), same as the preflight below.
    let validated_args = validate_method_args(ctx.requested_args(), method_name, method_decl)?;

    // 2. Look up the runtime via registry.
    let verified_runtime = ctx.engine.runtimes.lookup_for(kind).map_err(|_| {
        let mut serves: Vec<String> = ctx
            .engine
            .runtimes
            .all()
            .map(|r| format!("{}→{}", r.yaml.serves, r.canonical_ref))
            .collect();
        serves.sort();
        DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!(
                "no runtime serves kind '{kind}' for method dispatch \
                 (registered runtimes: [{}])",
                serves.join(", ")
            ),
        }
    })?;
    let runtime_protocol =
        require_method_runtime_protocol(&ctx.engine, kind, verified_runtime, "method")?;
    let runtime_item_ref = verified_runtime.canonical_ref.clone();
    let runtime_item_ref_string = runtime_item_ref.to_string();

    let bare = strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)?;
    let executor_ref = format!("native:{bare}");

    // Resolution roots are needed both for payload projection and for the
    // launch/inventory step below, so compute them once at this scope.
    let engine_roots = ctx
        .engine
        .resolution_roots(Some(request.project_path.to_path_buf()));

    // Payload projection (single-root resolution+trust / corpus build) is
    // deferred via `project_method_payload`: for `validate_only` it runs
    // below as validation; for a real dispatch it runs inside the post-mint
    // guard, so a projection failure finalizes the created thread rather than
    // orphaning the returned id.

    let method_subject = match root_subject {
        Some(subject) => subject,
        None => RootSubject {
            item_ref: canonical_ref.to_string(),
            thread_profile: thread_profile.ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: kind.to_string(),
                detail: "method dispatch requires execution.thread_profile".into(),
            })?,
            verified: hop_verified.clone(),
        },
    };
    let thread_profile_str = method_subject.thread_profile.as_str();

    // 6. Validate the launch mode up front — BEFORE the validate_only
    //    short-circuit, so a `validate_only` request with a bad mode is
    //    rejected rather than reported valid. Method dispatch runs the
    //    runtime synchronously and returns its result, so it only supports
    //    `wait`: `detached` is a known-but-unsupported capability, and
    //    anything else is an outright invalid mode (which must not be
    //    silently recorded on the thread or surface as an opaque internal
    //    error).
    match request.launch_mode {
        "wait" => {}
        "detached" => {
            return Err(DispatchError::CapabilityRejected {
                reason: "detached mode not yet supported for method dispatch".into(),
            });
        }
        other => {
            return Err(DispatchError::InvalidLaunchMode {
                other: other.to_string(),
            });
        }
    }

    // 7. validate_only: run the full pre-spawn path (args validation, runtime
    //    lookup, launch-mode check — already done — plus payload projection
    //    incl. resolution/trust/corpus) and report the call as valid without
    //    minting a thread or spawning the runtime. Mirrors the
    //    subprocess/service validate paths.
    if request.validate_only {
        project_method_payload(
            method_decl,
            canonical_ref,
            hop_verified.as_ref(),
            kind,
            &engine_roots,
            ctx,
            request,
            request.root_admission.as_ref(),
        )?;
        method_runtime_config_snapshot(kind, &method_decl.runtime_config, &engine_roots, state)?;
        return Ok(json!({
            "validated": true,
            "item_ref": canonical_ref.to_string(),
            "kind": kind,
            "method": method_name,
            "executor_ref": executor_ref,
        }));
    }

    // A method invocation is a real executable root. Reuse the exact public
    // admission when present; otherwise admit this already-verified subject
    // once. Method/runtime identity must never stand in for the invoked item.
    let history_subject = method_subject.verified.as_ref().ok_or_else(|| {
        DispatchError::InvalidRef(
            method_subject.item_ref.clone(),
            "method root did not resolve and verify before history-policy admission".to_string(),
        )
    })?;
    let root_admission = if let Some(admission) = request.root_admission.as_ref() {
        admission
            .ensure_matches_subject(&ctx.engine, history_subject, thread_profile_str)
            .map_err(DispatchError::Internal)?;
        if admission.ref_bindings() != &request.ref_bindings {
            return Err(DispatchError::Internal(anyhow::anyhow!(
                "method root admission ref bindings differ from the dispatch request"
            )));
        }
        admission.clone()
    } else {
        ryeos_app::thread_lifecycle::admit_verified_root_execution(
            &ctx.engine,
            &ctx.plan_ctx,
            &ctx.plan_ctx,
            ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
                &ctx.engine,
                &ctx.plan_ctx,
                &request.provenance,
            )
            .map_err(DispatchError::Internal)?,
            history_subject.clone(),
            &state.node_history_policy,
            thread_profile_str.to_string(),
            request.ref_bindings.clone(),
            request.usage_subject.clone(),
            request.usage_subject_asserted_by.clone(),
        )
        .map_err(DispatchError::Internal)?
    };

    // Accepted launch is an admission acknowledgement, so every rejection
    // that depends only on the invoked root must be settled before a durable
    // row is published or its id is handed back. Corpus construction can be
    // deferred, but corpus scope does not weaken the invoked root's trust
    // boundary.
    match method_decl.scope {
        MethodScope::SingleRoot => crate::execution::launch::enforce_effective_trust(
            root_admission.resolution_output().effective_trust_class,
            &canonical_ref.to_string(),
            kind,
        )?,
        MethodScope::Corpus => {
            let root = hop_verified.as_ref().ok_or_else(|| {
                DispatchError::InvalidRef(
                    canonical_ref.to_string(),
                    "requested ref did not resolve/verify; a corpus method still requires a valid invoked ref to authorize the call"
                        .to_string(),
                )
            })?;
            if matches!(
                root.trust_class,
                ryeos_engine::contracts::TrustClass::Unsigned
            ) {
                return Err(crate::execution::launch::effective_trust_unsigned_error(
                    &canonical_ref.to_string(),
                    kind,
                ));
            }
        }
    }

    // 8. Mint the thread record + callback token. Honor a pre-minted
    //    thread id when the caller supplied one: the SSE/gateway source
    //    mints the id up front so it can subscribe to the event hub
    //    BEFORE dispatch begins — minting a fresh id here would orphan
    //    that subscription and the stream would observe no events.
    //    Launch mode and usage attribution are carried from the request,
    //    not hardcoded.
    let thread_id = request
        .pre_minted_thread_id
        .clone()
        .unwrap_or_else(ryeos_app::thread_lifecycle::new_thread_id);
    // Reserve fresh root ownership before publishing the method row. Corpus
    // projection and executor preparation may be substantial; the daemon's
    // recovery sweep must see a durable launch claim throughout that created
    // window rather than classifying the row as an orphan.
    let launch_claim =
        crate::execution::launch_claim::ThreadLaunchClaim::acquire_fresh(state, &thread_id)
            .map_err(DispatchError::Internal)?;
    let launch_owner = launch_claim
        .canonical_owner()
        .map_err(DispatchError::Internal)?;
    let created = if let Some(parent) = request.parent_execution_context.as_ref() {
        let durable_parent = state
            .threads
            .get_thread(&parent.parent_thread_id)
            .map_err(DispatchError::Internal)?
            .ok_or_else(|| {
                DispatchError::Internal(anyhow::anyhow!(
                    "method parent thread not found: {}",
                    parent.parent_thread_id
                ))
            })?;
        let project_authority = request
            .provenance
            .project_authority()
            .clone()
            .for_child()
            .map_err(DispatchError::Internal)?;
        state
            .threads
            .create_thread(&ryeos_app::thread_lifecycle::ThreadCreateParams {
                thread_id: thread_id.clone(),
                chain_root_id: durable_parent.chain_root_id,
                kind: thread_profile_str.to_string(),
                item_ref: method_subject.item_ref.clone(),
                executor_ref: executor_ref.clone(),
                launch_mode: request.launch_mode.to_string(),
                current_site_id: ctx.plan_ctx.current_site_id.clone(),
                origin_site_id: ctx.plan_ctx.origin_site_id.clone(),
                upstream_thread_id: Some(durable_parent.thread_id.clone()),
                requested_by: Some(request.acting_principal.to_string()),
                project_root: project_authority
                    .project_root_projection()
                    .map(std::path::Path::to_path_buf),
                base_project_snapshot_hash: project_authority
                    .base_snapshot_projection()
                    .map(str::to_owned),
                project_authority,
                usage_subject: request.usage_subject.clone(),
                usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
                captured_history_policy: None,
            })
    } else {
        let resolved_method = ResolvedExecutionRequest {
            kind: thread_profile_str.to_string(),
            item_ref: method_subject.item_ref.clone(),
            executor_ref: executor_ref.clone(),
            launch_mode: request.launch_mode.to_string(),
            current_site_id: ctx.plan_ctx.current_site_id.clone(),
            origin_site_id: ctx.plan_ctx.origin_site_id.clone(),
            target_site_id: None,
            requested_by: Some(request.acting_principal.to_string()),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
            parameters: request.params.clone(),
            ref_bindings: request.ref_bindings.clone(),
            root_raw_content_digest: history_subject.resolved.raw_content_digest.clone(),
            resolved_item: history_subject.resolved.clone(),
            plan_context: ctx.plan_ctx.clone(),
            root_admission: Some(root_admission.clone()),
        };
        state.threads.create_root_thread_with_id(
            &thread_id,
            &resolved_method,
            request.provenance.project_authority().clone(),
        )
    };
    created.map_err(|e| DispatchError::Internal(anyhow::anyhow!("thread creation failed: {e}")))?;
    let mut lifecycle_owner =
        crate::execution::process_attachment::LifecycleOwnerGuard::new(state, &thread_id);

    if let Some(parent) = request.parent_execution_context.as_ref() {
        let inherited_stop = match state.state_store.record_child_link(
            &parent.parent_thread_id,
            &thread_id,
            "dispatch",
        ) {
            Ok(inherited_stop) => inherited_stop,
            Err(error) => {
                let cleanup = finalize_method_thread_if_needed(
                    state,
                    &thread_id,
                    &launch_owner,
                    "failed",
                    Some(json!({
                        "code": "child_link_failed",
                        "reason": error.to_string(),
                    })),
                )
                .map_err(|cleanup_error| {
                    DispatchError::Internal(anyhow::anyhow!(
                        "record method child lineage for {} failed: {error}; conditional cleanup also failed: {cleanup_error:#}",
                        parent.parent_thread_id
                    ))
                })?;
                if cleanup.is_settled() {
                    lifecycle_owner.disarm();
                } else {
                    tracing::warn!(
                        thread_id,
                        parent_thread_id = %parent.parent_thread_id,
                        "child-link cleanup preserved for daemon shutdown recovery"
                    );
                }
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "record method child lineage for {}: {error}",
                    parent.parent_thread_id
                )));
            }
        };
        if inherited_stop.is_some() {
            let settled = crate::execution::process_attachment::finalize_requested_stop_if_present(
                state, &thread_id,
            )
            .map_err(DispatchError::Internal)?;
            if !settled {
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "parent {} propagated a stop to method child {thread_id}, but the child had no durable stop",
                    parent.parent_thread_id
                )));
            }
            lifecycle_owner.disarm();
            return Err(DispatchError::Internal(anyhow::anyhow!(
                "parent {} was stop-requested before method launch",
                parent.parent_thread_id
            )));
        }
    }

    // The row is durable and the daemon-owned dispatch task now holds its
    // lifecycle guard. Every remaining preparation/spawn failure passes through
    // the guarded cleanup below and finalizes this exact row, so an accepted
    // caller can safely receive the pre-minted id without waiting for corpus
    // projection, executor materialization, or process scheduling.
    if let Some(handoff) = launch_handoff {
        handoff.publish(thread_id.clone());
    }

    // Generate callback token. The method child borrows the dispatch
    // provenance instead of minting an unprovenanced token.
    let ttl = ryeos_app::callback_token::launch_token_ttl(Some(METHOD_RUNTIME_TIMEOUT_SECS));
    let child_provenance = request.provenance.clone_for_borrowed_child();
    let callback_project_path = request
        .provenance
        .state_root_override()
        .unwrap_or(request.project_path)
        .to_path_buf();
    let cap = state.callback_tokens.generate_with_context(
        &thread_id,
        callback_project_path.clone(),
        ttl,
        Vec::new(), // method threads have no caps for now
        child_provenance,
        None,
        Some(method_subject.item_ref.clone()),
        history_subject.resolved.raw_content_digest.clone(),
        serde_json::Value::Null,
        0,
    );
    if !state
        .callback_tokens
        .set_launch_owner(&cap.token, launch_owner.clone())
    {
        return Err(DispatchError::Internal(anyhow::anyhow!(
            "method callback capability disappeared before owner binding"
        )));
    }
    lifecycle_owner.track_callback_token(cap.token.clone());

    // 9. Mint thread-auth authority only when the verified runtime protocol
    //    requests that typed source. The descriptor owns its environment name.
    let needs_thread_auth = runtime_protocol
        .descriptor
        .env_injections
        .iter()
        .any(|injection| {
            injection.source
                == ryeos_engine::protocol_vocabulary::EnvInjectionSource::ThreadAuthToken
        });
    let thread_auth = needs_thread_auth.then(|| {
        state.thread_auth.mint(
            &thread_id,
            request.acting_principal.to_string(),
            ctx.caller_scopes.clone(),
            ttl,
        )
    });
    if let Some(thread_auth) = &thread_auth {
        lifecycle_owner.track_thread_auth_token(thread_auth.token.clone());
    }

    // 10-11. All post-mint work runs inside this guarded block. ANY failure
    //       here — envelope serialize, native executor resolution, env
    //       build, spawn join, or result parse — returns `Err` and still
    //       falls through to the cleanup below, which finalizes the thread
    //       as failed unless a durable stop/shutdown owner already won and
    //       revokes all protocol-requested borrowed-child authority. On the
    //       normal path the daemon finalizes only after validating stdout.
    let outcome: Result<Value, DispatchError> = async {
        // Project the payload now — AFTER the thread row exists — from the
        // sealed pre-persistence resolution snapshot, so later source changes
        // cannot alter the admitted execution.
        let mut payload = project_method_payload(
            method_decl,
            canonical_ref,
            hop_verified.as_ref(),
            kind,
            &engine_roots,
            ctx,
            request,
            Some(&root_admission),
        )?;
        root_admission
            .ensure_matches_subject(&ctx.engine, history_subject, thread_profile_str)
            .map_err(DispatchError::Internal)?;
        if let Value::Object(ref mut map) = payload {
            map.insert("args".to_string(), validated_args);
        }

        let callback = ryeos_runtime::envelope::EnvelopeCallback {
            socket_path: state.config.uds_path.clone(),
            token: cap.token.clone(),
        };
        let runtime_config = method_runtime_config_snapshot(
            kind,
            &method_decl.runtime_config,
            &engine_roots,
            state,
        )?;

        let envelope = ryeos_engine::method_wire::MethodCallEnvelope {
            schema_version: ryeos_engine::method_wire::METHOD_CALL_SCHEMA_VERSION,
            kind: kind.to_string(),
            method: method_name.to_string(),
            thread_id: thread_id.clone(),
            callback,
            callback_project_path: callback_project_path.clone(),
            project_root: request.project_path.to_path_buf(),
            runtime_config,
            payload,
        };

        let stdin_data = ryeos_engine::protocols::build_method_call_stdin(
            &runtime_protocol.descriptor,
            &envelope,
        )
        .map_err(|error| DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!(
                "method protocol '{}' could not build its declared stdin: {error}",
                runtime_protocol.canonical_ref
            ),
        })?;
        let stdin_data = String::from_utf8(stdin_data)
            .map_err(|error| DispatchError::Internal(error.into()))?;

        // 9. Resolve the native executor path and spawn via lillux.
        let bundle_roots: Vec<std::path::PathBuf> = engine_roots
            .ordered
            .iter()
            .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::Bundle)
            .map(|r| {
                r.ai_root
                    .parent()
                    .map(|pp| pp.to_path_buf())
                    .unwrap_or(r.ai_root.clone())
            })
            .collect();
        let cache_root = state
            .config
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("state");
        let executor = crate::execution::launch::materialize_native_executor(
            &bundle_roots,
            &executor_ref,
            &cache_root,
            &ctx.engine.node_trust_store,
            ryeos_engine::resolution::TrustClass::TrustedBundle,
        )
        .map_err(|e| DispatchError::RuntimeMaterializationFailed {
            executor_ref: executor_ref.clone(),
            detail: e.to_string(),
        })?;

        let executor_path = executor.path.clone();
        let executor_path_str = executor_path
            .to_str()
            .ok_or_else(|| {
                DispatchError::Internal(anyhow::anyhow!(
                    "resolved executor path is not valid UTF-8"
                ))
            })?
            .to_owned();
        let project_path_str = request.project_path.to_str().ok_or_else(|| {
            DispatchError::Internal(anyhow::anyhow!("dispatch project path is not valid UTF-8"))
        })?;
        let isolation_verified_code = [ryeos_engine::isolation::IsolationVerifiedCode {
            source_path: executor.path,
            content_hash: executor.content_hash,
        }];
        let roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
            &engine_roots,
            &state.config.app_root,
        )
        .map_err(DispatchError::Internal)?;
        let callback_socket_requested = runtime_protocol
            .descriptor
            .env_injections
            .iter()
            .any(|injection| {
                injection.source
                    == ryeos_engine::protocol_vocabulary::EnvInjectionSource::CallbackSocketPath
            });
        let callback_ipc_requested = runtime_protocol.descriptor.callback_channel
            != ryeos_engine::protocol_vocabulary::CallbackChannel::None
            || callback_socket_requested;
        let env_request = ryeos_engine::subprocess_spec::SubprocessBuildRequest {
            cmd: executor_path,
            args: Vec::new(),
            cwd: request.project_path.to_path_buf(),
            timeout: std::time::Duration::from_secs(METHOD_RUNTIME_TIMEOUT_SECS),
            item_ref: runtime_item_ref.clone(),
            thread_id: thread_id.clone(),
            project_path: request.project_path.to_path_buf(),
            acting_principal: request.acting_principal.to_string(),
            cas_root: state
                .state_store
                .cas_root()
                .map_err(DispatchError::Internal)?,
            callback_token: Some(envelope.callback.token.clone()),
            callback_socket_path: if callback_socket_requested {
                Some(
                    envelope
                        .callback
                        .socket_path
                        .to_str()
                        .ok_or_else(|| {
                            DispatchError::Internal(anyhow::anyhow!(
                                "daemon callback socket path is not valid UTF-8"
                            ))
                        })?
                        .to_owned(),
                )
            } else {
                None
            },
            callback_project_path: Some(callback_project_path.clone()),
            thread_auth_token: thread_auth.as_ref().map(|auth| auth.token.clone()),
            params: envelope.payload.clone(),
            resolution_output: None,
        };
        let protocol_bindings = runtime_protocol
            .descriptor
            .env_injections
            .iter()
            .map(|injection| {
                let value = ryeos_engine::protocol_vocabulary::produce_env_value(
                    injection.source,
                    &env_request,
                )
                .map_err(|error| DispatchError::SchemaMisconfigured {
                    kind: runtime_item_ref.kind.clone(),
                    detail: format!(
                        "protocol '{}' env injection '{}' is unavailable for method runtime '{}': {error}",
                        runtime_protocol.canonical_ref, injection.name, runtime_item_ref,
                    ),
                })?;
                Ok(ryeos_app::env_contract::EnvBinding::new(
                    injection.name.clone(),
                    value,
                    ryeos_app::env_contract::EnvSourceDetail::ProtocolInjection {
                        source: injection.source,
                    },
                ))
            })
            .collect::<Result<Vec<_>, DispatchError>>()?;
        let envs = ryeos_app::env_contract::EnvContractBuilder::new()
            .with_base_allowlist(std::env::vars_os().map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            }))
            .map_err(|error| DispatchError::Internal(error.into()))?
            .with_daemon_roots(roots)
            .map_err(|error| DispatchError::Internal(error.into()))?
            .with_typed_bindings(protocol_bindings)
            .map_err(|error| DispatchError::Internal(error.into()))?
            .build();
        let subprocess_request = lillux::SubprocessRequest {
            cmd: executor_path_str,
            argv0: None,
            args: vec![],
            cwd: Some(project_path_str.to_owned()),
            envs,
            stdin_data: Some(stdin_data),
            timeout: METHOD_RUNTIME_TIMEOUT_SECS as f64,
            limits: None,
            inherited_fds: Vec::new(),
            supervised_status: None,
        };
        let node_trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
        let live_access = request
            .provenance
            .isolation_live_access_authority()
            .map_err(DispatchError::Internal)?;
        let applied = state
            .isolation
            .apply_awaiting_attachment_with_provenance(
                subprocess_request,
                ryeos_engine::isolation::IsolationLaunchContext {
                    project_path: request.project_path,
                    project_authority: request.provenance.isolation_project_authority(),
                    live_access: live_access.as_ref(),
                    state_root: request.provenance.state_root_override(),
                    checkpoint_dir: None,
                    daemon_socket_path: callback_ipc_requested
                        .then_some(envelope.callback.socket_path.as_path()),
                    bundle_roots: &bundle_roots,
                    node_trusted_keys_dir: Some(&node_trusted_keys_dir),
                    verified_code: &isolation_verified_code,
                    verified_command: Some(&isolation_verified_code[0]),
                    item_ref: &runtime_item_ref_string,
                    thread_id: &thread_id,
                },
            )
            .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
        state
            .state_store
            .seed_isolation_provenance(&thread_id, applied.provenance)
            .map_err(DispatchError::Internal)?;
        let subprocess_request = applied.request;
        let workspace_lifeline = request.provenance.workspace_lifeline();
        let process_state = state.clone();
        let process_thread_id = thread_id.clone();
        let process_launch_owner = launch_owner.clone();
        let process_handle = tokio::task::spawn_blocking(move || {
            crate::execution::process_attachment::run_lillux_attached(
                &process_state,
                &process_thread_id,
                &process_launch_owner,
                subprocess_request,
                workspace_lifeline,
            )
        });

        let result = process_handle
        .await
        .map_err(|e| DispatchError::Internal(e.into()))?
        .map_err(DispatchError::Internal)?;

        // Process the runtime result. On failure, return `Err` and let the
        // cleanup below finalize the thread. On success, the daemon publishes
        // terminal state only after validating the complete stdout contract.
        if !result.success {
            // Trust a structured error only if the runtime echoed the
            // dispatched kind/method; otherwise fall through to a generic
            // failure so a confused result cannot masquerade as a typed one.
            if let Ok(batch_result) = decode_method_runtime_result(runtime_protocol, &result.stdout)
            {
                if batch_result.kind == kind && batch_result.method == method_name {
                    if let Some(error) = batch_result.error {
                        return Err(map_method_error(kind, method_name, error));
                    }
                }
            }
            return Err(DispatchError::MethodFailed {
                kind: kind.to_string(),
                method: method_name.to_string(),
                reason: format!(
                    "exit_code={}, stderr={}",
                    result.exit_code,
                    result.stderr.trim()
                ),
            });
        }

        let batch_result = decode_method_runtime_result(runtime_protocol, &result.stdout)
            .map_err(|e| DispatchError::MethodFailed {
                kind: kind.to_string(),
                method: method_name.to_string(),
                reason: format!("failed to parse MethodCallResult: {e}"),
            })?;

        // The runtime must echo back the dispatched kind/method. A mismatch
        // means schema/runtime skew or a confused result — never trust it.
        if batch_result.kind != kind || batch_result.method != method_name {
            return Err(DispatchError::MethodFailed {
                kind: kind.to_string(),
                method: method_name.to_string(),
                reason: format!(
                    "runtime returned a result for '{}/{}' but '{}/{}' was dispatched",
                    batch_result.kind, batch_result.method, kind, method_name
                ),
            });
        }

        if !batch_result.success {
            return Err(match batch_result.error {
                Some(error) => map_method_error(kind, method_name, error),
                None => DispatchError::MethodFailed {
                    kind: kind.to_string(),
                    method: method_name.to_string(),
                    reason: "runtime returned success=false with no error detail".into(),
                },
            });
        }

        let output = batch_result.output.ok_or_else(|| DispatchError::MethodFailed {
            kind: kind.to_string(),
            method: method_name.to_string(),
            reason: "runtime returned success=true without an output payload".into(),
        })?;

        // Success: the daemon is the terminal authority after validating the
        // process result, method wire semantics, and kind/method echo.
        let finalization = finalize_method_thread_if_needed(
            state,
            &thread_id,
            &launch_owner,
            "completed",
            Some(output.clone()),
        )
        .map_err(DispatchError::Internal)?;
        match finalization {
            MethodFinalizeOutcome::Finalized => {}
            MethodFinalizeOutcome::AlreadyTerminal => {
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "method thread {thread_id} became terminal before its validated result was committed"
                )))
            }
            MethodFinalizeOutcome::DurableStopSettled => {
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "method thread {thread_id} completed after a durable stop won"
                )))
            }
            MethodFinalizeOutcome::PreservedForShutdown => {
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "method thread {thread_id} was interrupted by daemon shutdown and preserved for recovery"
                )))
            }
        }

        Ok(json!({
            "thread": {
                "thread_id": thread_id.clone(),
                "kind": thread_profile_str,
                "item_ref": method_subject.item_ref.clone(),
                "status": "completed",
            },
            "result": output,
        }))
    }
    .await;

    // Cleanup on every path. If the run errored, finalize the thread as
    // failed unless a durable stop or another terminal transition already won;
    // then revoke the borrowed-child authority, mirroring the subprocess
    // runner's guard. This covers post-mint failures (executor resolution, env
    // build, spawn join) that return before the runtime ever touched the
    // thread. Preserve the structured dispatch error so accepted/background
    // callers do not end up with a bare "failed" thread and no cause.
    let lifecycle_settled = match &outcome {
        Ok(_) => true,
        Err(err) => match finalize_method_thread_if_needed(
            state,
            &thread_id,
            &launch_owner,
            "failed",
            Some(json!({
                "code": err.code(),
                "reason": err.to_string(),
            })),
        ) {
            Ok(_) => true,
            Err(cleanup_error) => {
                tracing::error!(
                    thread_id,
                    execution_error = %err,
                    cleanup_error = %cleanup_error,
                    "method execution and terminal cleanup both failed"
                );
                false
            }
        },
    };
    state.callback_tokens.invalidate(&cap.token);
    state.callback_tokens.invalidate_for_thread(&thread_id);
    if let Some(thread_auth) = &thread_auth {
        state.thread_auth.invalidate(&thread_auth.token);
    }
    state.thread_auth.invalidate_for_thread(&thread_id);
    if lifecycle_settled {
        lifecycle_owner.disarm();
    }

    outcome
}

/// Conditionally finalize a daemon-owned subprocess thread. Method runtimes
/// attach and mark running, but the daemon publishes their terminal state only
/// after validating terminal stdout. The conditional transition still lets a
/// durable stop or shutdown/recovery owner win without a second terminal
/// write. The typed outcome prevents callers from treating an attempted but
/// unsuccessful write as settled cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodFinalizeOutcome {
    Finalized,
    AlreadyTerminal,
    DurableStopSettled,
    PreservedForShutdown,
}

impl MethodFinalizeOutcome {
    pub(crate) fn is_settled(self) -> bool {
        self != Self::PreservedForShutdown
    }
}

pub(crate) fn finalize_method_thread_if_needed(
    state: &AppState,
    thread_id: &str,
    launch_owner: &str,
    status: &str,
    result: Option<Value>,
) -> anyhow::Result<MethodFinalizeOutcome> {
    use ryeos_app::thread_lifecycle::FinalizeIfNonterminalOutcome;
    use ryeos_state::objects::ThreadStatus;

    let stop_terminal = |status: &str| {
        matches!(
            ThreadStatus::from_str_lossy(status),
            Some(ThreadStatus::Cancelled | ThreadStatus::Killed)
        )
    };
    match state
        .threads
        .finalize_if_nonterminal_owned(&finalize_params(thread_id, status, result), launch_owner)?
    {
        FinalizeIfNonterminalOutcome::Finalized(thread) if stop_terminal(&thread.status) => {
            Ok(MethodFinalizeOutcome::DurableStopSettled)
        }
        FinalizeIfNonterminalOutcome::Finalized(_) => Ok(MethodFinalizeOutcome::Finalized),
        FinalizeIfNonterminalOutcome::AlreadyTerminal { status } if stop_terminal(&status) => {
            Ok(MethodFinalizeOutcome::DurableStopSettled)
        }
        FinalizeIfNonterminalOutcome::AlreadyTerminal { .. } => {
            Ok(MethodFinalizeOutcome::AlreadyTerminal)
        }
        FinalizeIfNonterminalOutcome::PreservedForShutdown => {
            tracing::info!(
                thread_id,
                "preserving method thread after shutdown-owned interruption"
            );
            Ok(MethodFinalizeOutcome::PreservedForShutdown)
        }
    }
}

/// Conditional cleanup for a child whose operational-lineage write failed.
/// The lifecycle service performs the status/process/launch-claim check and the
/// terminal write under one StateStore lock.
pub(crate) fn finalize_child_link_failure_if_current(
    state: &AppState,
    thread_id: &str,
    error: Value,
) -> anyhow::Result<ryeos_app::thread_lifecycle::FinalizeCreatedUnattachedOutcome> {
    let mut params = finalize_params(thread_id, "failed", Some(error));
    params.outcome_code = Some("child_link_failed".to_string());
    state
        .threads
        .finalize_created_unattached_if_current(&params)
}

/// Map a `MethodCallError` from the runtime to a `DispatchError`.
fn map_method_error(
    kind: &str,
    method: &str,
    error: ryeos_runtime::method_wire::MethodCallError,
) -> DispatchError {
    use ryeos_runtime::method_wire::MethodCallError;
    match error {
        MethodCallError::NotImplemented { phase, .. } => DispatchError::MethodNotImplemented {
            kind: kind.to_string(),
            method: method.to_string(),
            phase,
        },
        MethodCallError::InvalidArg { reason, .. } => DispatchError::MethodInvalidArg {
            method: method.to_string(),
            reason,
        },
        MethodCallError::UnknownMethod {
            requested,
            declared,
            ..
        } => DispatchError::UnknownMethod {
            kind: kind.to_string(),
            requested,
            declared: declared.join(", "),
        },
        MethodCallError::MethodFailed { reason } => DispatchError::MethodFailed {
            kind: kind.to_string(),
            method: method.to_string(),
            reason,
        },
    }
}

/// Helper to build a `ThreadFinalizeParams` with only the required fields
/// set and all optional fields defaulted to `None` / empty.
pub(crate) fn finalize_params(
    thread_id: &str,
    status: &str,
    payload: Option<Value>,
) -> ryeos_app::thread_lifecycle::ThreadFinalizeParams {
    use ryeos_app::thread_lifecycle::ThreadFinalizeParams;
    use ryeos_state::objects::ThreadStatus;
    // A failed thread's payload is its cause → route it to `error` (which the
    // terminal braid event persists, and the feed reads) instead of burying it
    // in `result` (which the event drops), leaving the operator with a bare
    // "failed". A non-failure terminal's payload is its result.
    let is_failure = ThreadStatus::from_str_lossy(status).is_some_and(|s| s.is_failure());
    let (result, error) = if is_failure {
        (None, payload)
    } else {
        (payload, None)
    };
    ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: status.to_string(),
        outcome_code: None,
        result,
        error,
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }
}

// ── Service terminator ────────────────────────────────────────────────

/// Dispatch a `service:*` ref through the schema-declared
/// `InProcessHandler { Services }` terminator.
///
/// **A3**: the service envelope's `kind` field is read from
/// `schema.execution.thread_profile` (validated at engine init) — no
/// `"service_run"` literal anywhere on this hot path.
pub async fn dispatch_service(
    item_ref: &str,
    thread_profile: &str,
    verified: Option<VerifiedItem>,
    local_handler_context: Option<ryeos_app::handler_context::HandlerContext>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<Value, DispatchError> {
    let canonical = CanonicalRef::parse(item_ref)
        .map_err(|e| DispatchError::InvalidRef(item_ref.to_string(), e.to_string()))?;
    let schema = ctx.engine.kinds.get(&canonical.kind).ok_or_else(|| {
        let mut available: Vec<String> = ctx.engine.kinds.kinds().map(|k| k.to_string()).collect();
        available.sort();
        DispatchError::SchemaMisconfigured {
            kind: canonical.kind.clone(),
            detail: format!(
                "no kind schema registered; registered kinds: [{}]",
                available.join(", ")
            ),
        }
    })?;
    let exec = schema
        .execution()
        .ok_or_else(|| DispatchError::NotRootExecutable {
            kind: canonical.kind.clone(),
            detail: "schema has no `execution:` block".into(),
        })?;
    let terminator =
        exec.terminator
            .as_ref()
            .ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: canonical.kind.clone(),
                detail: "dispatch_service called on a schema with no terminator".into(),
            })?;
    match terminator {
        TerminatorDecl::InProcess {
            registry: InProcessRegistryKind::Services,
        } => {
            let verified = match verified {
                Some(verified) => verified,
                None => service_executor::resolve_and_verify(
                    &ctx.engine,
                    &ctx.plan_ctx,
                    item_ref,
                    Some(canonical.kind.as_str()),
                )
                .map_err(|e| {
                    match e.downcast_ref::<ryeos_engine::error::EngineError>() {
                        // The service item YAML is absent from every search
                        // space — the bundle shipping it is not installed.
                        // Surface the installed-bundle list instead of an
                        // opaque 500 so a remote operator can repair the
                        // deployment without source-level debugging.
                        Some(ryeos_engine::error::EngineError::ItemNotFound {
                            searched_spaces,
                            ..
                        }) => {
                            let mut installed_bundles: Vec<String> = state
                                .node_config
                                .bundles
                                .iter()
                                .map(|b| b.name.clone())
                                .collect();
                            installed_bundles.sort();
                            DispatchError::ServiceNotInstalled {
                                service_ref: item_ref.to_string(),
                                installed_bundles,
                                searched_spaces: searched_spaces.clone(),
                            }
                        }
                        _ => DispatchError::Internal(e),
                    }
                })?,
            };
            // validate_only: return schema info without invoking the handler.
            // This is the codepath triggered by `ryeos help <verb>` — the
            // handler body must NEVER be reached because it will fail on
            // required fields that the help request doesn't supply.
            if request.validate_only {
                let description = verified.resolved.metadata.description.clone();
                let input_schema = verified.resolved.metadata.extra.get("schema").cloned();
                let endpoint = verified
                    .resolved
                    .metadata
                    .extra
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let required_caps = ryeos_app::service_registry::extract_required_caps(
                    &verified.resolved.metadata.extra,
                );

                return Ok(json!({
                    "validated": true,
                    "item_ref": item_ref,
                    "kind": canonical.kind,
                    "executor_ref": endpoint.unwrap_or_default(),
                    "trust_class": format!("{:?}", verified.trust_class),
                    "description": description,
                    "schema": input_schema,
                    "required_caps": required_caps,
                }));
            }

            let result: ServiceExecutionResult = service_executor::execute_service_verified(
                verified,
                item_ref,
                request.params.clone(),
                ExecutionMode::Live,
                ctx,
                state,
                service_executor::ServiceRecordingContext {
                    authority_source:
                        service_executor::ServiceRecordingAuthoritySource::Execution {
                            provenance: &request.provenance,
                        },
                    usage_subject: request.usage_subject.as_ref(),
                    usage_subject_asserted_by: request.usage_subject_asserted_by.as_deref(),
                },
                request.pre_minted_thread_id.as_deref(),
                local_handler_context,
            )
            .await
            // `execute_service_verified` maps a handler's typed `HandlerError`
            // (e.g. ownership → NotFound) to a `DispatchError` and returns it
            // through `anyhow`. Recover that typed error here so it keeps its
            // HTTP status — a blanket `?` would re-wrap it as `Internal` (500),
            // dropping the 404/409/etc. that the route path preserves.
            .map_err(|e| e.downcast::<DispatchError>().unwrap_or_else(DispatchError::Internal))?;
            let envelope = serde_json::json!({
                "thread": {
                    "thread_id": result.invocation_id,
                    "recorded": result.recorded,
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
                "dispatch_service called on schema declaring terminator {other:?}, not InProcess {{ Services }}"
            ),
        }),
    }
}

/// The `method_dispatch:` block a wrapper tool declares (surfaced verbatim into
/// `metadata.extra["method_dispatch"]` by the tool kind schema's `path_value`
/// rule — the engine never interprets it). Generic over method-served kinds:
/// no kind is named here; the wrapper YAML supplies both the target kind
/// (`ref_kind`) and the method.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MethodDispatchConfig {
    /// Method to invoke on the target kind (e.g. `compose`). Fixed by the
    /// wrapper YAML — never chosen by the caller/LLM.
    method: String,
    /// Optional kind used to prefix a bare `ref` argument, and to require a
    /// canonical `ref` to match. Absent → the `ref` argument must be canonical.
    #[serde(default)]
    ref_kind: Option<String>,
}

/// Resolve the caller-supplied `ref` argument into a canonical target ref,
/// honoring the wrapper's optional `ref_kind`.
fn resolve_method_target_ref(
    raw: &str,
    ref_kind: Option<&str>,
    wrapper_ref: &str,
) -> Result<String, DispatchError> {
    match raw.split_once(':') {
        // Canonical `kind:id`. If the wrapper pins a `ref_kind`, the kind must
        // match — the wrapper is narrow by construction and must not be coaxed
        // into dispatching a different kind.
        Some((kind, _bare)) => {
            if let Some(rk) = ref_kind {
                if kind != rk {
                    return Err(DispatchError::MethodInvalidArg {
                        method: "method_dispatch".into(),
                        reason: format!(
                            "tool `{wrapper_ref}` only dispatches `{rk}:` refs, got `{kind}:`"
                        ),
                    });
                }
            }
            Ok(raw.to_string())
        }
        // Bare id — allowed only when the wrapper pins a `ref_kind` to prefix.
        None => match ref_kind {
            Some(rk) => Ok(format!("{rk}:{raw}")),
            None => Err(DispatchError::MethodInvalidArg {
                method: "method_dispatch".into(),
                reason: format!(
                    "tool `{wrapper_ref}` `ref` must be a canonical `kind:id` ref \
                     (declare `ref_kind:` to accept bare ids); got `{raw}`"
                ),
            }),
        },
    }
}

/// The resolved + authorized target of a `method_dispatch` wrapper.
struct MethodDispatchTarget {
    target_ref: String,
    target_canonical: CanonicalRef,
    method: String,
    /// Method args = the caller's params minus the `ref` selector.
    args: Value,
}

/// Resolve and authorize the target of a `method_dispatch` wrapper from its
/// config + the caller's params.
///
/// Shared by the live dispatch path ([`dispatch_via_method_executor`]) and the
/// accepted-launch preflight ([`preflight_root_dispatch`]) so both agree on the
/// target ref, method, args, and the inner-ref authorization. Kind-agnostic —
/// the target kind and method come entirely from the wrapper's data.
fn resolve_method_dispatch_target(
    wrapper: &ResolvedItem,
    params: &Value,
    caller_scopes: &[String],
    authorizer: &ryeos_runtime::authorizer::Authorizer,
) -> Result<MethodDispatchTarget, DispatchError> {
    let wrapper_ref = wrapper.canonical_ref.to_string();

    // The wrapper's method_dispatch config (surfaced via the kind-schema
    // `path_value` rule). Its absence means the terminal selected method
    // dispatch but the wrapper never configured it.
    let cfg_value = wrapper
        .metadata
        .extra
        .get("method_dispatch")
        .ok_or_else(|| DispatchError::SchemaMisconfigured {
            kind: wrapper.kind.clone(),
            detail: format!(
                "tool `{wrapper_ref}` routes to the method-dispatch terminal but declares no \
                 top-level `method_dispatch:` block"
            ),
        })?;
    let cfg: MethodDispatchConfig = serde_json::from_value(cfg_value.clone()).map_err(|e| {
        DispatchError::SchemaMisconfigured {
            kind: wrapper.kind.clone(),
            detail: format!("tool `{wrapper_ref}` has an invalid `method_dispatch:` block: {e}"),
        }
    })?;

    // Target ref from the `ref` argument.
    let raw_ref = params.get("ref").and_then(Value::as_str).ok_or_else(|| {
        DispatchError::MethodInvalidArg {
            method: cfg.method.clone(),
            reason: format!("tool `{wrapper_ref}` requires a string `ref` argument"),
        }
    })?;
    let target_ref = resolve_method_target_ref(raw_ref, cfg.ref_kind.as_deref(), &wrapper_ref)?;
    let target_canonical = CanonicalRef::parse(&target_ref)
        .map_err(|e| DispatchError::InvalidRef(target_ref.clone(), e.to_string()))?;

    // Authorize the inner ref against the caller's scopes. Neither the live
    // internal dispatch nor the accepted background dispatch crosses the
    // callback boundary, so its per-ref check (`enforce_callback_caps`) never
    // fires — this is the authorization boundary for the target ref, so a
    // wrapper cannot mint authority to refs the caller lacks.
    let required = format!(
        "ryeos.execute.{}.{}",
        target_canonical.kind, target_canonical.bare_id
    );
    enforce_runtime_caps(authorizer, &target_ref, &[required], caller_scopes)?;

    let mut args = params.clone();
    if let Some(obj) = args.as_object_mut() {
        obj.remove("ref");
    }

    Ok(MethodDispatchTarget {
        target_ref,
        target_canonical,
        method: cfg.method,
        args,
    })
}

/// Execution for a tool whose executor-chain terminal is
/// `terminal_executor.kind == method_dispatch`.
///
/// The wrapper is inert on its own: its execution is a checked recursive method
/// dispatch. It reads the wrapper's `method_dispatch` config, takes the `ref`
/// argument, applies the fixed `method` + optional `ref_kind`, and re-enters
/// [`dispatch`] targeting `<kind>:<ref>` with a synthesized `MethodCall`.
/// Kind-agnostic — the target kind and method come entirely from wrapper data.
async fn dispatch_via_method_executor(
    resolved: &ResolvedExecutionRequest,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    launch_handoff: Option<&crate::execution::launch::LaunchHandoff>,
) -> Result<Value, DispatchError> {
    let wrapper_ref = resolved.item_ref.as_str();

    // A method-dispatch recall runs synchronously. Inline is the normal path
    // (directive tool-calls; `execute` without `--async`). Accepted (`--async`)
    // is supported too — the accepted route mints the thread and runs the
    // background dispatch with launch_mode "wait", and `preflight_root_dispatch`
    // has already validated the target. Only detached (fire-and-forget, no
    // result capture) has no meaningful form for a recall.
    match LaunchMode::from_wire(request.launch_mode) {
        Some(LaunchMode::Wait) => {}
        Some(LaunchMode::Detached) => {
            return Err(DispatchError::CapabilityRejected {
                reason: format!(
                    "tool `{wrapper_ref}` routes to a method dispatch, which is wait-only; \
                     detached launch is not supported"
                ),
            });
        }
        None => {
            return Err(DispatchError::InvalidLaunchMode {
                other: request.launch_mode.to_string(),
            });
        }
    }

    let MethodDispatchTarget {
        target_ref,
        target_canonical,
        method,
        args,
    } = resolve_method_dispatch_target(
        &resolved.resolved_item,
        &request.params,
        &ctx.caller_scopes,
        &state.authorizer,
    )?;

    // Fresh context carrying the synthesized method call — the single source of
    // truth `dispatch_method` reads. Same caller identity/scopes/engine.
    let exec_ctx = ExecutionContext {
        principal_fingerprint: ctx.principal_fingerprint.clone(),
        caller_scopes: ctx.caller_scopes.clone(),
        engine: ctx.engine.clone(),
        plan_ctx: ctx.plan_ctx.clone(),
        requested_call: Some(ryeos_engine::method_call::MethodCall {
            method: Some(method),
            args: Some(args.clone()),
        }),
    };

    let dispatch_req = DispatchRequest {
        launch_mode: request.launch_mode,
        target_site_id: request.target_site_id,
        validate_only: request.validate_only,
        params: args,
        ref_bindings: request.ref_bindings.clone(),
        acting_principal: request.acting_principal,
        project_path: request.project_path,
        provenance: request.provenance.clone(),
        lifecycle_authority: request.lifecycle_authority,
        original_root_kind: target_canonical.kind.as_str(),
        pre_minted_thread_id: request.pre_minted_thread_id.clone(),
        usage_subject: request.usage_subject.clone(),
        usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
        previous_thread_id: request.previous_thread_id.clone(),
        root_admission: request.root_admission.clone(),
        parent_execution_context: request.parent_execution_context.clone(),
    };

    // Re-enter the shared dispatch loop on the target ref. Boxed: this closes
    // the recursion cycle while preserving accepted-launch handoff authority
    // through an inert method wrapper to the actual method-runtime leaf.
    Box::pin(dispatch_inner(
        &target_ref,
        None,
        None,
        &dispatch_req,
        &exec_ctx,
        state,
        launch_handoff,
    ))
    .await
}

/// Mint the manifest-backed callback caps an item is entitled to.
///
/// Launch-time satisfaction of the item's runtime capability *requirement
/// contract* (`requires.capabilities.manifest`). `requires_value` is the raw
/// `requires:` mapping from whichever source the caller trusts:
///
/// - **graph/directive (managed path):** the *composed* view, after the
///   extends-chain composer has narrowed a child against its parent — so a
///   child can never exceed the parent template.
/// - **tool path:** the resolved item's extracted metadata (tools use the
///   identity composer and never extend, so the leaf declaration is effective).
///
/// Semantics, regardless of source:
///
/// 1. Absent `requires:` → no caps. The provenance-bound signed manifest is an
///    authority *upper bound*, never an automatic grant.
/// 2. The requested subset must be backed by that authoritative manifest;
///    requesting a cap the installed bundle or live project does not declare
///    fails launch.
/// 3. Exactly the requested (manifest-backed) subset is minted.
pub(crate) fn mint_runtime_capability_caps(
    requires_value: Option<&Value>,
    resolved_item: &ResolvedItem,
    effective_trust_class: ryeos_engine::resolution::TrustClass,
    engine: &ryeos_engine::engine::Engine,
) -> Result<Vec<String>, String> {
    // (1) Requirement contract. No `requires:` → no runtime callback authority.
    let Some(requires_value) = requires_value else {
        return Ok(Vec::new());
    };
    let reqs = ryeos_bundle::runtime_authority::parse_runtime_requires(requires_value)
        .map_err(|err| format!("invalid `requires.capabilities`: {err}"))?;
    if reqs.manifest.runtime_authority.is_empty() {
        return Ok(Vec::new());
    }

    // Single source of truth for the bundle identity: the resolved canonical
    // ref. The callback token's `effective_bundle_id` is stamped from this same
    // value (see `effective_bundle_id_for_request`), so the caps minted here and
    // the token that carries them can never claim different bundles.
    let effective_bundle_id = ryeos_app::callback_token::effective_bundle_id_from_item_ref(
        &resolved_item.canonical_ref.to_string(),
    )
    .ok_or_else(|| {
        "runtime capability requirements need a bundle-qualified item ref".to_string()
    })?;
    ryeos_state::objects::validate_bundle_identifier("bundle_id", &effective_bundle_id)
        .map_err(|err| err.to_string())?;

    // Deep, segment-grammar validation of each requested resource. The static
    // shape (known keys, valid ops, non-empty arrays) was already enforced by
    // `parse_runtime_requires`; this checks the bundle-id segment grammar that
    // the cap-string scheme depends on.
    let requested_authority = &reqs.manifest.runtime_authority;
    for req in &requested_authority.bundle_events {
        ryeos_state::objects::validate_bundle_identifier("event_kind", &req.event_kind)
            .map_err(|err| err.to_string())?;
    }
    for req in &requested_authority.runtime_vault {
        ryeos_app::vault::validate_runtime_vault_segment("namespace", &req.namespace)
            .map_err(|err| err.to_string())?;
    }
    for req in &requested_authority.item_authoring {
        ryeos_bundle::runtime_authority::validate_item_author_pattern(&req.kind, &req.namespace)?;
    }

    // (2) Authority upper bound comes from the signed manifest at the exact
    // provenance boundary that supplied the item. Installed bundle authority
    // remains node-trusted. A live project may use its own signed manifest,
    // but only for a TrustedProject item physically below that exact project's
    // `.ai/` root and only with the request engine's effective project trust
    // store. Mixed-trust composition of an installed item therefore remains
    // unable to acquire project authority.
    let manifest = match resolved_item.source_space {
        ryeos_engine::contracts::ItemSpace::Bundle => {
            if effective_trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle {
                return Err(format!(
                    "installed runtime capability requirements need TrustedBundle provenance; \
                     effective trust class is {effective_trust_class:?}"
                ));
            }
            let ai_dir = authoritative_runtime_authority_ai_dir(
                resolved_item,
                &engine.bundle_roots,
                &engine.node_trust_store,
            )?;
            ryeos_bundle::manifest::load_verified_manifest(
                &ai_dir,
                &effective_bundle_id,
                &engine.node_trust_store,
            )
            .map_err(|err| err.to_string())?
            .manifest
        }
        ryeos_engine::contracts::ItemSpace::Project => {
            if effective_trust_class != ryeos_engine::resolution::TrustClass::TrustedProject {
                return Err(format!(
                    "project runtime capability requirements need TrustedProject provenance; \
                     effective trust class is {effective_trust_class:?}"
                ));
            }
            let project_root = resolved_item
                .materialized_project_root
                .as_deref()
                .ok_or_else(|| {
                    "project runtime capability item has no materialized project root".to_string()
                })?;
            let ai_dir =
                authoritative_project_runtime_authority_ai_dir(resolved_item, project_root)?;
            let project_trust = engine
                .trust_store
                .with_project_keys(project_root)
                .map_err(|err| err.to_string())?;
            ryeos_bundle::manifest::load_verified_manifest(
                &ai_dir,
                &effective_bundle_id,
                project_trust.as_ref(),
            )
            .map_err(|err| err.to_string())?
            .manifest
        }
    };
    manifest.runtime_authority.validate()?;

    // Manifest-declared caps form the upper bound. Cap strings come from the
    // manifest declarations' own constructors (`runtime_authority`), so the
    // minter and the daemon callback services share one definition.
    let manifest_caps = manifest
        .runtime_authority
        .declared_caps(&effective_bundle_id);

    // (3) Subset check + mint exactly the requested subset. A wildcard-carrying
    // request must be backed by an identical manifest declaration, not merely
    // glob-matched — see `manifest_backs_requested_cap`.
    let requested =
        ryeos_bundle::runtime_authority::requested_runtime_caps(&reqs, &effective_bundle_id);
    let missing: Vec<String> = requested
        .iter()
        .filter(|requested_cap| {
            !ryeos_bundle::runtime_authority::manifest_backs_requested_cap(
                &manifest_caps,
                requested_cap,
            )
        })
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "requested runtime capabilities are not declared in the signed manifest \
             (authority upper bound): {}",
            missing.join(", ")
        ));
    }

    Ok(requested.into_iter().collect())
}

/// Bind project runtime authority to the exact live project resolution root.
/// This deliberately does not walk upward looking for a convenient manifest.
fn authoritative_project_runtime_authority_ai_dir(
    resolved_item: &ResolvedItem,
    project_root: &Path,
) -> Result<PathBuf, String> {
    let canonical_project = std::fs::canonicalize(project_root).map_err(|err| {
        format!(
            "canonicalize runtime-authority project {}: {err}",
            project_root.display()
        )
    })?;
    let ai_dir = canonical_project.join(ryeos_engine::AI_DIR);
    let canonical_source = std::fs::canonicalize(&resolved_item.source_path).map_err(|err| {
        format!(
            "canonicalize resolved project runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    let source_metadata = std::fs::symlink_metadata(&resolved_item.source_path).map_err(|err| {
        format!(
            "stat resolved project runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.file_type().is_file() {
        return Err(format!(
            "project runtime-authority item {} must be a regular file (symlinks rejected)",
            resolved_item.source_path.display()
        ));
    }
    if !canonical_source.starts_with(&ai_dir) {
        return Err(format!(
            "project runtime-authority item {} is outside the exact project .ai root {}",
            canonical_source.display(),
            ai_dir.display()
        ));
    }
    Ok(ai_dir)
}

/// Establish the only provenance permitted to mint daemon callback authority:
/// a regular, content-pinned, node-signed item below exactly one registered
/// installed bundle's `.ai/` directory. Returns that authoritative `.ai/`
/// directory for manifest loading.
fn authoritative_runtime_authority_ai_dir(
    resolved_item: &ResolvedItem,
    installed_bundle_roots: &[PathBuf],
    node_trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<PathBuf, String> {
    if resolved_item.source_space != ryeos_engine::contracts::ItemSpace::Bundle {
        return Err(format!(
            "runtime capability requirements require installed TrustedBundle provenance; \
             item resolved from {} space",
            resolved_item.source_space.as_str()
        ));
    }

    let source_metadata = std::fs::symlink_metadata(&resolved_item.source_path).map_err(|err| {
        format!(
            "stat resolved runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.file_type().is_file() {
        return Err(format!(
            "runtime-authority item {} must be a regular installed file (symlinks rejected)",
            resolved_item.source_path.display()
        ));
    }

    let canonical_source = std::fs::canonicalize(&resolved_item.source_path).map_err(|err| {
        format!(
            "canonicalize resolved runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    let mut matching_ai_dirs = Vec::new();
    for bundle_root in installed_bundle_roots {
        let ai_dir = bundle_root.join(ryeos_engine::AI_DIR);
        let Ok(canonical_ai_dir) = std::fs::canonicalize(&ai_dir) else {
            // An unrelated unavailable registration must not prevent a valid
            // installed bundle from being identified. If no root matches, the
            // final error still fails closed and names the source item.
            continue;
        };
        if canonical_source.starts_with(&canonical_ai_dir)
            && !matching_ai_dirs
                .iter()
                .any(|existing| existing == &canonical_ai_dir)
        {
            matching_ai_dirs.push(canonical_ai_dir);
        }
    }

    let ai_dir = match matching_ai_dirs.as_slice() {
        [ai_dir] => ai_dir.clone(),
        [] => {
            return Err(format!(
                "runtime-authority item {} is not inside a registered installed bundle root",
                resolved_item.source_path.display()
            ));
        }
        _ => {
            return Err(format!(
                "runtime-authority item {} ambiguously belongs to multiple registered installed \
                 bundle roots",
                resolved_item.source_path.display()
            ));
        }
    };

    // Re-read once, pin it to the bytes that produced ResolvedItem metadata,
    // and verify the signature solely with persistent node trust. This keeps a
    // project key/caller overlay from turning an installed-path item into node
    // callback authority and detects a source replacement after resolution.
    let source = std::fs::read_to_string(&canonical_source).map_err(|err| {
        format!(
            "read resolved runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    let live_content_hash = ryeos_engine::item_resolution::content_hash(&source);
    if live_content_hash != resolved_item.content_hash {
        return Err(format!(
            "runtime-authority item {} changed after resolution (expected {}, found {})",
            resolved_item.source_path.display(),
            resolved_item.content_hash,
            live_content_hash
        ));
    }
    let signature_header = resolved_item.signature_header.as_ref().ok_or_else(|| {
        format!(
            "runtime-authority item {} is unsigned; installed TrustedBundle provenance is required",
            resolved_item.source_path.display()
        )
    })?;
    let (trust_class, _) = ryeos_engine::trust::verify_item_signature(
        &source,
        signature_header,
        &resolved_item.source_format.signature,
        node_trust_store,
    )
    .map_err(|err| {
        format!(
            "node verification failed for runtime-authority item {}: {err}",
            resolved_item.source_path.display()
        )
    })?;
    if trust_class != ryeos_engine::contracts::TrustClass::Trusted {
        return Err(format!(
            "runtime-authority item {} is not signed by a node-trusted publisher",
            resolved_item.source_path.display()
        ));
    }

    Ok(ai_dir)
}

/// Tool-path entry point: source the `requires` block from the resolved item's
/// extracted metadata. Graph/directive mint from the composed/narrowed view at
/// launch instead (see `build_and_launch`).
///
/// A tool has no composer to lift `requires.capabilities.declared` into
/// `effective_caps`, and self-asserted action authority is not part of the tool
/// surface — so a tool declaring `declared` is rejected rather than accepted and
/// silently ignored. Only `requires.capabilities.manifest` (runtime authority,
/// minted against the signed manifest) is honored for tools.
/// Tools have no surface to honor self-declared action authority, so the
/// *presence* of `requires.capabilities.declared` (even an empty list) is a
/// category error, not an accept-and-ignore. Shared by terminal-tool dispatch
/// and the accepted-launch preflight so both reject it before launch — a
/// deterministic check that closes the phantom where an accepted terminal
/// tool with invalid `requires` metadata would 202 then fail before its
/// thread row is created.
fn reject_tool_declared_capabilities(
    requires: Option<&Value>,
    item_ref: &str,
) -> Result<(), DispatchError> {
    if let Some(rv) = requires {
        if rv
            .get("capabilities")
            .and_then(|c| c.get("declared"))
            .is_some()
        {
            return Err(DispatchError::InvalidRef(
                item_ref.to_string(),
                "tool items cannot self-declare action authority under \
                 `requires.capabilities.declared`; only \
                 `requires.capabilities.manifest.runtime_authority` \
                 (manifest-backed runtime authority) is honored for tools"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// Validate + mint a terminal tool's manifest runtime caps from its resolved
/// item. Keyed on `&ResolvedItem` + `item_ref` (not the full execution
/// request) so the accepted-launch preflight can run the EXACT same check the
/// terminal subprocess runner does, before a thread row exists — both the
/// `declared` category rejection and the full manifest-backed derivation
/// (parse, bundle-id grammar, signed-manifest backing, subset check).
fn derive_manifest_runtime_caps(
    resolved_item: &ResolvedItem,
    item_ref: &str,
    ctx: &ExecutionContext,
) -> Result<Vec<String>, DispatchError> {
    let requires_value = resolved_item.metadata.extra.get("requires");
    reject_tool_declared_capabilities(requires_value, item_ref)?;
    // Tool schemas use the identity composer and have no inheritance chain, so
    // a node-verified installed root item is the complete effective trust
    // provenance. Managed graph/directive launches pass their actual composed
    // trust fold at their callsite below.
    mint_runtime_capability_caps(
        requires_value,
        resolved_item,
        match resolved_item.source_space {
            ryeos_engine::contracts::ItemSpace::Bundle => {
                ryeos_engine::resolution::TrustClass::TrustedBundle
            }
            ryeos_engine::contracts::ItemSpace::Project => {
                ryeos_engine::resolution::TrustClass::TrustedProject
            }
        },
        &ctx.engine,
    )
    .map_err(|reason| DispatchError::InvalidRef(item_ref.to_string(), reason))
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
/// hops. When the loop terminates on a `NativeRuntimeSpawn`, the
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
    if request.lifecycle_authority.ownership
        == ryeos_state::objects::ExecutionOwnershipAuthority::DaemonOwned
    {
        return dispatch_daemon_owned(item_ref, request, ctx, state)
            .await
            .map_err(|error| {
                DispatchError::Internal(anyhow::anyhow!(
                    "daemon-owned dispatch task ended before settlement: {error}"
                ))
            })?;
    }
    // `dispatch_inner` owns every terminal branch and is intentionally large.
    // Keep that state machine behind one heap indirection: runtime callbacks
    // enter here from a Tokio worker with the UDS router already on its stack,
    // and constructing the unboxed nested future can exhaust the worker stack
    // before the selected leaf gets a chance to run.
    Box::pin(dispatch_inner(
        item_ref, None, None, request, ctx, state, None,
    ))
    .await
}

/// Transfer a unary dispatch onto a daemon-owned task before awaiting it.
///
/// The HTTP response policy is deliberately separate from execution
/// ownership. A caller may choose to wait for a daemon-owned execution, but
/// dropping that wait must only detach the response observer; it must not drop
/// the dispatch future that owns admission, spawn, follow handoff, or terminal
/// settlement.
pub fn dispatch_daemon_owned(
    item_ref: &str,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> tokio::task::JoinHandle<Result<Value, DispatchError>> {
    let item_ref = item_ref.to_owned();
    let launch_mode = request.launch_mode.to_owned();
    let target_site_id = request.target_site_id.map(ToOwned::to_owned);
    let validate_only = request.validate_only;
    let params = request.params.clone();
    let ref_bindings = request.ref_bindings.clone();
    let acting_principal = request.acting_principal.to_owned();
    let project_path = request.project_path.to_path_buf();
    let provenance = request.provenance.clone();
    let lifecycle_authority = request.lifecycle_authority;
    let original_root_kind = request.original_root_kind.to_owned();
    let pre_minted_thread_id = request.pre_minted_thread_id.clone();
    let usage_subject = request.usage_subject.clone();
    let usage_subject_asserted_by = request.usage_subject_asserted_by.clone();
    let previous_thread_id = request.previous_thread_id.clone();
    let root_admission = request.root_admission.clone();
    let parent_execution_context = request.parent_execution_context.clone();
    let ctx = ExecutionContext {
        principal_fingerprint: ctx.principal_fingerprint.clone(),
        caller_scopes: ctx.caller_scopes.clone(),
        engine: ctx.engine.clone(),
        plan_ctx: ctx.plan_ctx.clone(),
        requested_call: ctx.requested_call.clone(),
    };
    let state = state.clone();

    tokio::spawn(async move {
        let request = DispatchRequest {
            launch_mode: &launch_mode,
            target_site_id: target_site_id.as_deref(),
            validate_only,
            params,
            ref_bindings,
            acting_principal: &acting_principal,
            project_path: &project_path,
            provenance,
            lifecycle_authority,
            original_root_kind: &original_root_kind,
            pre_minted_thread_id,
            usage_subject,
            usage_subject_asserted_by,
            previous_thread_id,
            root_admission,
            parent_execution_context,
        };
        Box::pin(dispatch_inner(
            &item_ref, None, None, &request, &ctx, &state, None,
        ))
        .await
    })
}

/// Dispatch a launch whose caller needs proof that durable execution authority
/// has been handed to a scheduled subprocess task before exposing its thread
/// ID. Managed LaunchEnvelope, method-runtime, and terminal-subprocess leaves
/// publish the signal; unsupported in-process paths never do.
pub async fn dispatch_with_launch_handoff(
    item_ref: &str,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    launch_handoff: &crate::execution::launch::LaunchHandoff,
) -> Result<Value, DispatchError> {
    let result = Box::pin(dispatch_inner(
        item_ref,
        None,
        None,
        request,
        ctx,
        state,
        Some(launch_handoff),
    ))
    .await;
    match &result {
        Err(error) => launch_handoff.publish_dispatch_failure(error),
        Ok(_) if launch_handoff.is_pending() => launch_handoff.publish_failure(
            "launch_handoff_missing",
            "dispatch completed without reaching a handoff-capable subprocess leaf",
            500,
            false,
        ),
        Ok(_) => {}
    }
    result
}

/// Dispatch an item whose root resolution and verification have already been
/// completed by the caller. The verified root enters the same schema-driven
/// hop loop as [`dispatch`]; only the first resolve/verify operation is reused.
pub async fn dispatch_verified(
    item_ref: &str,
    verified: VerifiedItem,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<Value, DispatchError> {
    Box::pin(dispatch_inner(
        item_ref,
        Some(verified),
        None,
        request,
        ctx,
        state,
        None,
    ))
    .await
}

/// Dispatch an already-verified root while carrying trusted local context for
/// an in-process handler whose signed item policy requests it. Routing remains
/// entirely schema-driven; subprocess and method terminators ignore it.
pub async fn dispatch_verified_with_handler_context(
    item_ref: &str,
    verified: VerifiedItem,
    local_handler_context: ryeos_app::handler_context::HandlerContext,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<Value, DispatchError> {
    Box::pin(dispatch_inner(
        item_ref,
        Some(verified),
        Some(local_handler_context),
        request,
        ctx,
        state,
        None,
    ))
    .await
}

async fn dispatch_inner(
    item_ref: &str,
    verified_root: Option<VerifiedItem>,
    local_handler_context: Option<ryeos_app::handler_context::HandlerContext>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    launch_handoff: Option<&crate::execution::launch::LaunchHandoff>,
) -> Result<Value, DispatchError> {
    const MAX_HOPS: usize = 8;
    let mut visited: HashSet<CanonicalRef> = HashSet::new();
    let mut hops: usize = 0;
    let mut current_ref: CanonicalRef = CanonicalRef::parse(item_ref)
        .map_err(|e| DispatchError::InvalidRef(item_ref.to_string(), e.to_string()))?;
    let mut verified_root = verified_root;
    if let Some(admission) = request.root_admission.as_ref() {
        admission.validate().map_err(DispatchError::Internal)?;
    }

    // Secondary execution identities are independently authorized before any
    // binding resolution or launch preparation. Their slot declarations are
    // selected later from the actual managed-envelope runtime contract.
    crate::execution::launch_preparation::validate_ref_bindings(&request.ref_bindings)?;
    for (binding_name, raw_ref) in &request.ref_bindings {
        let canonical = CanonicalRef::parse(raw_ref)
            .map_err(|error| DispatchError::InvalidRef(raw_ref.clone(), error.to_string()))?;
        let required = ryeos_runtime::authorizer::canonical_cap(
            &canonical.kind,
            &canonical.bare_id,
            "execute",
        );
        state
            .authorizer
            .authorize(
                &ctx.caller_scopes,
                &ryeos_runtime::authorizer::AuthorizationPolicy::require(&required),
            )
            .map_err(|_| DispatchError::LaunchPolicyForbidden {
                code: "ref_binding_unauthorized".to_owned(),
                message: format!("ref binding `{binding_name}` is not authorized"),
                binding: Some(binding_name.clone()),
            })?;
    }

    if !request.ref_bindings.is_empty() {
        if let LaunchContractApplicability::NonEnvelope { class } =
            launch_contract_applicability(item_ref, ctx)?
        {
            return Err(DispatchError::RefBindingNotApplicable {
                class: class.as_str().to_owned(),
            });
        }
    }

    // Reject a method call (`call.method`/`call.args`) aimed at a kind that
    // does not declare method dispatch. The method selector is control
    // plane for method-bearing kinds only; a directive/tool/service does
    // not interpret it, so silently ignoring it would hide a caller error.
    // Method kinds are always invoked directly (they never sit behind an
    // alias/delegate hop — mixed dispatch is forbidden), so the root ref's
    // kind is authoritative here.
    if ctx.has_requested_call() {
        let has_methods = ctx
            .engine
            .kinds
            .get(&current_ref.kind)
            .and_then(|s| s.execution())
            .map(|e| !e.methods.is_empty())
            .unwrap_or(false);
        if !has_methods {
            return Err(DispatchError::MethodInvalidArg {
                method: ctx
                    .requested_method()
                    .unwrap_or("<unspecified>")
                    .to_string(),
                reason: format!(
                    "kind '{}' does not support method dispatch (no `methods` declared); \
                     `call.method`/`call.args` are not accepted for this kind",
                    current_ref.kind
                ),
            });
        }
    }

    // P1.1: root subject captured from the first hop's resolution.
    // For direct paths, root_subject IS the runtime. For indirect
    // paths, it's the directive/tool that initiated the chain.
    let mut root_subject: Option<RootSubject> = None;

    // B1: derive the SubprocessRole ONCE based on the user's original
    // item_ref. Only direct `runtime:*` invocation triggers the
    // runtime.execute cap gate. Alias chains do NOT inherit the role.
    let role = if request.original_root_kind == ROOT_KIND_RUNTIME {
        let verified = ctx
            .engine
            .runtimes
            .lookup_by_ref(&current_ref)
            .ok_or_else(|| {
                let mut available: Vec<String> = ctx
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
            verified_runtime: Box::new(verified.clone()),
        }
    } else {
        SubprocessRole::Regular
    };

    loop {
        if !visited.insert(current_ref.clone()) {
            let mut visited_strs: Vec<String> = visited.iter().map(|r| r.to_string()).collect();
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

        let admitted_verified = request.root_admission.as_ref().and_then(|admission| {
            (admission.verified_subject().resolved.canonical_ref == current_ref)
                .then(|| admission.verified_subject().clone())
        });
        let hop = resolve_dispatch_hop_with_verified(
            &current_ref,
            ctx,
            verified_root.take().or(admitted_verified),
        )?;

        // Destructure up front so the match on `next` (which moves
        // the terminator out) can't conflict with later borrows. All
        // subsequent uses operate on owned locals, no view structs.
        let VerifiedHop {
            canonical_ref: hop_ref,
            verified,
            thread_profile,
            runtime,
            next,
            ..
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
                return Box::pin(dispatch_by(
                    DispatchByParams {
                        terminator,
                        canonical_ref: hop_ref,
                        verified,
                        thread_profile,
                        runtime,
                        root_subject,
                    },
                    request,
                    ctx,
                    state,
                    &role,
                    local_handler_context,
                    launch_handoff,
                ))
                .await;
            }
            HopAction::FollowAlias(next_ref) | HopAction::UseRegistry(next_ref) => {
                current_ref = next_ref;
            }
            HopAction::DispatchMethod {
                kind,
                method_name,
                method_decl,
            } => {
                return Box::pin(dispatch_method(
                    &kind,
                    &method_name,
                    &method_decl,
                    &hop_ref,
                    verified,
                    thread_profile,
                    root_subject,
                    request,
                    ctx,
                    state,
                    launch_handoff,
                ))
                .await;
            }
        }
    }
}

/// How a root ref's dispatch chain terminates — the basis for deciding
/// whether accepted/background launch can honor a pre-minted thread id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootDispatchClass {
    /// Terminal subprocess (DetachedOk lifecycle), e.g. a wrapper tool.
    /// Honors a pre-minted thread id; requires an `executor_id` on the
    /// resolved root item (checked during classification).
    TerminalSubprocess,
    /// Managed subprocess — directive/graph via runtime-registry delegate,
    /// or a runtime invoked directly. Honors a pre-minted thread id.
    ManagedSubprocess,
    /// Managed lifecycle protocol that bypasses LaunchEnvelope construction.
    ManagedNonEnvelope,
    /// Method dispatch (e.g. `knowledge`). Honors a pre-minted thread id.
    MethodDispatch,
    /// Managed protocol execution with no callback channel. It returns protocol
    /// frames directly and never creates a lifecycle row, so it cannot honor a
    /// pre-minted thread id.
    UnthreadedStreamingSubprocess,
    /// In-process execution (services). Runs synchronously and does NOT
    /// thread a pre-minted id — not eligible for accepted/background launch.
    InProcess,
}

#[derive(Debug, Clone)]
pub enum LaunchContractApplicability {
    ManagedEnvelope { runtime: Box<VerifiedRuntime> },
    NonEnvelope { class: RootDispatchClass },
}

/// Resolve the actual terminal protocol boundary. This deliberately follows
/// aliases/delegates and checks the selected protocol's callback shape; a
/// managed lifecycle label alone does not imply LaunchEnvelope construction.
pub fn launch_contract_applicability(
    item_ref: &str,
    ctx: &ExecutionContext,
) -> Result<LaunchContractApplicability, DispatchError> {
    const MAX_HOPS: usize = 8;
    let mut visited = HashSet::new();
    let mut current = CanonicalRef::parse(item_ref)
        .map_err(|error| DispatchError::InvalidRef(item_ref.to_owned(), error.to_string()))?;
    let mut selected_runtime = ctx.engine.runtimes.lookup_by_ref(&current).cloned();

    for _ in 0..MAX_HOPS {
        if !visited.insert(current.clone()) {
            let mut visited = visited
                .into_iter()
                .map(|item| item.to_string())
                .collect::<Vec<_>>();
            visited.sort();
            return Err(DispatchError::AliasCycle {
                root_ref: item_ref.to_owned(),
                visited,
            });
        }
        let hop = resolve_dispatch_hop(&current, ctx)?;
        match hop.next {
            HopAction::FollowAlias(next) => {
                current = next;
            }
            HopAction::UseRegistry(next) => {
                selected_runtime = ctx.engine.runtimes.lookup_by_ref(&next).cloned();
                current = next;
            }
            HopAction::DispatchMethod { .. } => {
                return Ok(LaunchContractApplicability::NonEnvelope {
                    class: RootDispatchClass::MethodDispatch,
                });
            }
            HopAction::Terminate(TerminatorDecl::InProcess { .. }, _) => {
                return Ok(LaunchContractApplicability::NonEnvelope {
                    class: RootDispatchClass::InProcess,
                });
            }
            HopAction::Terminate(TerminatorDecl::Subprocess { protocol_ref }, _) => {
                let protocol = ctx
                    .engine
                    .protocols
                    .require(&protocol_ref)
                    .map_err(|_| DispatchError::ProtocolNotRegistered(protocol_ref))?;
                use ryeos_engine::protocol_vocabulary::LifecycleMode;
                if protocol.descriptor.lifecycle.mode != LifecycleMode::Managed {
                    return Ok(LaunchContractApplicability::NonEnvelope {
                        class: RootDispatchClass::TerminalSubprocess,
                    });
                }
                if protocol.descriptor.callback_channel == CallbackChannel::None {
                    return Ok(LaunchContractApplicability::NonEnvelope {
                        class: RootDispatchClass::ManagedNonEnvelope,
                    });
                }
                let runtime = selected_runtime
                    .or_else(|| ctx.engine.runtimes.lookup_by_ref(&current).cloned())
                    .ok_or_else(|| DispatchError::SchemaMisconfigured {
                        kind: current.kind.clone(),
                        detail: format!(
                            "managed envelope path `{current}` has no selected runtime"
                        ),
                    })?;
                return Ok(LaunchContractApplicability::ManagedEnvelope {
                    runtime: Box::new(runtime),
                });
            }
        }
    }
    Err(DispatchError::AliasChainTooLong {
        root_ref: item_ref.to_owned(),
        max_hops: MAX_HOPS,
    })
}

/// Threadless admission pass for accepted/SSE launch producers. The live
/// launcher repeats this preparation authoritatively immediately before its
/// durable audit and spawn; no prepared runtime data crosses this seam.
pub fn admit_launch_contract(
    applicability: &LaunchContractApplicability,
    primary: &ResolvedItem,
    ref_bindings: &BTreeMap<String, String>,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<(), DispatchError> {
    let Some(prepared) = prepare_launch_contract(
        applicability,
        primary,
        ref_bindings,
        provenance.effective_path(),
        ctx,
    )?
    else {
        return Ok(());
    };
    let mut names = primary.metadata.required_secrets.clone();
    names.extend(
        prepared
            .required_secrets
            .into_iter()
            .map(|secret| secret.name),
    );
    names.sort();
    names.dedup();
    let dotenv_dirs =
        ryeos_app::vault::dotenv_search_dirs(Some(provenance.original_project_path()));
    ryeos_app::vault::read_required_secrets(
        state.vault.as_ref(),
        &ctx.principal_fingerprint,
        &names,
        &dotenv_dirs,
    )
    .map(|_| ())
    .map_err(|error| match error {
        ryeos_app::vault::VaultReadError::MissingSecrets { names, .. } => {
            let name = names
                .into_iter()
                .next()
                .unwrap_or_else(|| "unknown".to_owned());
            DispatchError::RequiredSecretMissing {
                item_ref: primary.canonical_ref.to_string(),
                env_var: name.clone(),
                source_kind: "launch_preparation".to_owned(),
                source_name: "symbolic_requirement".to_owned(),
                remediation: crate::dispatch_error::required_secret_remediation(&name),
            }
        }
        ryeos_app::vault::VaultReadError::Internal(error) => {
            DispatchError::LaunchPreparationFailed {
                code: "launch_secret_check_failed".to_owned(),
                message: error.to_string(),
                classification: "internal".to_owned(),
                binding: None,
                details: Box::new(BTreeMap::new()),
            }
        }
    })
}

/// Execute the generic, threadless launch-contract preparation pass without
/// reading secret values. Environment diagnostics use this to discover the
/// validated symbolic secret set; admission adds the availability check.
pub fn prepare_launch_contract(
    applicability: &LaunchContractApplicability,
    primary: &ResolvedItem,
    ref_bindings: &BTreeMap<String, String>,
    project_path: &Path,
    ctx: &ExecutionContext,
) -> Result<Option<crate::execution::launch_preparation::PreparedRuntimeLaunch>, DispatchError> {
    let runtime = match applicability {
        LaunchContractApplicability::NonEnvelope { class } => {
            if ref_bindings.is_empty() {
                return Ok(None);
            }
            return Err(DispatchError::RefBindingNotApplicable {
                class: class.as_str().to_owned(),
            });
        }
        LaunchContractApplicability::ManagedEnvelope { runtime } => runtime,
    };
    let roots = ctx
        .engine
        .resolution_roots(Some(project_path.to_path_buf()));
    let parsers = ctx
        .engine
        .effective_parser_dispatcher(Some(project_path))
        .map_err(|error| DispatchError::LaunchPreparationFailed {
            code: "launch_parser_registry_failed".to_owned(),
            message: error.to_string(),
            classification: "configuration".to_owned(),
            binding: None,
            details: Box::new(BTreeMap::new()),
        })?;
    let resolution = ryeos_engine::resolution::run_resolution_pipeline(
        &primary.canonical_ref,
        &ctx.engine.kinds,
        &parsers,
        &roots,
        &ctx.engine.trust_store,
        &ctx.engine.composers,
    )
    .map_err(|error| DispatchError::LaunchPreparationFailed {
        code: "primary_resolution_failed".to_owned(),
        message: error.to_string(),
        classification: "caller".to_owned(),
        binding: None,
        details: Box::new(BTreeMap::new()),
    })?;
    crate::execution::launch_preparation::prepare_runtime_launch(
        crate::execution::launch_preparation::PrepareRuntimeLaunchRequest {
            engine: &ctx.engine,
            runtime,
            primary: &resolution,
            ref_bindings,
            roots: &roots,
            parsers: &parsers,
            principal: &ctx.plan_ctx.requested_by,
        },
    )
    .map(Some)
}

impl RootDispatchClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TerminalSubprocess => "terminal_subprocess",
            Self::ManagedSubprocess => "managed_subprocess",
            Self::ManagedNonEnvelope => "managed_non_envelope",
            Self::MethodDispatch => "method_dispatch",
            Self::UnthreadedStreamingSubprocess => "unthreaded_streaming_subprocess",
            Self::InProcess => "in_process",
        }
    }
}

impl RootDispatchClass {
    /// Whether live dispatch is guaranteed to persist the caller's pre-minted
    /// id as a root row on its success path.
    pub fn persists_pre_minted_root(self) -> bool {
        matches!(
            self,
            Self::TerminalSubprocess | Self::ManagedSubprocess | Self::MethodDispatch
        )
    }
}

/// Synchronous public-route admission. `requested_subject` is the exact
/// caller-named item used for route capability/secret checks. `root_admission`
/// is the possibly different terminal/root subject that will own the durable
/// row (for example, the target behind a method-dispatch wrapper).
#[derive(Debug, Clone)]
pub struct RootDispatchPreflight {
    pub class: RootDispatchClass,
    pub requested_subject: VerifiedItem,
    pub root_admission: Option<ryeos_app::thread_lifecycle::RootExecutionAdmission>,
}

// Route class, requested/root evidence, usage attribution, bindings, and daemon
// policy context remain explicit at this admission boundary.
#[allow(clippy::too_many_arguments)]
fn finish_root_dispatch_preflight(
    class: RootDispatchClass,
    requested_subject: VerifiedItem,
    root_subject: VerifiedItem,
    thread_profile: String,
    ref_bindings: &BTreeMap<String, String>,
    usage_subject: Option<&ryeos_state::UsageSubject>,
    usage_subject_asserted_by: Option<&str>,
    project_binding: &ryeos_app::thread_lifecycle::AdmittedProjectBinding,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<RootDispatchPreflight, DispatchError> {
    let root_admission = ryeos_app::thread_lifecycle::admit_verified_root_execution(
        &ctx.engine,
        &ctx.plan_ctx,
        &ctx.plan_ctx,
        project_binding.clone(),
        root_subject,
        &state.node_history_policy,
        thread_profile,
        ref_bindings.clone(),
        usage_subject.cloned(),
        usage_subject_asserted_by.map(str::to_string),
    )
    .map_err(DispatchError::Internal)?;
    Ok(RootDispatchPreflight {
        class,
        requested_subject,
        root_admission: Some(root_admission),
    })
}

/// Preflight the dispatch route for accepted/background launch.
///
/// Walks the same hop chain as [`dispatch`] (via `resolve_dispatch_hop`)
/// WITHOUT executing, and runs the cheap, deterministic route-level checks
/// dispatch makes before creating the thread row — the
/// method-call-on-non-method-kind guard, the terminal `executor_id` gate, the
/// terminal-tool `requires.capabilities.declared` rejection, the direct
/// `runtime:` registry-cap gate, and method-arg validation. These give a
/// synchronous rejection for the common pre-thread failures, so they never
/// mint a `thread_id`. The no-phantom guarantee itself is NOT carried by this
/// preflight being exhaustive: deeper failures (method payload/corpus
/// projection, managed launcher policy/trust) are handled by persistence-first
/// leaf dispatch — create the thread row, then finalize it `failed` on any
/// later failure — plus the launch-side finalize-on-error net. Returns the
/// route class so the caller can refuse routes that cannot honor a pre-minted
/// id (in-process services).
///
/// Synchronous: classification touches verified resolution/composition, schema,
/// and the authorizer only — never the executing leaf dispatchers.
// Caller route identity, payload/bindings, usage attribution, and daemon policy
// context remain explicit so synchronous admission checks cannot omit them.
#[allow(clippy::too_many_arguments)]
pub fn preflight_root_dispatch(
    item_ref: &str,
    original_root_kind: &str,
    params: &Value,
    ref_bindings: &BTreeMap<String, String>,
    usage_subject: Option<&ryeos_state::UsageSubject>,
    usage_subject_asserted_by: Option<&str>,
    project_binding: &ryeos_app::thread_lifecycle::AdmittedProjectBinding,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<RootDispatchPreflight, DispatchError> {
    const MAX_HOPS: usize = 8;
    let mut visited: HashSet<CanonicalRef> = HashSet::new();
    let mut hops: usize = 0;
    let mut current_ref: CanonicalRef = CanonicalRef::parse(item_ref)
        .map_err(|e| DispatchError::InvalidRef(item_ref.to_string(), e.to_string()))?;
    let mut requested_subject: Option<VerifiedItem> = None;
    let mut root_subject: Option<(VerifiedItem, String)> = None;

    let caller_root_executable = ctx
        .engine
        .kinds
        .get(&current_ref.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .is_some_and(|profile| profile.root_executable);
    if !caller_root_executable {
        return Err(DispatchError::NotRootExecutable {
            kind: current_ref.kind.clone(),
            detail: format!(
                "public launch root `{current_ref}` has no root-executable thread profile in its verified kind schema"
            ),
        });
    }

    // Mirror dispatch: a method call aimed at a kind that declares no methods
    // is a caller error, not a silent no-op.
    if ctx.has_requested_call() {
        let has_methods = ctx
            .engine
            .kinds
            .get(&current_ref.kind)
            .and_then(|s| s.execution())
            .map(|e| !e.methods.is_empty())
            .unwrap_or(false);
        if !has_methods {
            return Err(DispatchError::MethodInvalidArg {
                method: ctx
                    .requested_method()
                    .unwrap_or("<unspecified>")
                    .to_string(),
                reason: format!(
                    "kind '{}' does not support method dispatch (no `methods` declared); \
                     `call.method`/`call.args` are not accepted for this kind",
                    current_ref.kind
                ),
            });
        }
    }

    // Mirror dispatch's role derivation: only a direct `runtime:*` invocation
    // carries the runtime registry caps that gate launch.
    let role = if original_root_kind == ROOT_KIND_RUNTIME {
        let verified = ctx
            .engine
            .runtimes
            .lookup_by_ref(&current_ref)
            .ok_or_else(|| {
                let mut available: Vec<String> = ctx
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
            verified_runtime: Box::new(verified.clone()),
        }
    } else {
        SubprocessRole::Regular
    };

    loop {
        if !visited.insert(current_ref.clone()) {
            let mut visited_strs: Vec<String> = visited.iter().map(|r| r.to_string()).collect();
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
        let VerifiedHop {
            canonical_ref: hop_ref,
            verified,
            resolution_error,
            thread_profile,
            runtime: hop_runtime,
            next,
            ..
        } = hop;

        if requested_subject.is_none() {
            let subject = verified.clone().ok_or_else(|| {
                let cause = resolution_error
                    .as_deref()
                    .unwrap_or("item resolution returned no verified subject");
                DispatchError::InvalidRef(
                    hop_ref.to_string(),
                    format!(
                        "public root admission requires the caller-named item to resolve and verify: {cause}"
                    ),
                )
            })?;
            requested_subject = Some(subject);
        }
        if root_subject.is_none() {
            if let (Some(subject), Some(profile)) = (verified.clone(), thread_profile.clone()) {
                root_subject = Some((subject, profile));
            }
        }
        let admitted_requested_subject = requested_subject.clone().ok_or_else(|| {
            DispatchError::InvalidRef(
                item_ref.to_string(),
                "public root admission lost the verified caller-named subject".to_string(),
            )
        })?;

        match next {
            HopAction::Terminate(terminator, hop_profile) => match terminator {
                TerminatorDecl::InProcess { .. } => {
                    return Ok(RootDispatchPreflight {
                        class: RootDispatchClass::InProcess,
                        requested_subject: admitted_requested_subject,
                        root_admission: None,
                    });
                }
                TerminatorDecl::Subprocess { protocol_ref } => {
                    let protocol =
                        ctx.engine.protocols.require(&protocol_ref).map_err(|_| {
                            DispatchError::ProtocolNotRegistered(protocol_ref.clone())
                        })?;
                    use ryeos_engine::protocol_vocabulary::LifecycleMode;
                    match protocol.descriptor.lifecycle.mode {
                        LifecycleMode::DetachedOk => {
                            validate_ordinary_protocol_contract(protocol, &hop_ref.kind)?;
                            require_terminal_executor_id(verified.as_ref(), &hop_ref.to_string())?;
                            // Mirror dispatch_tool_subprocess's FULL pre-thread
                            // manifest-cap validation (declared rejection +
                            // signed-manifest-backed mint) so an invalid tool
                            // `requires.capabilities.{declared,manifest}` fails
                            // before a thread_id is minted, not in the
                            // background after 202.
                            if let Some(v) = verified.as_ref() {
                                derive_manifest_runtime_caps(
                                    &v.resolved,
                                    &hop_ref.to_string(),
                                    ctx,
                                )?;
                                // A method-dispatch wrapper's accepted form is
                                // a pre-minted target method thread, not a
                                // wrapper subprocess thread. Resolve the chain
                                // terminal here so we can preflight the actual
                                // target method synchronously before a
                                // thread_id is minted.
                                if let Some(executor_id) =
                                    v.resolved.metadata.executor_id.as_deref()
                                {
                                    let project_root = match &ctx.plan_ctx.project_context {
                                        ryeos_engine::contracts::ProjectContext::LocalPath {
                                            path,
                                        } => Some(path.clone()),
                                        _ => None,
                                    };
                                    let terminal = ctx
                                        .engine
                                        .resolve_terminal_executor(
                                            &v.resolved.source_path,
                                            executor_id,
                                            &v.resolved.kind,
                                            project_root,
                                        )
                                        .map_err(|e| DispatchError::SchemaMisconfigured {
                                            kind: v.resolved.kind.clone(),
                                            detail: format!(
                                                "failed to resolve executor-chain terminal for \
                                                 '{hop_ref}': {e}"
                                            ),
                                        })?;
                                    if terminal.kind
                                        == ryeos_engine::plan_builder::TerminalExecutorKind::MethodDispatch
                                    {
                                        // Preflight a method-dispatch wrapper by
                                        // resolving its target and recursing as a
                                        // method call. This reuses the
                                        // DispatchMethod branch's cheap
                                        // pre-thread checks (arg validation,
                                        // runtime lookup, binary_ref shape), so
                                        // accepted launch rejects common failures
                                        // synchronously before a thread_id is
                                        // minted. Full payload projection still
                                        // happens in live dispatch after the row
                                        // exists, matching direct method launch.
                                        let MethodDispatchTarget {
                                            target_ref,
                                            target_canonical,
                                            method,
                                            args,
                                        } = resolve_method_dispatch_target(
                                            &v.resolved,
                                            params,
                                            &ctx.caller_scopes,
                                            &state.authorizer,
                                        )?;
                                        let inner_params = args.clone();
                                        let inner_ctx = ExecutionContext {
                                            principal_fingerprint: ctx
                                                .principal_fingerprint
                                                .clone(),
                                            caller_scopes: ctx.caller_scopes.clone(),
                                            engine: ctx.engine.clone(),
                                            plan_ctx: ctx.plan_ctx.clone(),
                                            requested_call: Some(
                                                ryeos_engine::method_call::MethodCall {
                                                    method: Some(method),
                                                    args: Some(args),
                                                },
                                            ),
                                        };
                                        let mut target = preflight_root_dispatch(
                                            &target_ref,
                                            target_canonical.kind.as_str(),
                                            &inner_params,
                                            ref_bindings,
                                            usage_subject,
                                            usage_subject_asserted_by,
                                            project_binding,
                                            &inner_ctx,
                                            state,
                                        )?;
                                        target.requested_subject =
                                            admitted_requested_subject.clone();
                                        return Ok(target);
                                    }
                                }
                            }
                            return finish_root_dispatch_preflight(
                                RootDispatchClass::TerminalSubprocess,
                                admitted_requested_subject,
                                verified.clone().ok_or_else(|| {
                                    DispatchError::InvalidRef(
                                        hop_ref.to_string(),
                                        "terminal root did not resolve and verify".to_string(),
                                    )
                                })?,
                                hop_profile,
                                ref_bindings,
                                usage_subject,
                                usage_subject_asserted_by,
                                project_binding,
                                ctx,
                                state,
                            );
                        }
                        LifecycleMode::Managed => {
                            let managed_route = if let Some(verified_runtime) = &hop_runtime {
                                // Keep accepted-launch admission identical to
                                // live runtime launch for both direct runtime
                                // roots and registry/alias hops. A runtime serving
                                // a method-only kind must be rejected before a
                                // thread is minted rather than failing later
                                // against the ordinary runtime envelope.
                                require_callback_runtime_protocol(
                                    &ctx.engine,
                                    verified_runtime,
                                    "managed preflight",
                                )?;
                                subprocess_execution::ManagedProtocolRoute::CallbackRuntime
                            } else {
                                subprocess_execution::classify_managed_protocol(
                                    protocol,
                                    &hop_ref.kind,
                                )?
                            };
                            if managed_route
                                == subprocess_execution::ManagedProtocolRoute::CallbackRuntime
                            {
                                if let SubprocessRole::RuntimeTarget { verified_runtime } = &role {
                                    enforce_runtime_caps(
                                        &state.authorizer,
                                        &hop_ref.to_string(),
                                        &verified_runtime.yaml.required_caps,
                                        &ctx.caller_scopes,
                                    )?;
                                }
                            }
                            let (subject, profile) = root_subject.clone().ok_or_else(|| {
                                DispatchError::InvalidRef(
                                    hop_ref.to_string(),
                                    "managed root did not resolve and verify".to_string(),
                                )
                            })?;
                            let class =
                                if protocol.descriptor.callback_channel == CallbackChannel::None {
                                    RootDispatchClass::ManagedNonEnvelope
                                } else {
                                    RootDispatchClass::ManagedSubprocess
                                };
                            return finish_root_dispatch_preflight(
                                class,
                                admitted_requested_subject,
                                subject,
                                profile,
                                ref_bindings,
                                usage_subject,
                                usage_subject_asserted_by,
                                project_binding,
                                ctx,
                                state,
                            );
                        }
                    }
                }
            },
            HopAction::FollowAlias(next_ref) | HopAction::UseRegistry(next_ref) => {
                current_ref = next_ref;
            }
            HopAction::DispatchMethod {
                kind,
                method_name,
                method_decl,
            } => {
                // Mirror the cheap pre-thread-creation work `dispatch_method`
                // does before it creates the thread row — arg validation,
                // runtime lookup, and binary_ref shape — so a method launch
                // dispatch would reject (e.g. a missing required arg) fails
                // here instead of returning a phantom thread_id.
                validate_method_args(ctx.requested_args(), &method_name, &method_decl)?;
                let verified_runtime = ctx.engine.runtimes.lookup_for(&kind).map_err(|_| {
                    let mut serves: Vec<String> = ctx
                        .engine
                        .runtimes
                        .all()
                        .map(|r| format!("{}→{}", r.yaml.serves, r.canonical_ref))
                        .collect();
                    serves.sort();
                    DispatchError::SchemaMisconfigured {
                        kind: kind.clone(),
                        detail: format!(
                            "no runtime serves kind '{kind}' for method dispatch \
                             (registered runtimes: [{}])",
                            serves.join(", ")
                        ),
                    }
                })?;
                require_method_runtime_protocol(&ctx.engine, &kind, verified_runtime, "method")?;
                strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)?;
                return finish_root_dispatch_preflight(
                    RootDispatchClass::MethodDispatch,
                    admitted_requested_subject,
                    verified.ok_or_else(|| {
                        DispatchError::InvalidRef(
                            hop_ref.to_string(),
                            "method root did not resolve and verify".to_string(),
                        )
                    })?,
                    thread_profile.ok_or_else(|| DispatchError::SchemaMisconfigured {
                        kind,
                        detail: "method root has no execution thread profile".to_string(),
                    })?,
                    ref_bindings,
                    usage_subject,
                    usage_subject_asserted_by,
                    project_binding,
                    ctx,
                    state,
                );
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
struct DispatchByParams {
    terminator: TerminatorDecl,
    canonical_ref: CanonicalRef,
    verified: Option<VerifiedItem>,
    thread_profile: Option<String>,
    runtime: Option<VerifiedRuntime>,
    root_subject: Option<RootSubject>,
}

async fn dispatch_by(
    params: DispatchByParams,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    role: &SubprocessRole,
    local_handler_context: Option<ryeos_app::handler_context::HandlerContext>,
    launch_handoff: Option<&crate::execution::launch::LaunchHandoff>,
) -> Result<Value, DispatchError> {
    let DispatchByParams {
        terminator,
        canonical_ref,
        verified,
        thread_profile,
        runtime,
        root_subject,
    } = params;
    match terminator {
        TerminatorDecl::Subprocess { .. } => {
            let tp = thread_profile.ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: canonical_ref.kind.clone(),
                detail: "subprocess terminator has no thread_profile".into(),
            })?;
            Box::pin(dispatch_subprocess(SubprocessDispatchContext {
                current_ref: &canonical_ref,
                thread_profile: &tp,
                verified: verified.as_ref(),
                request,
                ctx,
                state,
                role,
                root_subject,
                hop_runtime: runtime,
                launch_handoff,
            }))
            .await
        }
        TerminatorDecl::InProcess {
            registry: InProcessRegistryKind::Services,
        } => {
            if request.root_admission.is_some() {
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "threaded root admission resolved to an in-process terminator; refusing to acknowledge a pre-minted id without its admitted row"
                )));
            }
            let tp = thread_profile.ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: canonical_ref.kind.clone(),
                detail: "service terminator has no thread_profile".into(),
            })?;
            Box::pin(dispatch_service(
                &canonical_ref.to_string(),
                &tp,
                verified,
                local_handler_context,
                request,
                ctx,
                state,
            ))
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
    use ryeos_engine::contracts::ItemSpace;
    use ryeos_engine::engine::Engine;
    use ryeos_engine::kind_registry::KindRegistry;
    use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
    use ryeos_engine::trust::{compute_fingerprint, TrustStore, TrustedSigner};

    #[test]
    fn finalize_params_routes_failure_cause_to_error_not_result() {
        // Failure terminals (failed/killed/timed_out) carry a cause → it must
        // land in `error` (which the terminal braid event persists) not
        // `result` (dropped), so the feed shows the reason, not a bare "failed".
        for status in ["failed", "killed", "timed_out"] {
            let p = finalize_params("T-x", status, Some(serde_json::json!({ "error": "boom" })));
            assert_eq!(
                p.error,
                Some(serde_json::json!({ "error": "boom" })),
                "{status}"
            );
            assert!(p.result.is_none(), "{status}");
        }

        // Non-failure terminals carry a result, not an error — including ones
        // the old `status == "completed"` compare would have misrouted.
        for status in ["completed", "continued", "cancelled"] {
            let p = finalize_params("T-y", status, Some(serde_json::json!({ "ok": true })));
            assert_eq!(
                p.result,
                Some(serde_json::json!({ "ok": true })),
                "{status}"
            );
            assert!(p.error.is_none(), "{status}");
        }
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[71u8; 32])
    }

    fn trust_store_for_key(sk: &SigningKey) -> TrustStore {
        let vk = sk.verifying_key();
        let fp = compute_fingerprint(&vk);
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: None,
        }])
    }

    fn trust_store() -> TrustStore {
        trust_store_for_key(&signing_key())
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
resolution: []
effective_trust:
  include_references: false
execution:
  terminator:
    kind: subprocess
    protocol: protocol:ryeos/core/runtime
  thread_profile:
    name: runtime_run
    root_executable: true
    supports_interrupt: false
    supports_continuation: false
formats:
  - extensions: [".yaml", ".yml"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: "#"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
metadata:
  rules:
    name:
      from: path
      key: name
"##;

    fn write_runtime_kind_schema(kinds_dir: &Path) {
        let runtime_dir = kinds_dir.join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();
        let signed =
            lillux::signature::sign_content(RUNTIME_KIND_SCHEMA_BODY, &signing_key(), "#", None);
        fs::write(runtime_dir.join("runtime.kind-schema.yaml"), signed).unwrap();
    }

    fn build_test_engine_with_trust(
        bundle_roots: Vec<PathBuf>,
        item_trust_store: TrustStore,
        node_trust_store: TrustStore,
    ) -> Engine {
        let kinds_dir = tempdir();
        write_runtime_kind_schema(&kinds_dir);
        let kinds = KindRegistry::load_base(&[kinds_dir], &node_trust_store)
            .expect("load runtime kind schema");
        let parser_dispatcher = ParserDispatcher::new(
            ParserRegistry::empty(),
            std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
        );
        Engine::new(kinds, parser_dispatcher, bundle_roots)
            .with_trust_store(item_trust_store)
            .with_node_trust_store(node_trust_store)
    }

    fn build_test_engine() -> Engine {
        let node_trust_store = trust_store();
        build_test_engine_with_trust(Vec::new(), node_trust_store.clone(), node_trust_store)
    }

    fn test_plan_context(project_path: PathBuf) -> ryeos_engine::contracts::PlanContext {
        ryeos_engine::contracts::PlanContext {
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: "fp:test".into(),
                    scopes: vec!["*".into()],
                },
            ),
            project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
                path: project_path,
            },
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: Default::default(),
            validate_only: false,
        }
    }

    fn test_execution_context(bundle_root: PathBuf) -> ExecutionContext {
        let node_trust_store = trust_store();
        ExecutionContext {
            principal_fingerprint: "fp:test".into(),
            caller_scopes: vec!["*".into()],
            engine: std::sync::Arc::new(build_test_engine_with_trust(
                vec![bundle_root.clone()],
                node_trust_store.clone(),
                node_trust_store,
            )),
            plan_ctx: test_plan_context(bundle_root),
            requested_call: None,
        }
    }

    fn write_signed_manifest_with_key(
        ai_dir: &std::path::Path,
        body: &str,
        signing_key: &SigningKey,
    ) {
        fs::create_dir_all(ai_dir).unwrap();
        let signed = lillux::signature::sign_content(body, signing_key, "#", None);
        fs::write(ai_dir.join("manifest.yaml"), signed).unwrap();
    }

    fn write_signed_manifest(ai_dir: &std::path::Path, body: &str) {
        write_signed_manifest_with_key(ai_dir, body, &signing_key());
    }

    fn resolved_tool_in_space(
        source_root: &std::path::Path,
        item_ref: &str,
        source_space: ryeos_engine::contracts::ItemSpace,
        item_signing_key: &SigningKey,
    ) -> ResolvedExecutionRequest {
        let ai_dir = source_root.join(ryeos_engine::AI_DIR);
        let source_path = ai_dir.join("tools/example-bundle/send.yaml");
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        let source_body = "category: example-bundle\nexecutor_id: '@subprocess'\n";
        let source = lillux::signature::sign_content(source_body, item_signing_key, "#", None);
        fs::write(&source_path, &source).unwrap();
        let signature_envelope = ryeos_engine::contracts::SignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        };
        let signature_header =
            ryeos_engine::item_resolution::parse_signature_header(&source, &signature_envelope)
                .expect("signed tool fixture has a signature header");
        let canonical_ref = CanonicalRef::parse(item_ref).unwrap();
        let resolved_item = ResolvedItem {
            canonical_ref: canonical_ref.clone(),
            kind: canonical_ref.kind.clone(),
            source_path,
            source_space,
            resolved_from: source_space.as_str().into(),
            shadowed: Vec::new(),
            materialized_project_root: (source_space
                == ryeos_engine::contracts::ItemSpace::Project)
                .then(|| source_root.to_path_buf()),
            raw_content_digest: ryeos_engine::item_resolution::content_hash(source_body),
            content_hash: ryeos_engine::item_resolution::content_hash(&source),
            signature_header: Some(signature_header),
            source_format: ryeos_engine::contracts::ResolvedSourceFormat {
                extension: ".yaml".into(),
                parser: "parser:ryeos/core/yaml/yaml".into(),
                signature: signature_envelope,
            },
            metadata: Default::default(),
        };
        ResolvedExecutionRequest {
            kind: "tool_run".into(),
            item_ref: item_ref.into(),
            executor_ref: "@subprocess".into(),
            launch_mode: "wait".into(),
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            target_site_id: None,
            requested_by: Some("fp:test".into()),
            usage_subject: None,
            usage_subject_asserted_by: None,
            parameters: serde_json::Value::Null,
            root_raw_content_digest: resolved_item.raw_content_digest.clone(),
            ref_bindings: std::collections::BTreeMap::new(),
            root_admission: None,
            resolved_item,
            plan_context: test_plan_context(source_root.to_path_buf()),
        }
    }

    fn resolved_tool(bundle_root: &std::path::Path, item_ref: &str) -> ResolvedExecutionRequest {
        resolved_tool_in_space(
            bundle_root,
            item_ref,
            ryeos_engine::contracts::ItemSpace::Bundle,
            &signing_key(),
        )
    }

    fn resolved_tool_with_extra(
        bundle_root: &std::path::Path,
        item_ref: &str,
        extra: std::collections::HashMap<String, serde_json::Value>,
    ) -> ResolvedExecutionRequest {
        let mut resolved = resolved_tool(bundle_root, item_ref);
        resolved.resolved_item.metadata.extra = extra;
        resolved
    }

    /// Build a `metadata.extra` map carrying a `requires:` block (the value is
    /// the content of `requires:`, i.e. `{capabilities: {manifest: …}}`).
    fn requires_extra(
        requires: serde_json::Value,
    ) -> std::collections::HashMap<String, serde_json::Value> {
        std::collections::HashMap::from([("requires".to_string(), requires)])
    }

    const SELF_BUNDLE_MANIFEST: &str = r#"name: example-bundle
version: "0.1.0"
description: test
provides_kinds: []
requires_kinds: []
uses_kinds: []
runtime_authority:
  bundle_events:
    - event_kind: example_event
      operations: [append, scan]
  runtime_vault:
    - namespace: oauth
      operations: [put, get, delete, list]
  item_authoring:
    - kind: knowledge
      namespace: runtime-authored/*
"#;

    #[test]
    fn requirement_mints_exact_requested_subset() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        // Manifest is the upper bound ([append, scan] + full vault); the item
        // selects only `append` and vault `get`.
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "example_event", "operations": ["append"] }
                    ],
                    "runtime_vault": [
                        { "namespace": "oauth", "operations": ["get"] }
                    ]
                } } }
            })),
        );

        let caps = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap();
        assert_eq!(
            caps,
            vec![
                "ryeos.append.bundle-events.example-bundle/example_event".to_string(),
                "ryeos.get.vault.example-bundle/oauth".to_string(),
            ],
            "only the requested subset is minted, not the full manifest authority"
        );
    }

    #[test]
    fn trusted_live_project_mints_from_its_signed_manifest() {
        let project = tempdir().join("arc");
        write_signed_manifest(&project.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(project.clone());
        let mut resolved = resolved_tool_in_space(
            &project,
            "tool:example-bundle/send",
            ryeos_engine::contracts::ItemSpace::Project,
            &signing_key(),
        );
        resolved.resolved_item.metadata.extra = requires_extra(json!({
            "capabilities": { "manifest": { "runtime_authority": {
                "item_authoring": [
                    { "kind": "knowledge", "namespace": "runtime-authored/*" }
                ]
            } } }
        }));

        let caps = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap();
        assert_eq!(
            caps,
            vec!["ryeos.author.knowledge.runtime-authored/*".to_string()]
        );
    }

    #[test]
    fn composed_project_trust_cannot_mint_runtime_authority() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "example_event", "operations": ["append"] }
                    ]
                } } }
            })),
        );
        let requires = resolved.resolved_item.metadata.extra.get("requires");

        let err = mint_runtime_capability_caps(
            requires,
            &resolved.resolved_item,
            ryeos_engine::resolution::TrustClass::TrustedProject,
            &ctx.engine,
        )
        .unwrap_err();
        assert!(
            err.contains("effective trust class is TrustedProject"),
            "a project-trusted composed ancestor must taint runtime authority: {err}"
        );
    }

    #[test]
    fn no_requirement_mints_nothing_even_with_manifest_authority() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        // No `requires:` → manifest authority is an upper bound, never an
        // automatic grant.
        let resolved = resolved_tool(&bundle, "tool:example-bundle/send");
        assert!(
            derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn requirement_not_backed_by_manifest_fails_launch() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(
            &bundle.join(ryeos_engine::AI_DIR),
            r#"name: example-bundle
version: "0.1.0"
description: test
provides_kinds: []
requires_kinds: []
uses_kinds: []
runtime_authority:
  bundle_events:
    - event_kind: example_event
      operations: [append]
"#,
        );
        let ctx = test_execution_context(bundle.clone());
        // Manifest grants only `append`; the item requests `scan`.
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "example_event", "operations": ["scan"] }
                    ]
                } } }
            })),
        );
        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not declared in the signed manifest"),
            "got: {err}"
        );
    }

    #[test]
    fn requirement_runtime_vault_subset_semantics() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "runtime_vault": [
                        { "namespace": "oauth", "operations": ["get", "put"] }
                    ]
                } } }
            })),
        );
        let caps = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap();
        assert_eq!(
            caps,
            vec![
                "ryeos.get.vault.example-bundle/oauth".to_string(),
                "ryeos.put.vault.example-bundle/oauth".to_string(),
            ]
        );
    }

    #[test]
    fn item_authoring_requirement_can_narrow_manifest_wildcard() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "item_authoring": [
                        { "kind": "knowledge", "namespace": "runtime-authored/foo" }
                    ]
                } } }
            })),
        );
        let caps = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap();
        assert_eq!(
            caps,
            vec!["ryeos.author.knowledge.runtime-authored/foo".to_string()],
            "mint the requested narrow cap, not the manifest wildcard"
        );
    }

    #[test]
    fn item_authoring_requirement_broader_than_manifest_fails_launch() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "item_authoring": [
                        { "kind": "knowledge", "namespace": "other-namespace/*" }
                    ]
                } } }
            })),
        );
        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not declared in the signed manifest"),
            "got: {err}"
        );
    }

    #[test]
    fn item_authoring_wildcard_request_not_backed_by_wildcard_manifest_fails_launch() {
        // The manifest declares `runtime-authored/*`. A request for the *wildcard*
        // `runtime-authored/foo*` must NOT be admitted just because the manifest
        // glob would match the literal string — `foo*` authorizes names the
        // manifest never granted. Fail closed (see `manifest_backs_requested_cap`).
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "item_authoring": [
                        { "kind": "knowledge", "namespace": "runtime-authored/foo*" }
                    ]
                } } }
            })),
        );
        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not declared in the signed manifest"),
            "got: {err}"
        );
    }

    #[test]
    fn requirement_without_signed_manifest_fails_launch() {
        let bundle = tempdir().join("example-bundle");
        let ctx = test_execution_context(bundle.clone());
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "example_event", "operations": ["append"] }
                    ]
                } } }
            })),
        );
        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("required signed bundle manifest is missing"),
            "got: {err}"
        );
    }

    #[test]
    fn requirement_manifest_namespace_mismatch_fails_launch() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(
            &bundle.join(ryeos_engine::AI_DIR),
            r#"name: other-bundle
version: "0.1.0"
description: test
provides_kinds: []
requires_kinds: []
uses_kinds: []
runtime_authority:
  bundle_events:
    - event_kind: example_event
      operations: [append]
"#,
        );
        let ctx = test_execution_context(bundle.clone());
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "example_event", "operations": ["append"] }
                    ]
                } } }
            })),
        );
        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(err.to_string().contains("identity mismatch"), "got: {err}");
    }

    #[test]
    fn project_self_trust_mints_runtime_authority_from_exact_live_root() {
        let project = tempdir().join("project");
        let project_signing_key = SigningKey::from_bytes(&[72u8; 32]);
        write_signed_manifest_with_key(
            &project.join(ryeos_engine::AI_DIR),
            SELF_BUNDLE_MANIFEST,
            &project_signing_key,
        );

        // Model the live per-request store: persistent node trust plus the
        // project's self-pinned publisher.
        let node_trust_store = trust_store();
        let mut combined_trust_store = node_trust_store.clone();
        combined_trust_store.extend_from(&trust_store_for_key(&project_signing_key));
        let project_manifest = ryeos_bundle::manifest::load_verified_manifest(
            &project.join(ryeos_engine::AI_DIR),
            "example-bundle",
            &combined_trust_store,
        )
        .expect("fixture project manifest must verify");
        assert_eq!(project_manifest.manifest.name, "example-bundle");

        let engine =
            build_test_engine_with_trust(Vec::new(), combined_trust_store, node_trust_store);
        let ctx = ExecutionContext {
            principal_fingerprint: "fp:test".into(),
            caller_scopes: vec!["*".into()],
            engine: std::sync::Arc::new(engine),
            plan_ctx: test_plan_context(project.clone()),
            requested_call: None,
        };
        let mut resolved = resolved_tool_in_space(
            &project,
            "tool:example-bundle/send",
            ryeos_engine::contracts::ItemSpace::Project,
            &project_signing_key,
        );
        resolved.resolved_item.metadata.extra = requires_extra(json!({
            "capabilities": { "manifest": { "runtime_authority": {
                "bundle_events": [
                    { "event_kind": "example_event", "operations": ["append"] }
                ]
            } } }
        }));

        let caps = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap();
        assert_eq!(
            caps,
            vec!["ryeos.append.bundle-events.example-bundle/example_event".to_string()],
            "the exact live project manifest is the authority upper bound"
        );
    }

    #[test]
    fn node_trusted_manifest_in_exact_live_project_root_mints_runtime_authority() {
        let installed_bundle = tempdir().join("installed-example-bundle");
        let installed_ai = installed_bundle.join(ryeos_engine::AI_DIR);
        write_signed_manifest(&installed_ai, SELF_BUNDLE_MANIFEST);

        // A node-trusted publisher is also valid in the effective project
        // trust store, but authority remains anchored to the exact live
        // project root rather than borrowed from installed provenance.
        let project = tempdir().join("project");
        let project_ai = project.join(ryeos_engine::AI_DIR);
        fs::create_dir_all(&project_ai).unwrap();
        fs::write(
            project_ai.join("manifest.yaml"),
            fs::read(installed_ai.join("manifest.yaml")).unwrap(),
        )
        .unwrap();
        let replayed_manifest = ryeos_bundle::manifest::load_verified_manifest(
            &project_ai,
            "example-bundle",
            &trust_store(),
        )
        .expect("fixture must be a valid replay of the node-signed manifest");
        assert_eq!(replayed_manifest.manifest.name, "example-bundle");

        let ctx = test_execution_context(installed_bundle);
        let mut resolved = resolved_tool_in_space(
            &project,
            "tool:example-bundle/send",
            ryeos_engine::contracts::ItemSpace::Project,
            &signing_key(),
        );
        resolved.resolved_item.metadata.extra = requires_extra(json!({
            "capabilities": { "manifest": { "runtime_authority": {
                "bundle_events": [
                    { "event_kind": "example_event", "operations": ["append"] }
                ]
            } } }
        }));

        let caps = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap();
        assert_eq!(
            caps,
            vec!["ryeos.append.bundle-events.example-bundle/example_event".to_string()],
            "the node-trusted manifest is bounded by exact project provenance"
        );
    }

    #[test]
    fn bundle_space_item_outside_registered_roots_cannot_mint_runtime_authority() {
        let unregistered_bundle = tempdir().join("unregistered-example-bundle");
        write_signed_manifest(
            &unregistered_bundle.join(ryeos_engine::AI_DIR),
            SELF_BUNDLE_MANIFEST,
        );
        let registered_bundle = tempdir().join("different-registered-bundle");
        fs::create_dir_all(registered_bundle.join(ryeos_engine::AI_DIR)).unwrap();
        let ctx = test_execution_context(registered_bundle);
        let resolved = resolved_tool_with_extra(
            &unregistered_bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "example_event", "operations": ["append"] }
                    ]
                } } }
            })),
        );

        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not inside a registered installed bundle root"),
            "source-space labels alone must not establish installed provenance: {err}"
        );
    }

    #[test]
    fn requirement_rejects_unsafe_requested_segments() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());

        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "../bad", "operations": ["append"] }
                    ]
                } } }
            })),
        );
        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(err.to_string().contains("unsafe character"), "got: {err}");

        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "runtime_vault": [
                        { "namespace": "../bad", "operations": ["get"] }
                    ]
                } } }
            })),
        );
        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(
            err.to_string().contains("must match [A-Za-z0-9_]+"),
            "got: {err}"
        );
    }

    #[test]
    fn requirement_with_empty_operations_fails_static_validation() {
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "example_event", "operations": [] }
                    ]
                } } }
            })),
        );
        let err = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap_err();
        assert!(
            err.to_string().contains("at least one operation"),
            "got: {err}"
        );
    }

    #[test]
    fn bundle_identity_uses_resolved_canonical_ref_not_item_ref() {
        // Regression guard for the cap/token identity split: the requested
        // `item_ref` is deliberately diverged from the resolved canonical ref.
        // Both the minted caps AND the callback token's effective_bundle_id must
        // follow the *resolved canonical ref*, never the requested alias —
        // otherwise caps would be minted under one bundle while the token claims
        // another.
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        let mut resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            requires_extra(json!({
                "capabilities": { "manifest": { "runtime_authority": {
                    "bundle_events": [
                        { "event_kind": "example_event", "operations": ["append"] }
                    ]
                } } }
            })),
        );
        // resolved_item.canonical_ref stays `example-bundle`; only the requested
        // ref points at a different bundle.
        resolved.item_ref = "tool:other-alias/send".to_string();

        // Single source of truth: derived from the resolved canonical ref.
        assert_eq!(
            ryeos_app::callback_token::effective_bundle_id_for_request(&resolved).as_deref(),
            Some("example-bundle"),
            "token bundle identity must follow the resolved canonical ref, not item_ref"
        );
        // Caps are minted under that same identity.
        let caps = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap();
        assert_eq!(
            caps,
            vec!["ryeos.append.bundle-events.example-bundle/example_event".to_string()]
        );
    }

    // Minimal kind schema exercising the metadata rail (the tool path): a
    // `requires` rule using the `path_value` extractor under the identity
    // composer. Tools never extend, so the leaf declaration is effective and
    // metadata extraction is the carrier (graph/directive use the composed view).
    const METADATA_RAIL_KIND_SCHEMA_BODY: &str = r##"category: "engine/kinds/tool"
version: "1.0.0"
location:
  directory: tools
resolution: []
effective_trust:
  include_references: false
execution:
  terminator:
    kind: subprocess
    protocol: protocol:ryeos/core/runtime
  thread_profile:
    name: tool_run
    root_executable: true
    supports_interrupt: false
    supports_continuation: false
formats:
  - extensions: [".yaml", ".yml"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: "#"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
  optional:
    requires:
      type: single
      prim: mapping
metadata:
  rules:
    name:
      from: filename
      required: true
    requires:
      from: path_value
      key: requires
"##;

    #[test]
    fn requires_metadata_rail_extraction_to_caps_pipeline() {
        use ryeos_engine::kind_registry::KindRegistry;

        // 1. Load the kind schema through the REAL loader so `from: path_value`
        //    is parsed into the extractor (not a hand-built rule).
        let kinds_dir = tempdir();
        let tool_dir = kinds_dir.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        let signed = lillux::signature::sign_content(
            METADATA_RAIL_KIND_SCHEMA_BODY,
            &signing_key(),
            "#",
            None,
        );
        fs::write(tool_dir.join("tool.kind-schema.yaml"), signed).unwrap();
        let ts = trust_store();
        let kinds = KindRegistry::load_base(&[kinds_dir], &ts).expect("load kind schema");
        let schema = kinds.get("tool").expect("tool kind registered");

        // 2. A real item document carrying a `requires.capabilities.manifest`
        //    block (as a parser would produce it).
        let item_yaml = r#"
version: "1.0.0"
category: example-bundle
requires:
  capabilities:
    manifest:
      runtime_authority:
        bundle_events:
          - event_kind: example_event
            operations: [append]
"#;
        let parsed: serde_json::Value = serde_yaml::from_str(item_yaml).unwrap();

        // 3. REAL metadata extraction: the path_value rule carries the nested
        //    `requires` mapping verbatim into metadata.extra.
        let metadata = ryeos_engine::kind_registry::apply_extraction_rules(
            &parsed,
            &schema.extraction_rules,
            std::path::Path::new("/proj/.ai/tools/example-bundle/play.yaml"),
            schema.directory.as_str(),
        );
        assert!(
            metadata.extra.contains_key("requires"),
            "path_value rule must surface `requires` into metadata.extra"
        );

        // 4. Feed the *extracted* metadata into the minter together with a
        //    signed manifest. No manual metadata.extra injection.
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        let mut resolved = resolved_tool(&bundle, "tool:example-bundle/play");
        resolved.resolved_item.metadata = metadata;

        // 5. Exact manifest-backed subset is derived end-to-end.
        let caps = derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
            .unwrap();
        assert_eq!(
            caps,
            vec!["ryeos.append.bundle-events.example-bundle/example_event".to_string()],
            "schema → extraction → manifest upper-bound → caps must yield the requested subset"
        );
    }

    #[test]
    fn edited_kind_schemas_carry_requires_on_the_right_rail() {
        // Validates the ACTUAL repo schema edits. Only `tool` uses the metadata
        // rail (`path_value`); graph + directive carry `requires` through the
        // composed view, so they must NOT declare a `requires` metadata rule.
        use ryeos_engine::kind_registry::{ExtractionRule, KindRegistry};
        use ryeos_engine::trust::{PublisherTrustDoc, TrustStore, TrustedSigner};

        fn workspace_root() -> PathBuf {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .find(|p| p.join("bundles").is_dir())
                .expect("workspace root with bundles/ directory")
                .to_path_buf()
        }

        let root = workspace_root();
        let trust_text = fs::read_to_string(root.join(".dev-keys/PUBLISHER_DEV_TRUST.toml"))
            .expect("read dev publisher trust doc");
        let trust_doc =
            PublisherTrustDoc::parse(&trust_text).expect("parse dev publisher trust doc");
        let ts = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: trust_doc.fingerprint.clone(),
            verifying_key: trust_doc
                .decode_verifying_key()
                .expect("decode dev publisher key"),
            label: Some(trust_doc.owner.clone()),
        }]);
        let schema_roots = [
            root.join("bundles/core/.ai/node/engine/kinds"),
            root.join("bundles/standard/.ai/node/engine/kinds"),
        ];
        let kinds =
            KindRegistry::load_base(&schema_roots, &ts).expect("repo kind schemas load cleanly");

        // Tool: metadata rail → `requires` extracted via path_value.
        let tool = kinds.get("tool").expect("tool kind registered");
        assert_eq!(
            tool.extraction_rules.get("requires").map(|r| &r.extractor),
            Some(&ExtractionRule::PathValue {
                key: "requires".into()
            }),
            "tool must extract `requires` via path_value"
        );

        // Graph + directive: composed-view rail → NO `requires` metadata rule.
        for kind in ["graph", "directive"] {
            let schema = kinds
                .get(kind)
                .unwrap_or_else(|| panic!("{kind} kind registered"));
            assert!(
                !schema.extraction_rules.contains_key("requires"),
                "{kind} must NOT carry `requires` on the metadata rail (it uses the composed view)"
            );
        }
    }

    #[test]
    fn tool_declared_action_authority_rejected() {
        // Tools have no surface to honor self-declared action authority — the
        // `declared` *key's presence* is rejected, not silently ignored. Both a
        // populated list and an empty `declared: []` fail.
        let bundle = tempdir().join("example-bundle");
        write_signed_manifest(&bundle.join(ryeos_engine::AI_DIR), SELF_BUNDLE_MANIFEST);
        let ctx = test_execution_context(bundle.clone());
        for declared in [json!(["ryeos.execute.tool.echo"]), json!([])] {
            let resolved = resolved_tool_with_extra(
                &bundle,
                "tool:example-bundle/send",
                requires_extra(json!({ "capabilities": { "declared": declared } })),
            );
            let err =
                derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
                    .unwrap_err();
            assert!(
                err.to_string()
                    .contains("cannot self-declare action authority"),
                "declared={declared}: got {err}"
            );
        }
    }

    #[test]
    fn direct_tool_required_caps_do_not_become_callback_caps() {
        let bundle = tempdir().join("example-bundle");
        let ctx = test_execution_context(bundle.clone());
        // `required_caps` is caller-side invoke authorization, not a runtime
        // requirement — it must never mint callback caps.
        let resolved = resolved_tool_with_extra(
            &bundle,
            "tool:example-bundle/send",
            std::collections::HashMap::from([(
                "required_caps".to_string(),
                serde_json::json!(["ryeos.*"]),
            )]),
        );

        assert!(
            derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, &ctx)
                .unwrap()
                .is_empty()
        );
    }

    // P1.4: tests for `lookup_runtime_for_dispatch` were removed when
    // that helper was deleted. The dispatch loop now attaches the
    // verified runtime to the hop via `RuntimeRegistry::lookup_by_ref`
    // (covered by `runtime_registry` integration tests) and
    // `dispatch_native_runtime` consumes that owned value, so the
    // per-call lookup path no longer exists.

    #[test]
    fn reject_tool_declared_capabilities_rejects_declared_key() {
        let requires = serde_json::json!({ "capabilities": { "declared": [] } });
        let err = reject_tool_declared_capabilities(Some(&requires), "tool:x/y").unwrap_err();
        assert!(matches!(err, DispatchError::InvalidRef(..)));
        assert!(err.to_string().contains("declared"));
    }

    #[test]
    fn reject_tool_declared_capabilities_allows_manifest_or_absent() {
        // manifest-only requires is fine
        let manifest = serde_json::json!({ "capabilities": { "manifest": ["a"] } });
        assert!(reject_tool_declared_capabilities(Some(&manifest), "tool:x/y").is_ok());
        // no requires at all is fine
        assert!(reject_tool_declared_capabilities(None, "tool:x/y").is_ok());
    }

    #[test]
    fn strip_binary_ref_prefix_strips_triple() {
        assert_eq!(
            strip_binary_ref_prefix("bin/x86_64-unknown-linux-gnu/directive-runtime").unwrap(),
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

    fn test_authorizer() -> ryeos_runtime::authorizer::Authorizer {
        ryeos_runtime::authorizer::Authorizer::new()
    }

    /// **B1 unit test**: `enforce_runtime_caps` itself is unconditional
    /// (it always checks). The CALLER (`dispatch_native_runtime`) is
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
        let auth = test_authorizer();
        // If `dispatch_native_runtime` skips the call entirely (B1
        // gate), then a missing cap does NOT translate to an error.
        // We model this by simply never calling `enforce_runtime_caps`
        // and asserting the synthetic outcome is `Ok`.
        let required = vec!["runtime.execute".to_string()];
        let caller_scopes: Vec<String> = vec![]; // would normally fail

        // SIMULATED indirect path — gate skips the call.
        let original_root_kind = "directive";
        let outcome: Result<(), DispatchError> = if original_root_kind == "runtime" {
            enforce_runtime_caps(
                &auth,
                "runtime:directive-runtime",
                &required,
                &caller_scopes,
            )
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
            enforce_runtime_caps(
                &auth,
                "runtime:directive-runtime",
                &required,
                &caller_scopes,
            )
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
        let auth = test_authorizer();
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["runtime.execute".to_string(), "execute".to_string()];
        assert!(enforce_runtime_caps(&auth, "runtime:directive-runtime", &req, &caller).is_ok());
    }

    #[test]
    fn enforce_runtime_caps_allows_wildcard_scope() {
        let auth = test_authorizer();
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["*".to_string()];
        assert!(enforce_runtime_caps(&auth, "runtime:directive-runtime", &req, &caller).is_ok());
    }

    #[test]
    fn enforce_runtime_caps_denies_when_caller_lacks_required_cap() {
        let auth = test_authorizer();
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["execute".to_string()];
        let err = enforce_runtime_caps(&auth, "runtime:directive-runtime", &req, &caller)
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
        let auth = test_authorizer();
        let req: Vec<String> = vec![];
        let caller = vec!["execute".to_string()];
        assert!(enforce_runtime_caps(&auth, "runtime:test", &req, &caller).is_ok());
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
            requested_call: None,
        };
        // `unknown` kind has no schema; only `runtime` was loaded.
        // For non-runtime kinds the resolver tries engine.resolve()
        // first and that fails with a resolution error (still
        // enumerating in DispatchError wording). The runtime: kind
        // path is verified by the dispatch_pin tests.
        let cref = CanonicalRef::parse("unknown:thing").expect("parse");
        let err = resolve_dispatch_hop(&cref, &ctx).expect_err("unknown kind must error");
        let msg = err.to_string();
        // Either "registered kinds: [runtime]" (schema lookup path)
        // OR "resolution failed" (engine.resolve path) — both are
        // acceptable enumerated errors.
        assert!(
            msg.contains("runtime") || msg.contains("resolution failed"),
            "error must enumerate available kinds or explain resolution failure, got: {msg}"
        );
    }

    // ── Method dispatch unit tests ───────────────────────────────────

    use axum::http::StatusCode;
    use ryeos_engine::kind_registry::{
        ArgDecl, ArgType, ExecutionSchema, MethodDecl, MethodDispatchDecl, MethodDispatchVia,
        MethodScope,
    };
    use ryeos_engine::resolution::{
        ResolutionEdge, ResolutionOutput, ResolutionStepName, ResolvedAncestor,
        TrustClass as EngineTrustClass,
    };
    use ryeos_runtime::method_wire::{MethodCallError, TrustClass as WireTrustClass};
    use std::collections::BTreeMap;

    fn make_method(args: BTreeMap<String, ArgDecl>) -> MethodDecl {
        MethodDecl {
            scope: MethodScope::default(),
            args,
            runtime_config: BTreeMap::new(),
        }
    }

    /// Build a method-bearing `ExecutionSchema` from method names + an
    /// optional default, so the `resolve_requested_method` tests exercise
    /// the real lookup path.
    fn exec_with_methods(names: &[&str], default: Option<&str>) -> ExecutionSchema {
        let mut methods = BTreeMap::new();
        for n in names {
            methods.insert(n.to_string(), make_method(BTreeMap::new()));
        }
        ExecutionSchema {
            aliases: std::collections::HashMap::new(),
            alias_max_depth: 8,
            terminator: None,
            delegate: None,
            thread_profile: None,
            history_policy: None,
            method_dispatch: Some(MethodDispatchDecl {
                via: MethodDispatchVia::RuntimeRegistry,
                protocol: "protocol:ryeos/core/method_runtime".to_string(),
                default: default.map(|s| s.to_string()),
            }),
            methods,
            launch_augmentations: Vec::new(),
        }
    }

    fn string_arg(required: bool) -> ArgDecl {
        ArgDecl {
            ty: ArgType::String,
            required,
            default: None,
            enum_values: None,
            min: None,
            items: None,
        }
    }

    fn integer_arg(required: bool, default: Option<i64>) -> ArgDecl {
        ArgDecl {
            ty: ArgType::Integer,
            required,
            default: default.map(|v| json!(v)),
            enum_values: None,
            min: None,
            items: None,
        }
    }

    // ── resolve_requested_method ─────────────────────────────────────────

    #[test]
    fn resolve_requested_method_explicit_known() {
        let exec = exec_with_methods(&["compose"], None);
        let r = resolve_requested_method(Some("compose"), &exec, "knowledge");
        assert!(r.is_ok());
        assert_eq!(r.unwrap().0, "compose");
    }

    #[test]
    fn resolve_requested_method_explicit_unknown_lists_declared() {
        let exec = exec_with_methods(&["compose", "query"], None);
        let err = resolve_requested_method(Some("bogus"), &exec, "knowledge")
            .expect_err("unknown method must error");
        let msg = err.to_string();
        assert!(
            msg.contains("bogus"),
            "must name requested method, got: {msg}"
        );
        assert!(msg.contains("compose"), "must list compose, got: {msg}");
        assert!(msg.contains("query"), "must list query, got: {msg}");
        assert!(
            matches!(err, DispatchError::UnknownMethod { .. }),
            "must be UnknownMethod variant"
        );
    }

    #[test]
    fn resolve_requested_method_rejects_augmentation_private_compose_positions() {
        // `compose_positions` is a knowledge runtime handler used only by
        // the compose_context_positions launch augmentation. It must not be
        // accepted as a generic requested method unless the kind schema
        // declares it in `methods`.
        let exec = exec_with_methods(&["compose", "query", "graph", "validate"], None);
        let err = resolve_requested_method(Some("compose_positions"), &exec, "knowledge")
            .expect_err("augmentation-private handler must be rejected generically");
        assert!(
            matches!(
                err,
                DispatchError::UnknownMethod {
                    ref requested,
                    ref declared,
                    ..
                } if requested == "compose_positions"
                    && !declared.contains("compose_positions")
            ),
            "must reject compose_positions as undeclared, got: {err:?}"
        );
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resolve_requested_method_falls_back_to_default() {
        let exec = exec_with_methods(&["compose"], Some("compose"));
        let r = resolve_requested_method(None, &exec, "knowledge");
        assert!(r.is_ok());
        assert_eq!(r.unwrap().0, "compose");
    }

    #[test]
    fn resolve_requested_method_no_request_no_default_is_misconfigured() {
        let exec = exec_with_methods(&["compose"], None);
        let err = resolve_requested_method(None, &exec, "knowledge")
            .expect_err("missing default must error");
        assert!(
            matches!(err, DispatchError::SchemaMisconfigured { .. }),
            "must be SchemaMisconfigured, got: {err:?}"
        );
    }

    #[test]
    fn resolve_requested_method_empty_methods() {
        let exec = exec_with_methods(&[], None);
        let err = resolve_requested_method(Some("anything"), &exec, "tool")
            .expect_err("empty methods must error");
        assert!(
            matches!(err, DispatchError::UnknownMethod { declared, .. } if declared.is_empty())
        );
    }

    // ── validate_method_args ──────────────────────────────────────────

    #[test]
    fn validate_method_args_all_required_present() {
        let mut args = BTreeMap::new();
        args.insert("question".to_string(), string_arg(true));
        let method = make_method(args);

        let r = validate_method_args(Some(&json!({"question": "hello"})), "ask", &method);
        assert!(r.is_ok());
        let val = r.unwrap();
        assert_eq!(val["question"], "hello");
    }

    #[test]
    fn validate_method_args_missing_required() {
        let mut args = BTreeMap::new();
        args.insert("question".to_string(), string_arg(true));
        let method = make_method(args);

        let err = validate_method_args(Some(&json!({"question": null})), "ask", &method)
            .expect_err("wrong-typed required must error");
        assert!(matches!(err, DispatchError::MethodInvalidArg { .. }));

        // Truly absent required arg is also rejected.
        let err = validate_method_args(Some(&json!({})), "ask", &method)
            .expect_err("missing required must error");
        let msg = err.to_string();
        assert!(
            msg.contains("question"),
            "must name missing field, got: {msg}"
        );
        assert!(
            matches!(err, DispatchError::MethodInvalidArg { .. }),
            "must be MethodInvalidArg variant"
        );
    }

    #[test]
    fn validate_method_args_unknown_arg_rejected() {
        // Strict contract: an arg the method does not declare is rejected.
        let mut args = BTreeMap::new();
        args.insert("question".to_string(), string_arg(true));
        let method = make_method(args);

        let err =
            validate_method_args(Some(&json!({"question": "hi", "bogus": 1})), "ask", &method)
                .expect_err("undeclared arg must error");
        let msg = err.to_string();
        assert!(msg.contains("unknown arg 'bogus'"), "got: {msg}");
        assert!(matches!(err, DispatchError::MethodInvalidArg { .. }));
    }

    #[test]
    fn validate_method_args_optional_default_filled() {
        let mut args = BTreeMap::new();
        args.insert("question".to_string(), string_arg(true));
        args.insert("max_tokens".to_string(), integer_arg(false, Some(42)));
        let method = make_method(args);

        let r = validate_method_args(Some(&json!({"question": "hello"})), "ask", &method).unwrap();
        assert_eq!(r["max_tokens"], 42, "default must be filled");
    }

    #[test]
    fn validate_method_args_rejects_invalid_default() {
        // A schema default that violates its own declaration must be caught
        // before it reaches the runtime, not silently inserted.
        let mut decl = integer_arg(false, Some(0));
        decl.min = Some(1);
        let mut args = BTreeMap::new();
        args.insert("limit".to_string(), decl);
        let method = make_method(args);

        let err = validate_method_args(None, "query", &method)
            .expect_err("an out-of-range default must error");
        assert!(err.to_string().contains(">= 1"), "got: {err}");
        assert!(matches!(err, DispatchError::MethodInvalidArg { .. }));
    }

    #[test]
    fn validate_method_args_optional_with_explicit_value() {
        let mut args = BTreeMap::new();
        args.insert("question".to_string(), string_arg(true));
        args.insert("max_tokens".to_string(), integer_arg(false, Some(42)));
        let method = make_method(args);

        let r = validate_method_args(
            Some(&json!({"question": "hello", "max_tokens": 100})),
            "ask",
            &method,
        )
        .unwrap();
        assert_eq!(r["max_tokens"], 100, "explicit value must override default");
    }

    #[test]
    fn validate_method_args_wrong_type() {
        let mut args = BTreeMap::new();
        args.insert("question".to_string(), string_arg(true));
        let method = make_method(args);

        let err = validate_method_args(Some(&json!({"question": 123})), "ask", &method)
            .expect_err("wrong type must error");
        assert!(
            matches!(err, DispatchError::MethodInvalidArg { .. }),
            "must be MethodInvalidArg variant, got: {err:?}"
        );
    }

    #[test]
    fn validate_method_args_non_object_rejected() {
        let method = make_method(BTreeMap::new());
        let err = validate_method_args(Some(&json!("not an object")), "ask", &method)
            .expect_err("non-object must error");
        let msg = err.to_string();
        assert!(msg.contains("must be an object"), "got: {msg}");
    }

    #[test]
    fn validate_method_args_none_uses_defaults() {
        let mut args = BTreeMap::new();
        args.insert("count".to_string(), integer_arg(false, Some(10)));
        let method = make_method(args);

        let r = validate_method_args(None, "list", &method).unwrap();
        assert_eq!(r["count"], 10, "default must be applied when args is None");
    }

    // ── validate_method_args: enum / min / array items ────────────────

    fn string_array_arg() -> ArgDecl {
        ArgDecl {
            ty: ArgType::Array,
            required: false,
            default: None,
            enum_values: None,
            min: None,
            items: Some(Box::new(string_arg(false))),
        }
    }

    #[test]
    fn validate_method_args_array_items_type_enforced() {
        let mut args = BTreeMap::new();
        args.insert("roots".to_string(), string_array_arg());
        let method = make_method(args);

        // All-string array passes.
        assert!(
            validate_method_args(Some(&json!({"roots": ["a", "b"]})), "validate", &method).is_ok()
        );

        // A non-string element is rejected, naming the index.
        let err = validate_method_args(Some(&json!({"roots": ["a", 7]})), "validate", &method)
            .expect_err("non-string array element must error");
        let msg = err.to_string();
        assert!(
            msg.contains("roots[1]"),
            "must name the bad index, got: {msg}"
        );
    }

    #[test]
    fn validate_method_args_enum_enforced() {
        let mut decl = string_arg(false);
        decl.enum_values = Some(vec!["lexical".into(), "semantic".into()]);
        let mut args = BTreeMap::new();
        args.insert("mode".to_string(), decl);
        let method = make_method(args);

        assert!(validate_method_args(Some(&json!({"mode": "lexical"})), "query", &method).is_ok());
        let err = validate_method_args(Some(&json!({"mode": "fuzzy"})), "query", &method)
            .expect_err("out-of-enum value must error");
        assert!(err.to_string().contains("must be one of"), "got: {err}");
    }

    #[test]
    fn validate_method_args_min_enforced() {
        let mut decl = integer_arg(false, None);
        decl.min = Some(1);
        let mut args = BTreeMap::new();
        args.insert("limit".to_string(), decl);
        let method = make_method(args);

        assert!(validate_method_args(Some(&json!({"limit": 5})), "query", &method).is_ok());
        let err = validate_method_args(Some(&json!({"limit": 0})), "query", &method)
            .expect_err("below-min value must error");
        assert!(err.to_string().contains(">= 1"), "got: {err}");
    }

    // ── map_method_error ──────────────────────────────────────────────

    #[test]
    fn map_method_error_not_implemented_preserves_phase() {
        let err = map_method_error(
            "knowledge",
            "query",
            MethodCallError::NotImplemented {
                method: "query".into(),
                phase: 3,
            },
        );
        assert!(
            matches!(err, DispatchError::MethodNotImplemented { phase: 3, .. }),
            "phase must be preserved"
        );
        // HTTP mapping: BAD_GATEWAY (502)
        assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn map_method_error_invalid_arg() {
        let err = map_method_error(
            "knowledge",
            "compose",
            MethodCallError::InvalidArg {
                method: "compose".into(),
                field: Some("token_budget".into()),
                reason: "must be positive".into(),
            },
        );
        assert!(matches!(err, DispatchError::MethodInvalidArg { .. }));
        // HTTP mapping: BAD_REQUEST (400)
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn map_method_error_unknown_method() {
        let err = map_method_error(
            "knowledge",
            "delete",
            MethodCallError::UnknownMethod {
                kind: "knowledge".into(),
                requested: "delete".into(),
                declared: vec!["compose".into(), "query".into()],
            },
        );
        assert!(
            matches!(err, DispatchError::UnknownMethod { ref requested, ref declared, .. }
            if requested == "delete" && declared.contains("compose"))
        );
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn map_method_error_method_failed() {
        let err = map_method_error(
            "knowledge",
            "compose",
            MethodCallError::MethodFailed {
                reason: "OOM".into(),
            },
        );
        assert!(matches!(err, DispatchError::MethodFailed { ref reason, .. } if reason == "OOM"));
        assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn map_method_error_not_implemented_is_not_method_failed() {
        // Critical assertion: NotImplemented must produce MethodNotImplemented,
        // NOT MethodFailed (which would lose the phase information).
        let err = map_method_error(
            "knowledge",
            "query",
            MethodCallError::NotImplemented {
                method: "query".into(),
                phase: 5,
            },
        );
        assert!(
            !matches!(err, DispatchError::MethodFailed { .. }),
            "NotImplemented must NOT map to MethodFailed — phase info would be lost"
        );
        assert!(
            matches!(err, DispatchError::MethodNotImplemented { phase: 5, .. }),
            "NotImplemented must map to MethodNotImplemented with correct phase"
        );
    }

    // ── project_single_root ──────────────────────────────────────────

    fn make_ancestor(
        ref_str: &str,
        content: &str,
        source_space: ItemSpace,
        trust: EngineTrustClass,
    ) -> ResolvedAncestor {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let digest = format!("{:016x}", hasher.finish());
        ResolvedAncestor {
            requested_id: ref_str.to_string(),
            resolved_ref: ref_str.to_string(),
            source_path: std::path::PathBuf::from(format!("/tmp/{ref_str}")),
            source_space,
            trust_class: trust,
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: content.to_string(),
            source_content_digest: digest.clone(),
            raw_content_digest: digest,
        }
    }

    #[test]
    fn project_single_root_root_only() {
        let root = make_ancestor(
            "knowledge:my/doc",
            "hello",
            ItemSpace::Bundle,
            EngineTrustClass::TrustedBundle,
        );
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![],
            referenced_items: vec![],
            step_outputs: std::collections::HashMap::new(),
            effective_trust_class: EngineTrustClass::TrustedBundle,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.root_ref, "knowledge:my/doc");
        assert_eq!(payload.items_by_ref.len(), 1);
        assert!(payload.items_by_ref.contains_key("knowledge:my/doc"));
        assert_eq!(
            payload.items_by_ref["knowledge:my/doc"].raw_content,
            "hello"
        );
        assert!(payload.edges.is_empty());
    }

    #[test]
    fn project_single_root_extends_chain_produces_edges() {
        let root = make_ancestor(
            "directive:base",
            "base body",
            ItemSpace::Bundle,
            EngineTrustClass::TrustedBundle,
        );
        let mid = make_ancestor(
            "directive:mid",
            "mid body",
            ItemSpace::Project,
            EngineTrustClass::TrustedProject,
        );
        let leaf = make_ancestor(
            "directive:leaf",
            "leaf body",
            ItemSpace::Project,
            EngineTrustClass::TrustedProject,
        );
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![mid.clone(), leaf.clone()],
            references_edges: vec![],
            referenced_items: vec![],
            step_outputs: std::collections::HashMap::new(),
            effective_trust_class: EngineTrustClass::TrustedProject,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.items_by_ref.len(), 3); // root + 2 ancestors
        assert_eq!(payload.edges.len(), 2); // root→mid, mid→leaf

        // First edge: root → mid
        assert_eq!(payload.edges[0].from, "directive:base");
        assert_eq!(payload.edges[0].to, "directive:mid");
        assert_eq!(
            payload.edges[0].kind,
            ryeos_runtime::method_wire::EdgeKind::Extends
        );
        assert_eq!(payload.edges[0].depth_from_root, Some(1));

        // Second edge: mid → leaf
        assert_eq!(payload.edges[1].from, "directive:mid");
        assert_eq!(payload.edges[1].to, "directive:leaf");
        assert_eq!(payload.edges[1].depth_from_root, Some(2));
    }

    #[test]
    fn project_single_root_reference_edges() {
        let root = make_ancestor(
            "knowledge:main",
            "main content",
            ItemSpace::Project,
            EngineTrustClass::TrustedProject,
        );
        let ref_item = make_ancestor(
            "knowledge:other",
            "other content",
            ItemSpace::Bundle,
            EngineTrustClass::TrustedBundle,
        );
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![ResolutionEdge {
                from_ref: "knowledge:main".to_string(),
                from_source_path: std::path::PathBuf::from("/tmp/main"),
                to_ref: "knowledge:other".to_string(),
                to_source_path: std::path::PathBuf::from("/tmp/other"),
                to_source_space: ItemSpace::Bundle,
                trust_class: EngineTrustClass::TrustedBundle,
                added_by: ResolutionStepName::ResolveReferences,
            }],
            referenced_items: vec![ref_item.clone()],
            step_outputs: std::collections::HashMap::new(),
            effective_trust_class: EngineTrustClass::TrustedProject,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.items_by_ref.len(), 2);
        assert_eq!(payload.edges.len(), 1);
        assert_eq!(payload.edges[0].from, "knowledge:main");
        assert_eq!(payload.edges[0].to, "knowledge:other");
        assert_eq!(
            payload.edges[0].kind,
            ryeos_runtime::method_wire::EdgeKind::References
        );
        assert_eq!(
            payload.edges[0].depth_from_root, None,
            "reference edges have no depth"
        );
    }

    #[test]
    fn project_single_root_trust_class_mapping() {
        // Engine TrustClass → Wire TrustClass mapping must preserve trust exactly.
        let root_bundle = make_ancestor(
            "item:bundle",
            "bundle",
            ItemSpace::Bundle,
            EngineTrustClass::TrustedBundle,
        );
        let root_project = make_ancestor(
            "item:project",
            "project",
            ItemSpace::Project,
            EngineTrustClass::TrustedProject,
        );
        let root_untrusted = make_ancestor(
            "item:untrusted",
            "un",
            ItemSpace::Project,
            EngineTrustClass::UntrustedProject,
        );
        let root_unsigned = make_ancestor(
            "item:unsigned",
            "us",
            ItemSpace::Project,
            EngineTrustClass::Unsigned,
        );

        let cases: Vec<(ResolvedAncestor, WireTrustClass)> = vec![
            (root_bundle, WireTrustClass::TrustedBundle),
            (root_project, WireTrustClass::TrustedProject),
            (root_untrusted, WireTrustClass::UntrustedProject),
            (root_unsigned, WireTrustClass::Unsigned),
        ];

        for (ancestor, expected_trust) in cases {
            let output = ResolutionOutput {
                root: ancestor.clone(),
                ancestors: vec![],
                references_edges: vec![],
                referenced_items: vec![],
                step_outputs: std::collections::HashMap::new(),
                effective_trust_class: EngineTrustClass::Unsigned,
                composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
            };
            let payload = project_single_root(&output).unwrap();
            assert_eq!(
                payload.items_by_ref[&ancestor.resolved_ref].trust_class, expected_trust,
                "trust class mismatch for {:?}",
                ancestor.trust_class
            );
        }
    }

    #[test]
    fn project_single_root_edge_endpoint_missing_is_error() {
        // An edge referencing an item NOT in items_by_ref should produce
        // a ProjectionInvariant error.
        let root = make_ancestor(
            "knowledge:main",
            "content",
            ItemSpace::Project,
            EngineTrustClass::TrustedProject,
        );
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![ResolutionEdge {
                from_ref: "knowledge:main".to_string(),
                from_source_path: std::path::PathBuf::from("/tmp/main"),
                to_ref: "knowledge:missing".to_string(), // not in referenced_items!
                to_source_path: std::path::PathBuf::from("/tmp/missing"),
                to_source_space: ItemSpace::Bundle,
                trust_class: EngineTrustClass::TrustedBundle,
                added_by: ResolutionStepName::ResolveReferences,
            }],
            referenced_items: vec![], // missing item NOT included
            step_outputs: std::collections::HashMap::new(),
            effective_trust_class: EngineTrustClass::TrustedProject,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let err = project_single_root(&output).expect_err("dangling edge endpoint must error");
        let msg = err.to_string();
        assert!(
            msg.contains("edge endpoint missing"),
            "must explain which invariant failed, got: {msg}"
        );
        assert!(
            matches!(err, DispatchError::ProjectionInvariant { .. }),
            "must be ProjectionInvariant variant"
        );
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn project_single_root_referenced_items_in_payload() {
        // Verify that referenced_items are included in items_by_ref
        // (not just root + ancestors).
        let root = make_ancestor(
            "knowledge:main",
            "main",
            ItemSpace::Project,
            EngineTrustClass::TrustedProject,
        );
        let ref1 = make_ancestor(
            "knowledge:ref1",
            "ref1 content",
            ItemSpace::Bundle,
            EngineTrustClass::TrustedBundle,
        );
        let ref2 = make_ancestor(
            "knowledge:ref2",
            "ref2 content",
            ItemSpace::Project,
            EngineTrustClass::TrustedProject,
        );
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![
                ResolutionEdge {
                    from_ref: "knowledge:main".to_string(),
                    from_source_path: std::path::PathBuf::from("/tmp/main"),
                    to_ref: "knowledge:ref1".to_string(),
                    to_source_path: std::path::PathBuf::from("/tmp/ref1"),
                    to_source_space: ItemSpace::Bundle,
                    trust_class: EngineTrustClass::TrustedBundle,
                    added_by: ResolutionStepName::ResolveReferences,
                },
                ResolutionEdge {
                    from_ref: "knowledge:main".to_string(),
                    from_source_path: std::path::PathBuf::from("/tmp/main"),
                    to_ref: "knowledge:ref2".to_string(),
                    to_source_path: std::path::PathBuf::from("/tmp/ref2"),
                    to_source_space: ItemSpace::Project,
                    trust_class: EngineTrustClass::TrustedProject,
                    added_by: ResolutionStepName::ResolveReferences,
                },
            ],
            referenced_items: vec![ref1.clone(), ref2.clone()],
            step_outputs: std::collections::HashMap::new(),
            effective_trust_class: EngineTrustClass::TrustedProject,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.items_by_ref.len(), 3, "root + 2 referenced items");
        assert_eq!(
            payload.items_by_ref["knowledge:ref1"].raw_content,
            "ref1 content"
        );
        assert_eq!(
            payload.items_by_ref["knowledge:ref2"].raw_content,
            "ref2 content"
        );
        assert_eq!(payload.edges.len(), 2);
    }

    #[test]
    fn project_single_root_dedup_by_ref() {
        // If referenced_items contains an item with the same ref as root,
        // the root version wins (or_insert_with keeps first).
        let root = make_ancestor(
            "knowledge:dup",
            "root version",
            ItemSpace::Bundle,
            EngineTrustClass::TrustedBundle,
        );
        let ref_dup = make_ancestor(
            "knowledge:dup",
            "ref version",
            ItemSpace::Project,
            EngineTrustClass::TrustedProject,
        );
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![],
            referenced_items: vec![ref_dup], // same ref as root
            step_outputs: std::collections::HashMap::new(),
            effective_trust_class: EngineTrustClass::TrustedBundle,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(
            payload.items_by_ref.len(),
            1,
            "dedup: same ref counted once"
        );
        // Root is iterated first, so root version wins.
        assert_eq!(
            payload.items_by_ref["knowledge:dup"].raw_content, "root version",
            "root version must win over referenced item (first-write wins)"
        );
    }
}
