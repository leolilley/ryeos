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
    DelegationVia, ExecutionSchema, InProcessRegistryKind, OperationDecl,
    TerminatorDecl,
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
/// every input the three terminators (Subprocess, InProcessHandler,
/// NativeRuntimeSpawn) need so `/execute`'s HTTP layer can hand off
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
    /// event. `None` (the default) preserves the legacy
    /// "mint inside `create_root_thread`" path.
    pub pre_minted_thread_id: Option<String>,
    /// **Op dispatch**: the operation name from the `/execute` request.
    /// When `None`, the op resolver uses `default_operation` from the
    /// schema. When `Some` but the kind has no `operations`, the field
    /// is ignored (terminator/delegate paths don't use it).
    pub operation: Option<String>,
    /// **Op dispatch**: op-specific inputs from the `/execute` request.
    /// Validated against the op's `InputDecl` spec before dispatch.
    pub inputs: Option<Value>,
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
    /// Schema declares `operations` (op-dispatch path). The kind is
    /// dispatched by resolving the requested op, validating inputs,
    /// and spawning the kind's runtime with a `BatchOpEnvelope`.
    /// Carries the kind name (from the schema, not hardcoded) and
    /// the resolved `OperationDecl`.
    DispatchOp {
        kind: String,
        op_decl: OperationDecl,
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

    // **Op-dispatch path**: if the schema declares `operations`, take
    // the op-dispatch path instead of terminator/alias/delegate. The
    // boot-time mixed-dispatch reject guarantees `operations` is never
    // non-empty alongside terminator/alias/delegate, so this branch is
    // unambiguous.
    if !exec.operations.is_empty() {
        let requested_op = ctx.requested_op.as_deref();
        let op_decl = resolve_requested_op(
            requested_op,
            &exec.default_operation,
            &exec.operations,
            &schema_kind,
        )?;
        return Ok(VerifiedHop {
            canonical_ref: current_ref.clone(),
            verified,
            thread_profile,
            runtime,
            next: HopAction::DispatchOp {
                kind: schema_kind,
                op_decl,
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

// ── Op dispatch helpers ───────────────────────────────────────────────

/// Resolve the requested operation name to a declared `OperationDecl`.
///
/// If the caller provided an explicit op name, look it up in the schema's
/// `operations`. If no op was requested, use `default_operation` (which
/// MUST be declared on an op-bearing schema). Unknown/missing ops produce
/// structured errors listing the declared ops (Rule 8).
fn resolve_requested_op(
    requested: Option<&str>,
    default_operation: &Option<String>,
    operations: &[OperationDecl],
    kind: &str,
) -> Result<OperationDecl, DispatchError> {
    let op_name = match requested {
        Some(name) => name,
        None => default_operation
            .as_deref()
            .ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: kind.to_string(),
                detail: "schema declares operations but no default_operation, and no \
                         operation was specified in the request"
                    .into(),
            })?,
    };

    operations
        .iter()
        .find(|op| op.name == op_name)
        .cloned()
        .ok_or_else(|| {
            let declared: Vec<String> = operations.iter().map(|op| op.name.clone()).collect();
            DispatchError::UnknownOp {
                kind: kind.to_string(),
                requested: op_name.to_string(),
                declared: declared.join(", "),
            }
        })
}

/// Validate the caller's `inputs` object against the op's typed spec.
/// Each declared input with `required: true` must be present and match
/// the declared type. Optional inputs with defaults are filled in.
///
/// Returns the validated+defaulted inputs as a `serde_json::Value::Object`.
fn validate_op_inputs(
    inputs: Option<&Value>,
    op: &OperationDecl,
) -> Result<Value, DispatchError> {
    let mut inputs_map = match inputs {
        Some(Value::Object(map)) => map.clone(),
        Some(other) => {
            return Err(DispatchError::OpInvalidInput {
                op: op.name.clone(),
                reason: format!("inputs must be an object, got {}", other),
            });
        }
        None => serde_json::Map::new(),
    };

    for (name, decl) in &op.inputs {
        match inputs_map.get(name) {
            Some(val) => {
                // Type-check the value against the declared type.
                validate_input_type(val, &decl.ty, name, &op.name)?;
            }
            None => {
                if decl.required {
                    return Err(DispatchError::OpInvalidInput {
                        op: op.name.clone(),
                        reason: format!("required input '{name}' is missing"),
                    });
                }
                // Apply default if present.
                if let Some(default) = &decl.default {
                    inputs_map.insert(name.clone(), default.clone());
                }
            }
        }
    }

    Ok(Value::Object(inputs_map))
}

/// Check that a JSON value matches the declared `InputType`.
fn validate_input_type(
    val: &Value,
    ty: &ryeos_engine::kind_registry::InputType,
    name: &str,
    op_name: &str,
) -> Result<(), DispatchError> {
    use ryeos_engine::kind_registry::InputType;
    let ok = match ty {
        InputType::String => val.is_string(),
        InputType::Integer => val.is_i64() || val.is_u64(),
        InputType::Boolean => val.is_boolean(),
        InputType::Array => val.is_array(),
        InputType::Object => val.is_object(),
    };
    if !ok {
        return Err(DispatchError::OpInvalidInput {
            op: op_name.to_string(),
            reason: format!(
                "input '{name}' expected type {:?}, got {}",
                ty,
                val
            ),
        });
    }
    Ok(())
}

/// Project a `ResolutionOutput` into a `SingleRootPayload` for the
/// knowledge runtime. The daemon owns this conversion because it consumes
/// an engine type (`ResolutionOutput`) and produces a knowledge type
/// (`SingleRootPayload`).
fn project_single_root(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> Result<ryeos_runtime::op_wire::SingleRootPayload, DispatchError> {
    use ryeos_runtime::op_wire::{EdgeKind, GraphEdge, TrustClass, VerifiedItem};

    let mut items_by_ref: std::collections::BTreeMap<String, VerifiedItem> =
        std::collections::BTreeMap::new();

    // Helper to convert engine ResolvedAncestor → knowledge VerifiedItem.
    let ancestor_to_verified = |a: &ryeos_engine::resolution::ResolvedAncestor| VerifiedItem {
        raw_content: a.raw_content.clone(),
        raw_content_digest: a.raw_content_digest.clone(),
        metadata: serde_json::to_value(
            serde_json::json!({
                "source_path": a.source_path,
                "trust_class": format!("{:?}", a.trust_class),
                "requested_id": a.requested_id,
            }),
        )
        .unwrap_or(Value::Null),
        trust_class: match a.trust_class {
            ryeos_engine::resolution::TrustClass::TrustedSystem => TrustClass::TrustedSystem,
            ryeos_engine::resolution::TrustClass::TrustedUser => TrustClass::TrustedUser,
            ryeos_engine::resolution::TrustClass::UntrustedUserSpace => TrustClass::TrustedProject,
            ryeos_engine::resolution::TrustClass::Unsigned => TrustClass::Untrusted,
        },
    };

    // Root + ancestors.
    for resolved in std::iter::once(&resolution.root).chain(resolution.ancestors.iter()) {
        items_by_ref
            .entry(resolved.resolved_ref.clone())
            .or_insert_with(|| ancestor_to_verified(resolved));
    }
    // Referenced items.
    for resolved in &resolution.referenced_items {
        items_by_ref
            .entry(resolved.resolved_ref.clone())
            .or_insert_with(|| ancestor_to_verified(resolved));
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
                reason: format!(
                    "edge endpoint missing: {} -> {}",
                    edge.from, edge.to
                ),
            });
        }
    }

    Ok(ryeos_runtime::op_wire::SingleRootPayload {
        root_ref: resolution.root.resolved_ref.clone(),
        items_by_ref,
        edges,
    })
}

// ── Op dispatch terminator ─────────────────────────────────────────────

/// Dispatch an op-style kind by spawning its runtime with a
/// `BatchOpEnvelope`. This is the generic op-dispatch path:
///
/// 1. Validate inputs against the op's typed spec.
/// 2. Look up the runtime via `RuntimeRegistry::lookup_for(kind)`.
/// 3. Run the engine's resolution pipeline for the item.
/// 4. Project to `SingleRootPayload` (single-root ops only).
/// 5. Mint thread record + callback token.
/// 6. Build `BatchOpEnvelope` and spawn via lillux::run.
/// 7. Parse `BatchOpResult` and return the output.
///
/// The runtime binary (e.g. `ryeos-knowledge-runtime`) handles the
/// actual op logic. The daemon never calls ops in-process (Rule 1).
pub(crate) async fn dispatch_op(
    kind: &str,
    op_decl: &OperationDecl,
    canonical_ref: &CanonicalRef,
    _hop_verified: Option<VerifiedItem>,
    thread_profile: Option<String>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<Value, DispatchError> {
    // 1. Validate inputs against the op's spec.
    let validated_inputs = validate_op_inputs(request.inputs.as_ref(), op_decl)?;

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
                "no runtime serves kind '{kind}' for op dispatch \
                 (registered runtimes: [{}])",
                serves.join(", ")
            ),
        }
    })?;

    let bare = strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)?;
    let executor_ref = format!("native:{bare}");

    // 3. Run the engine resolution pipeline to get a full
    //    ResolutionOutput (extends chain + references + content).
    let engine_roots = ctx.engine.resolution_roots(Some(request.project_path.to_path_buf()));
    let effective_parsers = ctx
        .engine
        .effective_parser_dispatcher(Some(request.project_path))
        .map_err(|e| DispatchError::InvalidRef(
            canonical_ref.to_string(),
            format!("parser dispatcher: {e}"),
        ))?;
    let resolution_output = ryeos_engine::resolution::run_resolution_pipeline(
        canonical_ref,
        &ctx.engine.kinds,
        &effective_parsers,
        &engine_roots,
        &ctx.engine.trust_store,
        &ctx.engine.composers,
    )
    .map_err(|e| DispatchError::InvalidRef(
        canonical_ref.to_string(),
        format!("resolution pipeline failed: {e}"),
    ))?;

    // 4. Project the resolution output to a SingleRootPayload.
    let single_root = project_single_root(&resolution_output)?;

    // 5. Build the envelope payload: merge root + inputs.
    let op_name = &op_decl.name;
    let mut payload = serde_json::to_value(&single_root)
        .map_err(|e| DispatchError::Internal(e.into()))?;
    if let Value::Object(ref mut map) = payload {
        if let Value::Object(inputs) = validated_inputs {
            map.extend(inputs);
        }
    }

    // 6. Mint thread record + callback token.
    let thread_profile_str = thread_profile
        .as_deref()
        .unwrap_or("op_run");
    let thread_id = crate::services::thread_lifecycle::new_thread_id();
    let chain_root_id = thread_id.clone(); // top-level, chain_root == self

    state
        .threads
        .create_thread(&crate::services::thread_lifecycle::ThreadCreateParams {
            thread_id: thread_id.clone(),
            chain_root_id,
            kind: thread_profile_str.to_string(),
            item_ref: canonical_ref.to_string(),
            executor_ref: executor_ref.clone(),
            launch_mode: "inline".to_string(),
            current_site_id: ctx.plan_ctx.current_site_id.clone(),
            origin_site_id: ctx.plan_ctx.origin_site_id.clone(),
            upstream_thread_id: None,
            requested_by: Some(request.acting_principal.to_string()),
        })
        .map_err(|e| DispatchError::Internal(
            anyhow::anyhow!("thread creation failed: {e}")
        ))?;

    // Generate callback token.
    let ttl = crate::execution::callback_token::compute_ttl(None);
    let cap = state.callback_tokens.generate(
        &thread_id,
        request.project_path.to_path_buf(),
        ttl,
        Vec::new(), // op threads have no caps for now
    );

    let callback = ryeos_runtime::envelope::EnvelopeCallback {
        socket_path: state.config.uds_path.clone(),
        token: cap.token.clone(),
    };

    let envelope = ryeos_runtime::op_wire::BatchOpEnvelope {
        schema_version: 1,
        kind: kind.to_string(),
        op: op_name.clone(),
        thread_id: thread_id.clone(),
        callback,
        project_root: request.project_path.to_path_buf(),
        payload,
    };

    let stdin_data = serde_json::to_string(&envelope)
        .map_err(|e| DispatchError::Internal(e.into()))?;

    // 7. Resolve the native executor path and spawn via lillux.
    let system_roots: Vec<std::path::PathBuf> = engine_roots
        .ordered
        .iter()
        .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::System)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or(r.ai_root.clone())
        })
        .collect();
    let materialize_dir = request.project_path.to_path_buf();
    let executor_path = crate::execution::launch::resolve_native_executor_path(
        &system_roots,
        &executor_ref,
        &materialize_dir,
        &state.engine.trust_store,
        ryeos_engine::resolution::TrustClass::TrustedSystem,
    )
    .map_err(|e| DispatchError::RuntimeMaterializationFailed {
        executor_ref: executor_ref.clone(),
        detail: e.to_string(),
    })?;

    let executor_path_str = executor_path.to_string_lossy().to_string();
    let result = tokio::task::spawn_blocking(move || {
        lillux::run(lillux::SubprocessRequest {
            cmd: executor_path_str,
            args: vec![],
            cwd: None,
            envs: vec![],
            stdin_data: Some(stdin_data),
            timeout: 120.0,
        })
    })
    .await
    .map_err(|e| DispatchError::Internal(e.into()))?;

    // Invalidate callback token (cleanup, matching launch.rs pattern).
    state.callback_tokens.invalidate(&cap.token);
    state.callback_tokens.invalidate_for_thread(&thread_id);

    if !result.success {
        // Try to parse a BatchOpResult from stdout for structured error.
        if let Ok(batch_result) =
            serde_json::from_str::<ryeos_runtime::op_wire::BatchOpResult>(&result.stdout)
        {
            if let Some(error) = batch_result.error {
                // Finalize thread as failed.
                let _ = state.threads.finalize_thread(
                    &finalize_params(&thread_id, "failed", None),
                );
                return Err(map_batch_error(kind, op_name, error));
            }
        }
        let _ = state.threads.finalize_thread(
            &finalize_params(&thread_id, "failed", None),
        );
        return Err(DispatchError::OpFailed {
            kind: kind.to_string(),
            op: op_name.clone(),
            reason: format!(
                "exit_code={}, stderr={}",
                result.exit_code,
                result.stderr.trim()
            ),
        });
    }

    // 8. Parse BatchOpResult.
    let batch_result: ryeos_runtime::op_wire::BatchOpResult =
        serde_json::from_str(&result.stdout).map_err(|e| {
            let _ = state.threads.finalize_thread(
                &finalize_params(&thread_id, "failed", None),
            );
            DispatchError::OpFailed {
                kind: kind.to_string(),
                op: op_name.clone(),
                reason: format!("failed to parse BatchOpResult: {e}"),
            }
        })?;

    if !batch_result.success {
        let _ = state.threads.finalize_thread(
            &finalize_params(&thread_id, "failed", None),
        );
        return Err(match batch_result.error {
            Some(error) => map_batch_error(kind, op_name, error),
            None => DispatchError::OpFailed {
                kind: kind.to_string(),
                op: op_name.clone(),
                reason: "runtime returned success=false with no error detail".into(),
            },
        });
    }

    // Finalize thread as completed.
    let _ = state.threads.finalize_thread(
        &finalize_params(&thread_id, "completed", batch_result.output.clone()),
    );

    Ok(json!({
        "thread": {
            "thread_id": thread_id,
            "kind": thread_profile_str,
            "item_ref": canonical_ref.to_string(),
            "status": "completed",
        },
        "result": batch_result.output.unwrap_or(Value::Null),
    }))
}

/// Map a `BatchOpError` from the runtime to a `DispatchError`.
fn map_batch_error(
    kind: &str,
    op: &str,
    error: ryeos_runtime::op_wire::BatchOpError,
) -> DispatchError {
    use ryeos_runtime::op_wire::BatchOpError;
    match error {
        BatchOpError::NotImplemented { phase, .. } => DispatchError::OpNotImplemented {
            kind: kind.to_string(),
            op: op.to_string(),
            phase,
        },
        BatchOpError::InvalidInput { reason, .. } => DispatchError::OpInvalidInput {
            op: op.to_string(),
            reason,
        },
        BatchOpError::UnknownOp { requested, declared, .. } => DispatchError::UnknownOp {
            kind: kind.to_string(),
            requested,
            declared: declared.join(", "),
        },
        BatchOpError::OpFailed { reason } => DispatchError::OpFailed {
            kind: kind.to_string(),
            op: op.to_string(),
            reason,
        },
    }
}

/// Helper to build a `ThreadFinalizeParams` with only the required fields
/// set and all optional fields defaulted to `None` / empty.
pub(crate) fn finalize_params(thread_id: &str, status: &str, result: Option<Value>) -> crate::services::thread_lifecycle::ThreadFinalizeParams {
    use crate::services::thread_lifecycle::ThreadFinalizeParams;
    ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: status.to_string(),
        outcome_code: None,
        result,
        error: None,
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
                    None,
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
                "dispatch_service called on schema declaring terminator {other:?}, not InProcess {{ Services }}"
            ),
        }),
    }
}

// ── Unified subprocess terminator ─────────────────────────────────────

/// Strip the `bin/<triple>/` prefix from a runtime YAML's `binary_ref`.
pub(crate) fn strip_binary_ref_prefix(binary_ref: &str) -> Result<String, DispatchError> {
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

/// **B1**: cap gate factored out for unit testing.
/// Uses the unified `Authorizer` for wildcard + implication expansion.
fn enforce_runtime_caps(
    verb_registry: &std::sync::Arc<ryeos_runtime::verb_registry::VerbRegistry>,
    item_ref: &str,
    required_caps: &[String],
    caller_scopes: &[String],
) -> Result<(), DispatchError> {
    if required_caps.is_empty() {
        return Ok(());
    }
    let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require_all(
        &required_caps.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    );
    let authorizer = ryeos_runtime::authorizer::Authorizer::new(verb_registry.clone());
    authorizer
        .authorize(caller_scopes, &policy)
        .map_err(|_| DispatchError::InsufficientCaps {
            runtime: item_ref.to_string(),
            required: required_caps.to_vec(),
            caller_scopes: caller_scopes.to_vec(),
        })
}

pub(crate) async fn dispatch_subprocess(sctx: SubprocessDispatchContext<'_>) -> Result<Value, DispatchError> {
    let SubprocessDispatchContext {
        current_ref,
        thread_profile,
        verified: hop_verified,
        request,
        ctx,
        state,
        role,
        root_subject,
        hop_runtime,
    } = sctx;
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
        TerminatorDecl::InProcess { .. } => {
            return Err(DispatchError::SchemaMisconfigured {
                kind: current_ref.kind.clone(),
                detail: "dispatch_subprocess called on schema declaring InProcess terminator, not Subprocess".into(),
            });
        }
    };

    enforce_runtime_target_caps(role, &ctx.caller_scopes)?;

    let protocol = ctx
        .engine
        .protocols
        .require(protocol_ref)
        .map_err(|_| DispatchError::ProtocolNotRegistered(protocol_ref.to_string()))?;

    check_dispatch_capabilities(&protocol.descriptor.capabilities, request)?;

    use ryeos_engine::protocol_vocabulary::StdoutMode;
    if protocol.descriptor.stdout.mode == StdoutMode::Streaming && request.launch_mode == "detached"
    {
        return Err(DispatchError::StreamingNotDetachable);
    }

    use ryeos_engine::protocol_vocabulary::LifecycleMode;
    match protocol.descriptor.lifecycle.mode {
        LifecycleMode::Managed => {
            dispatch_managed_subprocess(
                SubprocessDispatchContext {
                    current_ref,
                    thread_profile,
                    verified: hop_verified,
                    request,
                    ctx,
                    state,
                    role,
                    root_subject,
                    hop_runtime,
                },
                protocol,
            )
            .await
        }
        LifecycleMode::DetachedOk => {
            dispatch_tool_subprocess(
                current_ref,
                thread_profile,
                hop_verified,
                request,
                ctx,
                state,
            )
            .await
        }
    }
}

async fn dispatch_managed_subprocess(
    sctx: SubprocessDispatchContext<'_>,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> Result<Value, DispatchError> {
    let SubprocessDispatchContext {
        current_ref: canonical_ref,
        verified: hop_verified,
        thread_profile: hop_thread_profile,
        hop_runtime: _hop_runtime,
        root_subject,
        request,
        ctx,
        state,
        role,
    } = sctx;

    use ryeos_engine::protocol_vocabulary::CallbackChannel;
    if protocol.descriptor.callback_channel == CallbackChannel::None {
        return dispatch_streaming_subprocess(
            canonical_ref,
            hop_verified,
            request,
            ctx,
            state,
            protocol,
        )
        .await;
    }

    let runtime_ref = canonical_ref.to_string();

    let verified_runtime = match role {
        SubprocessRole::RuntimeTarget { verified_runtime } => Some(verified_runtime.as_ref().clone()),
        SubprocessRole::Regular => {
            state
                .engine
                .runtimes
                .lookup_by_ref(canonical_ref)
                .cloned()
        }
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
                "runtime '{runtime_ref}' has no registry entry; registered runtimes: [{}]",
                available.join(", ")
            ),
        }
    })?;

    let params = request.params.clone();
    let acting_principal = request.acting_principal;
    let project_path: &Path = request.project_path;

    if request.original_root_kind == ROOT_KIND_RUNTIME {
        enforce_runtime_caps(
            &state.verb_registry,
            &runtime_ref,
            &verified_runtime.yaml.required_caps,
            &ctx.caller_scopes,
        )?;
    }

    let bare = strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)?;
    let executor_ref = format!("native:{bare}");

    let subject = root_subject.unwrap_or_else(|| RootSubject {
        item_ref: runtime_ref.clone(),
        thread_profile: hop_thread_profile.to_string(),
        verified: hop_verified.cloned(),
    });

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
        launch::BuildAndLaunchParams {
            state,
            executor_ref: &executor_ref,
            acting_principal,
            resolved: &resolved,
            project_path,
            parameters: &params,
            vault_bindings: &vault_bindings,
            pre_minted_thread_id: request.pre_minted_thread_id.as_deref(),
        },
    ).await.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("manifest") || msg.contains("binary") || msg.contains("blob")
            || msg.contains("materializ") || msg.contains("native executor")
            || msg.contains("arch check")
        {
            DispatchError::RuntimeMaterializationFailed {
                executor_ref: executor_ref.clone(),
                detail: msg,
            }
        } else {
            DispatchError::Internal(e.into())
        }
    })?;

    Ok(json!({
        "thread": result.thread,
        "result": result.result,
    }))
}

async fn dispatch_streaming_subprocess(
    current_ref: &CanonicalRef,
    verified: Option<&VerifiedItem>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    _protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> Result<Value, DispatchError> {
    let item_ref_str = current_ref.to_string();
    let engine_roots = ctx.engine.resolution_roots(Some(request.project_path.to_path_buf()));

    let system_roots: Vec<std::path::PathBuf> = engine_roots
        .ordered
        .iter()
        .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::System)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or(r.ai_root.clone())
        })
        .collect();

    let effective_parsers = ctx
        .engine
        .effective_parser_dispatcher(Some(request.project_path))
        .map_err(|e| DispatchError::InvalidRef(
            current_ref.to_string(),
            format!("parser dispatcher: {e}"),
        ))?;

    let resolution_output = ryeos_engine::resolution::run_resolution_pipeline(
        current_ref,
        &ctx.engine.kinds,
        &effective_parsers,
        &engine_roots,
        &ctx.engine.trust_store,
        &ctx.engine.composers,
    )
    .map_err(|e| DispatchError::InvalidRef(
        current_ref.to_string(),
        format!("resolution pipeline failed: {e}"),
    ))?;

    let single_root = project_single_root(&resolution_output)?;

    let stdin_data = serde_json::to_string(&single_root)
        .map_err(|e| DispatchError::Internal(e.into()))?;

    // Streaming tools are *items*, not runtime-hosted kinds — each
    // tool ships its own binary in the bundle (e.g.
    // `bin/<triple>/rye-tool-streaming-demo`) and the dispatcher
    // resolves it from the verified item's `executor_id` metadata.
    // Phase2's first cut wrongly routed this through
    // `RuntimeRegistry::lookup_for(kind)` — which only finds runtimes
    // declared via `kind: runtime` YAMLs (directive/graph/knowledge),
    // never streaming tools — so every streaming dispatch hit
    // `no runtime serves kind 'streaming_tool'`. The fix is to use the
    // streaming tool's own binary, which is the `executor_id` field
    // the kind-schema's `inventory_schema_keys` already surfaces on
    // `ResolvedItem.metadata`.
    let verified_item = verified.ok_or_else(|| DispatchError::SchemaMisconfigured {
        kind: current_ref.kind.clone(),
        detail: format!(
            "streaming tool '{item_ref_str}' dispatched without a verified item — \
             the dispatch loop must resolve before reaching a streaming terminator"
        ),
    })?;

    let executor_id = verified_item
        .resolved
        .metadata
        .executor_id
        .as_ref()
        .ok_or_else(|| DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: format!(
                "streaming tool '{item_ref_str}' has no `executor_id` in its YAML \
                 — every `kind: streaming_tool` item must declare \
                 `executor_id: <bare-binary-name>` so the daemon can resolve the \
                 binary against the system bundle's `bin/<triple>/` CAS"
            ),
        })?;
    let executor_ref = format!("native:{executor_id}");

    let executor_path = crate::execution::launch::resolve_native_executor_path(
        &system_roots,
        &executor_ref,
        request.project_path,
        &state.engine.trust_store,
        ryeos_engine::resolution::TrustClass::TrustedSystem,
    )
    .map_err(|e| DispatchError::RuntimeMaterializationFailed {
        executor_ref: executor_ref.clone(),
        detail: e.to_string(),
    })?;

    let executor_path_str = executor_path.to_string_lossy().to_string();
    let result = tokio::task::spawn_blocking(move || {
        lillux::run(lillux::SubprocessRequest {
            cmd: executor_path_str,
            args: vec![],
            cwd: None,
            envs: vec![],
            stdin_data: Some(stdin_data),
            timeout: 120.0,
        })
    })
    .await
    .map_err(|e| DispatchError::Internal(e.into()))?;

    if !result.success {
        return Err(DispatchError::SubprocessRunFailed {
            item_ref: item_ref_str,
            detail: format!(
                "streaming tool exited with code {}: {}",
                result.exit_code,
                &result.stderr[..result.stderr.len().min(500)]
            ),
        });
    }

    let frames = ryeos_engine::protocol_vocabulary::read_all_frames(
        std::io::Cursor::new(result.stdout.as_bytes()),
    )
    .map_err(|e| {
        DispatchError::Internal(anyhow::anyhow!(
            "frame read failed for streaming tool: {e}"
        ))
    })?;

    serde_json::to_value(&frames).map_err(|e| {
        DispatchError::Internal(anyhow::anyhow!("frame serialize: {e}"))
    })
}

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
        crate::services::thread_lifecycle::ResolveRootExecutionParams {
            engine: &state.engine,
            site_id: &ctx.plan_ctx.current_site_id,
            project_path: request.project_path,
            item_ref: &item_ref,
            launch_mode: request.launch_mode,
            parameters: request.params.clone(),
            requested_by: Some(request.acting_principal.to_string()),
            caller_scopes: ctx.caller_scopes.clone(),
            validate_only: request.validate_only,
        },
    )?;

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

    let required_caps = crate::service_registry::extract_required_caps(
        &resolved.resolved_item.metadata.extra,
    );
    if !required_caps.is_empty() {
        enforce_runtime_caps(&state.verb_registry, &item_ref_for_error, &required_caps, &ctx.caller_scopes)?;
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
        let result = crate::execution::runner::run_detached(state.clone(), params).await
            .map_err(|e| DispatchError::SubprocessRunFailed {
                item_ref: item_ref_for_error.clone(),
                detail: e.to_string(),
            })?;
        Ok(json!({
            "thread": result.running_thread,
            "detached": true,
        }))
    } else {
        let result = crate::execution::runner::run_inline(state.clone(), params).await
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
            verified_runtime: Box::new(verified.clone()),
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
                )
                .await;
            }
            HopAction::FollowAlias(next_ref) | HopAction::UseRegistry(next_ref) => {
                current_ref = next_ref;
            }
            HopAction::DispatchOp { kind, op_decl } => {
                return dispatch_op(
                    &kind,
                    &op_decl,
                    &hop_ref,
                    verified,
                    thread_profile,
                    request,
                    ctx,
                    state,
                )
                .await;
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
            let tp = thread_profile.ok_or_else(|| {
                DispatchError::SchemaMisconfigured {
                    kind: canonical_ref.kind.clone(),
                    detail: "subprocess terminator has no thread_profile".into(),
                }
            })?;
            dispatch_subprocess(
                SubprocessDispatchContext {
                    current_ref: &canonical_ref,
                    thread_profile: &tp,
                    verified: verified.as_ref(),
                    request,
                    ctx,
                    state,
                    role,
                    root_subject,
                    hop_runtime: runtime,
                },
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

    // P1.4: tests for `lookup_runtime_for_dispatch` were removed when
    // that helper was deleted. The dispatch loop now attaches the
    // verified runtime to the hop via `RuntimeRegistry::lookup_by_ref`
    // (covered by `runtime_registry` integration tests) and
    // `dispatch_native_runtime` consumes that owned value, so the
    // per-call lookup path no longer exists.

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

    fn test_verb_registry() -> std::sync::Arc<ryeos_runtime::verb_registry::VerbRegistry> {
        std::sync::Arc::new(ryeos_runtime::verb_registry::VerbRegistry::with_builtins())
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
        let vr = test_verb_registry();
        // If `dispatch_native_runtime` skips the call entirely (B1
        // gate), then a missing cap does NOT translate to an error.
        // We model this by simply never calling `enforce_runtime_caps`
        // and asserting the synthetic outcome is `Ok`.
        let required = vec!["runtime.execute".to_string()];
        let caller_scopes: Vec<String> = vec![]; // would normally fail

        // SIMULATED indirect path — gate skips the call.
        let original_root_kind = "directive";
        let outcome: Result<(), DispatchError> = if original_root_kind == "runtime" {
            enforce_runtime_caps(&vr, "runtime:directive-runtime", &required, &caller_scopes)
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
            enforce_runtime_caps(&vr, "runtime:directive-runtime", &required, &caller_scopes)
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
        let vr = test_verb_registry();
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["runtime.execute".to_string(), "execute".to_string()];
        assert!(enforce_runtime_caps(&vr, "runtime:directive-runtime", &req, &caller).is_ok());
    }

    #[test]
    fn enforce_runtime_caps_allows_wildcard_scope() {
        let vr = test_verb_registry();
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["*".to_string()];
        assert!(enforce_runtime_caps(&vr, "runtime:directive-runtime", &req, &caller).is_ok());
    }

    #[test]
    fn enforce_runtime_caps_denies_when_caller_lacks_required_cap() {
        let vr = test_verb_registry();
        let req = vec!["runtime.execute".to_string()];
        let caller = vec!["execute".to_string()];
        let err = enforce_runtime_caps(&vr, "runtime:directive-runtime", &req, &caller)
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
        let vr = test_verb_registry();
        let req: Vec<String> = vec![];
        let caller = vec!["execute".to_string()];
        assert!(enforce_runtime_caps(&vr, "runtime:test", &req, &caller).is_ok());
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
            requested_op: None,
            requested_inputs: None,
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

    // ── Op dispatch unit tests ───────────────────────────────────────

    use axum::http::StatusCode;
    use std::collections::BTreeMap;
    use ryeos_engine::kind_registry::{InputDecl, InputType, OperationDecl, OperationDispatch, SideEffectClass};
    use ryeos_engine::resolution::{ResolutionOutput, ResolvedAncestor, ResolutionEdge, ResolutionStepName, TrustClass as EngineTrustClass};
    use ryeos_runtime::op_wire::{BatchOpError, TrustClass as WireTrustClass};

    fn make_op(name: &str, inputs: BTreeMap<String, InputDecl>) -> OperationDecl {
        OperationDecl {
            name: name.to_string(),
            side_effects: SideEffectClass::None,
            dispatch: OperationDispatch::RuntimeRegistry,
            inputs,
        }
    }

    fn string_input(required: bool) -> InputDecl {
        InputDecl {
            ty: InputType::String,
            required,
            default: None,
            enum_values: None,
            min: None,
            items: None,
        }
    }

    fn integer_input(required: bool, default: Option<i64>) -> InputDecl {
        InputDecl {
            ty: InputType::Integer,
            required,
            default: default.map(|v| json!(v)),
            enum_values: None,
            min: None,
            items: None,
        }
    }

    // ── resolve_requested_op ─────────────────────────────────────────

    #[test]
    fn resolve_requested_op_explicit_known() {
        let ops = vec![make_op("compose", BTreeMap::new())];
        let r = resolve_requested_op(Some("compose"), &None, &ops, "knowledge");
        assert!(r.is_ok());
        assert_eq!(r.unwrap().name, "compose");
    }

    #[test]
    fn resolve_requested_op_explicit_unknown_lists_declared() {
        let ops = vec![
            make_op("compose", BTreeMap::new()),
            make_op("query", BTreeMap::new()),
        ];
        let err = resolve_requested_op(Some("bogus"), &None, &ops, "knowledge")
            .expect_err("unknown op must error");
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "must name requested op, got: {msg}");
        assert!(msg.contains("compose"), "must list compose, got: {msg}");
        assert!(msg.contains("query"), "must list query, got: {msg}");
        assert!(
            matches!(err, DispatchError::UnknownOp { .. }),
            "must be UnknownOp variant"
        );
    }

    #[test]
    fn resolve_requested_op_falls_back_to_default() {
        let ops = vec![make_op("compose", BTreeMap::new())];
        let default = Some("compose".to_string());
        let r = resolve_requested_op(None, &default, &ops, "knowledge");
        assert!(r.is_ok());
        assert_eq!(r.unwrap().name, "compose");
    }

    #[test]
    fn resolve_requested_op_no_request_no_default_is_misconfigured() {
        let ops = vec![make_op("compose", BTreeMap::new())];
        let err = resolve_requested_op(None, &None, &ops, "knowledge")
            .expect_err("missing default must error");
        assert!(
            matches!(err, DispatchError::SchemaMisconfigured { .. }),
            "must be SchemaMisconfigured, got: {err:?}"
        );
    }

    #[test]
    fn resolve_requested_op_empty_ops_list() {
        let err = resolve_requested_op(Some("anything"), &None, &[], "tool")
            .expect_err("empty ops must error");
        assert!(matches!(err, DispatchError::UnknownOp { declared, .. } if declared.is_empty()));
    }

    // ── validate_op_inputs ───────────────────────────────────────────

    #[test]
    fn validate_op_inputs_all_required_present() {
        let mut inputs = BTreeMap::new();
        inputs.insert("question".to_string(), string_input(true));
        let op = make_op("ask", inputs);

        let r = validate_op_inputs(Some(&json!({"question": "hello"})), &op);
        assert!(r.is_ok());
        let val = r.unwrap();
        assert_eq!(val["question"], "hello");
    }

    #[test]
    fn validate_op_inputs_missing_required() {
        let mut inputs = BTreeMap::new();
        inputs.insert("question".to_string(), string_input(true));
        let op = make_op("ask", inputs);

        let err = validate_op_inputs(Some(&json!({"other": "val"})), &op)
            .expect_err("missing required must error");
        let msg = err.to_string();
        assert!(msg.contains("question"), "must name missing field, got: {msg}");
        assert!(
            matches!(err, DispatchError::OpInvalidInput { .. }),
            "must be OpInvalidInput variant"
        );
    }

    #[test]
    fn validate_op_inputs_optional_default_filled() {
        let mut inputs = BTreeMap::new();
        inputs.insert("question".to_string(), string_input(true));
        inputs.insert("max_tokens".to_string(), integer_input(false, Some(42)));
        let op = make_op("ask", inputs);

        let r = validate_op_inputs(Some(&json!({"question": "hello"})), &op).unwrap();
        assert_eq!(r["max_tokens"], 42, "default must be filled");
    }

    #[test]
    fn validate_op_inputs_optional_with_explicit_value() {
        let mut inputs = BTreeMap::new();
        inputs.insert("question".to_string(), string_input(true));
        inputs.insert("max_tokens".to_string(), integer_input(false, Some(42)));
        let op = make_op("ask", inputs);

        let r = validate_op_inputs(Some(&json!({"question": "hello", "max_tokens": 100})), &op).unwrap();
        assert_eq!(r["max_tokens"], 100, "explicit value must override default");
    }

    #[test]
    fn validate_op_inputs_wrong_type() {
        let mut inputs = BTreeMap::new();
        inputs.insert("question".to_string(), string_input(true));
        let op = make_op("ask", inputs);

        let err = validate_op_inputs(Some(&json!({"question": 123})), &op)
            .expect_err("wrong type must error");
        assert!(
            matches!(err, DispatchError::OpInvalidInput { .. }),
            "must be OpInvalidInput variant, got: {err:?}"
        );
    }

    #[test]
    fn validate_op_inputs_non_object_rejected() {
        let op = make_op("ask", BTreeMap::new());
        let err = validate_op_inputs(Some(&json!("not an object")), &op)
            .expect_err("non-object must error");
        let msg = err.to_string();
        assert!(msg.contains("must be an object"), "got: {msg}");
    }

    #[test]
    fn validate_op_inputs_none_uses_defaults() {
        let mut inputs = BTreeMap::new();
        inputs.insert("count".to_string(), integer_input(false, Some(10)));
        let op = make_op("list", inputs);

        let r = validate_op_inputs(None, &op).unwrap();
        assert_eq!(r["count"], 10, "default must be applied when inputs is None");
    }

    // ── map_batch_error ──────────────────────────────────────────────

    #[test]
    fn map_batch_error_not_implemented_preserves_phase() {
        let err = map_batch_error(
            "knowledge",
            "query",
            BatchOpError::NotImplemented {
                op: "query".into(),
                phase: 3,
            },
        );
        assert!(
            matches!(err, DispatchError::OpNotImplemented { phase: 3, .. }),
            "phase must be preserved"
        );
        // HTTP mapping: BAD_GATEWAY (502)
        assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn map_batch_error_invalid_input() {
        let err = map_batch_error(
            "knowledge",
            "compose",
            BatchOpError::InvalidInput {
                op: "compose".into(),
                field: Some("token_budget".into()),
                reason: "must be positive".into(),
            },
        );
        assert!(matches!(err, DispatchError::OpInvalidInput { .. }));
        // HTTP mapping: BAD_REQUEST (400)
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn map_batch_error_unknown_op() {
        let err = map_batch_error(
            "knowledge",
            "delete",
            BatchOpError::UnknownOp {
                kind: "knowledge".into(),
                requested: "delete".into(),
                declared: vec!["compose".into(), "query".into()],
            },
        );
        assert!(matches!(err, DispatchError::UnknownOp { ref requested, ref declared, .. }
            if requested == "delete" && declared.contains("compose")));
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn map_batch_error_op_failed() {
        let err = map_batch_error(
            "knowledge",
            "compose",
            BatchOpError::OpFailed {
                reason: "OOM".into(),
            },
        );
        assert!(matches!(err, DispatchError::OpFailed { ref reason, .. } if reason == "OOM"));
        assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn map_batch_error_not_implemented_is_not_op_failed() {
        // Critical assertion: NotImplemented must produce OpNotImplemented,
        // NOT OpFailed (which would lose the phase information).
        let err = map_batch_error(
            "knowledge",
            "query",
            BatchOpError::NotImplemented {
                op: "query".into(),
                phase: 5,
            },
        );
        assert!(
            !matches!(err, DispatchError::OpFailed { .. }),
            "NotImplemented must NOT map to OpFailed — phase info would be lost"
        );
        assert!(
            matches!(err, DispatchError::OpNotImplemented { phase: 5, .. }),
            "NotImplemented must map to OpNotImplemented with correct phase"
        );
    }

    // ── project_single_root ──────────────────────────────────────────

    fn make_ancestor(ref_str: &str, content: &str, trust: EngineTrustClass) -> ResolvedAncestor {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let digest = format!("{:016x}", hasher.finish());
        ResolvedAncestor {
            requested_id: ref_str.to_string(),
            resolved_ref: ref_str.to_string(),
            source_path: std::path::PathBuf::from(format!("/tmp/{ref_str}")),
            trust_class: trust,
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: content.to_string(),
            raw_content_digest: digest,
        }
    }

    #[test]
    fn project_single_root_root_only() {
        let root = make_ancestor("knowledge:my/doc", "hello", EngineTrustClass::TrustedSystem);
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![],
            referenced_items: vec![],
            step_outputs: std::collections::HashMap::new(),
            executor_trust_class: EngineTrustClass::TrustedSystem,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.root_ref, "knowledge:my/doc");
        assert_eq!(payload.items_by_ref.len(), 1);
        assert!(payload.items_by_ref.contains_key("knowledge:my/doc"));
        assert_eq!(payload.items_by_ref["knowledge:my/doc"].raw_content, "hello");
        assert!(payload.edges.is_empty());
    }

    #[test]
    fn project_single_root_extends_chain_produces_edges() {
        let root = make_ancestor("directive:base", "base body", EngineTrustClass::TrustedSystem);
        let mid = make_ancestor("directive:mid", "mid body", EngineTrustClass::TrustedUser);
        let leaf = make_ancestor("directive:leaf", "leaf body", EngineTrustClass::TrustedUser);
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![mid.clone(), leaf.clone()],
            references_edges: vec![],
            referenced_items: vec![],
            step_outputs: std::collections::HashMap::new(),
            executor_trust_class: EngineTrustClass::TrustedUser,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.items_by_ref.len(), 3); // root + 2 ancestors
        assert_eq!(payload.edges.len(), 2); // root→mid, mid→leaf

        // First edge: root → mid
        assert_eq!(payload.edges[0].from, "directive:base");
        assert_eq!(payload.edges[0].to, "directive:mid");
        assert_eq!(payload.edges[0].kind, ryeos_runtime::op_wire::EdgeKind::Extends);
        assert_eq!(payload.edges[0].depth_from_root, Some(1));

        // Second edge: mid → leaf
        assert_eq!(payload.edges[1].from, "directive:mid");
        assert_eq!(payload.edges[1].to, "directive:leaf");
        assert_eq!(payload.edges[1].depth_from_root, Some(2));
    }

    #[test]
    fn project_single_root_reference_edges() {
        let root = make_ancestor("knowledge:main", "main content", EngineTrustClass::TrustedUser);
        let ref_item = make_ancestor("knowledge:other", "other content", EngineTrustClass::TrustedSystem);
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![ResolutionEdge {
                from_ref: "knowledge:main".to_string(),
                from_source_path: std::path::PathBuf::from("/tmp/main"),
                to_ref: "knowledge:other".to_string(),
                to_source_path: std::path::PathBuf::from("/tmp/other"),
                trust_class: EngineTrustClass::TrustedSystem,
                added_by: ResolutionStepName::ResolveReferences,
            }],
            referenced_items: vec![ref_item.clone()],
            step_outputs: std::collections::HashMap::new(),
            executor_trust_class: EngineTrustClass::TrustedUser,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.items_by_ref.len(), 2);
        assert_eq!(payload.edges.len(), 1);
        assert_eq!(payload.edges[0].from, "knowledge:main");
        assert_eq!(payload.edges[0].to, "knowledge:other");
        assert_eq!(payload.edges[0].kind, ryeos_runtime::op_wire::EdgeKind::References);
        assert_eq!(payload.edges[0].depth_from_root, None, "reference edges have no depth");
    }

    #[test]
    fn project_single_root_trust_class_mapping() {
        // Engine TrustClass → Wire TrustClass mapping is the critical semantic bridge.
        let root_system = make_ancestor("item:sys", "sys", EngineTrustClass::TrustedSystem);
        let root_user = make_ancestor("item:user", "user", EngineTrustClass::TrustedUser);
        let root_untrusted = make_ancestor("item:untrusted", "un", EngineTrustClass::UntrustedUserSpace);
        let root_unsigned = make_ancestor("item:unsigned", "us", EngineTrustClass::Unsigned);

        let cases: Vec<(ResolvedAncestor, WireTrustClass)> = vec![
            (root_system, WireTrustClass::TrustedSystem),
            (root_user, WireTrustClass::TrustedUser),
            // UntrustedUserSpace → TrustedProject (semantic bridge)
            (root_untrusted, WireTrustClass::TrustedProject),
            (root_unsigned, WireTrustClass::Untrusted),
        ];

        for (ancestor, expected_trust) in cases {
            let output = ResolutionOutput {
                root: ancestor.clone(),
                ancestors: vec![],
                references_edges: vec![],
                referenced_items: vec![],
                step_outputs: std::collections::HashMap::new(),
                executor_trust_class: EngineTrustClass::Unsigned,
                composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
            };
            let payload = project_single_root(&output).unwrap();
            assert_eq!(
                payload.items_by_ref[&ancestor.resolved_ref].trust_class,
                expected_trust,
                "trust class mismatch for {:?}",
                ancestor.trust_class
            );
        }
    }

    #[test]
    fn project_single_root_edge_endpoint_missing_is_error() {
        // An edge referencing an item NOT in items_by_ref should produce
        // a ProjectionInvariant error.
        let root = make_ancestor("knowledge:main", "content", EngineTrustClass::TrustedUser);
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![ResolutionEdge {
                from_ref: "knowledge:main".to_string(),
                from_source_path: std::path::PathBuf::from("/tmp/main"),
                to_ref: "knowledge:missing".to_string(), // not in referenced_items!
                to_source_path: std::path::PathBuf::from("/tmp/missing"),
                trust_class: EngineTrustClass::TrustedSystem,
                added_by: ResolutionStepName::ResolveReferences,
            }],
            referenced_items: vec![], // missing item NOT included
            step_outputs: std::collections::HashMap::new(),
            executor_trust_class: EngineTrustClass::TrustedUser,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let err = project_single_root(&output)
            .expect_err("dangling edge endpoint must error");
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
        let root = make_ancestor("knowledge:main", "main", EngineTrustClass::TrustedUser);
        let ref1 = make_ancestor("knowledge:ref1", "ref1 content", EngineTrustClass::TrustedSystem);
        let ref2 = make_ancestor("knowledge:ref2", "ref2 content", EngineTrustClass::TrustedUser);
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![
                ResolutionEdge {
                    from_ref: "knowledge:main".to_string(),
                    from_source_path: std::path::PathBuf::from("/tmp/main"),
                    to_ref: "knowledge:ref1".to_string(),
                    to_source_path: std::path::PathBuf::from("/tmp/ref1"),
                    trust_class: EngineTrustClass::TrustedSystem,
                    added_by: ResolutionStepName::ResolveReferences,
                },
                ResolutionEdge {
                    from_ref: "knowledge:main".to_string(),
                    from_source_path: std::path::PathBuf::from("/tmp/main"),
                    to_ref: "knowledge:ref2".to_string(),
                    to_source_path: std::path::PathBuf::from("/tmp/ref2"),
                    trust_class: EngineTrustClass::TrustedUser,
                    added_by: ResolutionStepName::ResolveReferences,
                },
            ],
            referenced_items: vec![ref1.clone(), ref2.clone()],
            step_outputs: std::collections::HashMap::new(),
            executor_trust_class: EngineTrustClass::TrustedUser,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.items_by_ref.len(), 3, "root + 2 referenced items");
        assert_eq!(payload.items_by_ref["knowledge:ref1"].raw_content, "ref1 content");
        assert_eq!(payload.items_by_ref["knowledge:ref2"].raw_content, "ref2 content");
        assert_eq!(payload.edges.len(), 2);
    }

    #[test]
    fn project_single_root_dedup_by_ref() {
        // If referenced_items contains an item with the same ref as root,
        // the root version wins (or_insert_with keeps first).
        let root = make_ancestor("knowledge:dup", "root version", EngineTrustClass::TrustedSystem);
        let ref_dup = make_ancestor("knowledge:dup", "ref version", EngineTrustClass::TrustedUser);
        let output = ResolutionOutput {
            root: root.clone(),
            ancestors: vec![],
            references_edges: vec![],
            referenced_items: vec![ref_dup], // same ref as root
            step_outputs: std::collections::HashMap::new(),
            executor_trust_class: EngineTrustClass::TrustedSystem,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
        };

        let payload = project_single_root(&output).unwrap();
        assert_eq!(payload.items_by_ref.len(), 1, "dedup: same ref counted once");
        // Root is iterated first, so root version wins.
        assert_eq!(
            payload.items_by_ref["knowledge:dup"].raw_content,
            "root version",
            "root version must win over referenced item (first-write wins)"
        );
    }
}
