//! Compiled dispatch invoker — synchronous waited dispatch for canonical refs
//! whose admitted kind schema selects engine execution rather than an
//! in-process registry.
//!
//! Unlike the service invoker (in-process handler call) or the launch/gateway
//! invokers (background task), this calls `dispatch::dispatch` synchronously
//! and returns the result value directly as `RouteInvocationResult::Json`.
//!
//! Input contract: `ctx.input` must be a JSON object with:
//! - `project_path` (string, required) — absolute path for engine resolution
//! - `parameters` (object, optional) — business params passed to dispatch
//!
//! This separates execution metadata from business parameters so route
//! authors see a clear, typed structure in source_config.

use serde::Deserialize;

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContext, RouteInvocationContract,
    RouteInvocationOutput, RouteInvocationResult,
};

/// Typed input shape for engine-dispatched route sources.
///
/// Parsed from the interpolated `source_config`. `project_path` is required;
/// `parameters` defaults to an empty object.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DispatchSourceConfig {
    /// Absolute path to the project root for engine resolution.
    pub project_path: String,
    pub ref_bindings: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub product_selections:
        ryeos_state::external_content::products::composition::ProductSelectionInputs,
    /// Business parameters forwarded to `dispatch::dispatch` as `params`.
    #[serde(default)]
    pub parameters: serde_json::Value,
}

/// Synchronous invoker for an engine-dispatched canonical source.
///
/// At compile time the `item_ref` is stored. At runtime the invoker parses
/// a typed `DispatchSourceConfig` from `ctx.input`, constructs a
/// `DispatchRequest` and `ExecutionContext`, and calls `dispatch::dispatch`
/// by waiting until execution completes.
#[derive(Debug, Clone)]
pub enum DispatchAuthority {
    CallerPrincipal,
    FixedPrincipal {
        fingerprint: String,
        scopes: Vec<String>,
    },
}

pub struct CompiledDispatchInvoker {
    /// The canonical ref to dispatch (e.g., `"directive:webhooks/ingest"`).
    pub item_ref: String,
    /// Authority used for engine plan/build and execution checks.
    pub authority: DispatchAuthority,
}

/// Exact item a fixed-principal route admitted before dispatch: the canonical
/// on-disk file it resolved to and that file's content digests. Dispatch runs
/// this item or nothing.
#[derive(Debug, Clone)]
pub(crate) struct PinnedDispatchSubject {
    pub source_path: std::path::PathBuf,
    pub content_hash: String,
    pub raw_content_digest: String,
}

impl PinnedDispatchSubject {
    /// Pin a resolved item by its canonical file and content digests.
    pub(crate) fn from_resolved(
        resolved: &ryeos_engine::contracts::ResolvedItem,
    ) -> Result<Self, String> {
        let source_path = resolved.source_path.canonicalize().map_err(|error| {
            format!(
                "canonicalize resolved source {}: {error}",
                resolved.source_path.display()
            )
        })?;
        Ok(Self {
            source_path,
            content_hash: resolved.content_hash.clone(),
            raw_content_digest: resolved.raw_content_digest.clone(),
        })
    }

    fn matches(&self, resolved: &ryeos_engine::contracts::ResolvedItem) -> bool {
        self.matches_parts(
            &resolved.source_path,
            &resolved.content_hash,
            &resolved.raw_content_digest,
        )
    }

    fn matches_parts(
        &self,
        source_path: &std::path::Path,
        content_hash: &str,
        raw_content_digest: &str,
    ) -> bool {
        content_hash == self.content_hash
            && raw_content_digest == self.raw_content_digest
            && source_path
                .canonicalize()
                .is_ok_and(|path| path == self.source_path)
    }
}

/// Per-request confinement for fixed-principal dispatch: the item the route
/// admitted and the one canonical workspace the fixed principal's live
/// project authority is bound to. The workspace is the execution's project
/// (resolution, state and callback root); the executor may still place the
/// item's process in a narrower daemon-owned scratch. That scratch keeps the
/// process away from the workspace only under enforced isolation; under
/// direct-host or disabled isolation the process has the node account's file
/// access and can reach the workspace and the node's installed bundles. This
/// confinement governs the authority dispatch grants (the item that runs and
/// the root its callbacks write through), not the process's file access.
///
/// Dispatch resolves in the engine's normal registered order, exactly like
/// any other run; it is never given a reordered or restricted engine. When
/// that order selects anything other than the pinned item (for example a
/// same-named item an earlier-sorting bundle ships), dispatch refuses rather
/// than run it. Admitting the pinned item through a bundle-restricted
/// attestation that the normal-order dispatch could consume is not
/// implemented.
#[derive(Debug, Clone)]
pub(crate) struct FixedPrincipalConfinement {
    pub subject: PinnedDispatchSubject,
    pub workspace: std::path::PathBuf,
}

/// A dispatch whose input, authority, project and (for a fixed principal)
/// admitted root have all been checked; only the run itself remains.
struct PreparedDispatch {
    item_ref: crate::routes::parsed_ref::ParsedItemRef,
    config: DispatchSourceConfig,
    principal_id: String,
    effective_path: std::path::PathBuf,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    exec_ctx: ryeos_executor::executor::ExecutionContext,
    root_admission: Option<ryeos_app::thread_lifecycle::RootExecutionAdmission>,
    root_dispatch_evidence: Option<ryeos_executor::dispatch::RootDispatchEvidence>,
}

static DISPATCH_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Json,
    principal: PrincipalPolicy::Optional,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledDispatchInvoker {
    fn contract(&self) -> &'static RouteInvocationContract {
        &DISPATCH_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        self.invoke_with(ctx, None).await
    }
}

impl CompiledDispatchInvoker {
    /// Fixed-principal dispatch: run exactly `confinement.subject` with live
    /// project authority bound to `confinement.workspace` and nothing else.
    pub(crate) async fn invoke_confined(
        &self,
        ctx: RouteInvocationContext,
        confinement: FixedPrincipalConfinement,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        self.invoke_with(ctx, Some(confinement)).await
    }

    /// Everything dispatch needs, checked and sealed before the item runs.
    async fn prepare(
        &self,
        ctx: &RouteInvocationContext,
        confinement: Option<&FixedPrincipalConfinement>,
    ) -> Result<PreparedDispatch, RouteDispatchError> {
        use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};

        // A fixed route principal carries a live-write grant that is only
        // sound inside one request-owned workspace and for one admitted item,
        // so it never runs without that confinement; caller-principal
        // dispatch takes neither.
        match (&self.authority, confinement.is_some()) {
            (DispatchAuthority::CallerPrincipal, false)
            | (
                DispatchAuthority::FixedPrincipal {
                    fingerprint: _,
                    scopes: _,
                },
                true,
            ) => {}
            (DispatchAuthority::CallerPrincipal, true) => {
                return Err(RouteDispatchError::Internal(format!(
                    "dispatch invoker for '{}': caller-principal dispatch does not take a \
                     fixed-principal confinement",
                    self.item_ref
                )));
            }
            (
                DispatchAuthority::FixedPrincipal {
                    fingerprint: _,
                    scopes: _,
                },
                false,
            ) => {
                return Err(RouteDispatchError::Internal(format!(
                    "dispatch invoker for '{}': fixed-principal dispatch requires its admitted \
                     item and request workspace",
                    self.item_ref
                )));
            }
        }

        let item_ref =
            crate::routes::parsed_ref::ParsedItemRef::parse(&self.item_ref).map_err(|e| {
                RouteDispatchError::Internal(format!(
                    "dispatch invoker: invalid item_ref '{}': {}",
                    self.item_ref, e
                ))
            })?;

        // Parse typed dispatch config — project_path is required, no fallback.
        let config: DispatchSourceConfig =
            serde_json::from_value(ctx.input.clone()).map_err(|e| {
                RouteDispatchError::BadRequest(format!(
                    "engine dispatch requires {{ project_path, parameters }} in source_config: {e}"
                ))
            })?;

        let project_path = std::path::PathBuf::from(&config.project_path);
        if let Some(confinement) = confinement {
            require_confined_path(
                "dispatch project_path",
                &project_path,
                &confinement.workspace,
                &self.item_ref,
            )?;
        }

        let (principal_id, principal_scopes, authenticated_origin_site_id) = match &self.authority {
            DispatchAuthority::CallerPrincipal => {
                let principal_id = ctx
                    .principal
                    .as_ref()
                    .map(|p| p.id.clone())
                    .unwrap_or_default();

                let principal_scopes = ctx
                    .principal
                    .as_ref()
                    .map(|p| p.scopes.clone())
                    .unwrap_or_default();

                let origin = crate::routes::invocation::authenticated_execution_origin(
                    ctx.principal.as_ref(),
                    ctx.state.threads.site_id(),
                );
                (principal_id, principal_scopes, Some(origin))
            }
            DispatchAuthority::FixedPrincipal {
                fingerprint,
                scopes,
            } => (fingerprint.clone(), scopes.clone(), None),
        };

        ryeos_executor::execution::launch_preparation::validate_ref_bindings(&config.ref_bindings)
            .map_err(|error| {
                RouteDispatchError::BadRequest(format!("invalid ref_bindings: {error}"))
            })?;
        ryeos_state::external_content::products::composition::validate_product_selection_inputs(
            &config.product_selections,
        )
        .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;
        if !config.product_selections.is_empty()
            && (!matches!(self.authority, DispatchAuthority::CallerPrincipal)
                || ctx.principal.is_none())
        {
            return Err(RouteDispatchError::BadRequest(
                "product selectors require a configured local caller authority".to_string(),
            ));
        }
        for (name, bound_ref) in &config.ref_bindings {
            let canonical =
                ryeos_engine::canonical_ref::CanonicalRef::parse(bound_ref).map_err(|error| {
                    RouteDispatchError::BadRequest(format!("invalid ref_bindings.{name}: {error}"))
                })?;
            let required = ryeos_runtime::authorizer::canonical_cap(
                &canonical.kind,
                &canonical.bare_id,
                "execute",
            );
            let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(&required);
            ctx.state
                .authorizer
                .authorize(&principal_scopes, &policy)
                .map_err(|_| {
                    RouteDispatchError::Forbidden(format!(
                        "missing required capability for ref binding '{name}': {required}"
                    ))
                })?;
        }

        ryeos_app::execution_policy::authorize_standard_local_live_execution(&principal_scopes)
            .map_err(|error| RouteDispatchError::Forbidden(error.to_string()))?;
        let site_id = ctx.state.threads.site_id().to_string();
        let origin_site_id = authenticated_origin_site_id.unwrap_or_else(|| site_id.clone());
        let project_ctx = ryeos_executor::execution::project_source::resolve_project_context(
            &ctx.state,
            &ryeos_executor::execution::project_source::ProjectSource::LiveFs,
            &project_path,
            &principal_id,
            &format!("dispatch-{}", ryeos_app::thread_lifecycle::new_thread_id()),
            None,
        )
        .map_err(|error| {
            RouteDispatchError::BadRequest(format!("capture dispatch project: {error}"))
        })?;

        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: principal_id.clone(),
                scopes: principal_scopes.clone(),
            }),
            project_context: ProjectContext::LocalPath {
                path: project_ctx.effective_path.clone(),
            },
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs,
            current_site_id: site_id.clone(),
            origin_site_id,
            execution_hints: Default::default(),
            scheduled_fire: None,
            validate_only: false,
        };

        let resolved_authority =
            ryeos_app::execution_policy::resolve_standard_local_live_authority(
                &project_ctx.effective_path,
                principal_scopes.clone(),
                &ctx.state.isolation,
            )
            .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;
        if let Some(confinement) = confinement {
            // Callbacks (snapshot create, item authoring, child spawn) take
            // their project root and write ceiling from this sealed record,
            // never from the caller scopes, so binding the record to the
            // workspace confines the live-write grant to it.
            require_confined_path(
                "dispatch effective project",
                &project_ctx.effective_path,
                &confinement.workspace,
                &self.item_ref,
            )?;
            let write_root = resolved_authority
                .project
                .authorized_live_write_root("project")
                .map_err(|error| {
                    RouteDispatchError::Internal(format!(
                        "dispatch invoker for '{}': fixed-principal project authority grants no \
                         live write root: {error}",
                        self.item_ref
                    ))
                })?;
            require_confined_path(
                "live project write root",
                write_root,
                &confinement.workspace,
                &self.item_ref,
            )?;
        }
        let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            project_ctx.effective_path.clone(),
            project_ctx.request_engine.clone(),
            resolved_authority.project,
        )
        .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;
        let provenance = match (confinement, ctx.workspace_lifeline.clone()) {
            (Some(_), Some(lifeline)) => {
                if !lifeline.owns_effective_path(provenance.effective_path()) {
                    return Err(RouteDispatchError::Internal(format!(
                        "dispatch invoker for '{}': request workspace guard does not own {}",
                        self.item_ref,
                        provenance.effective_path().display()
                    )));
                }
                provenance.with_workspace_lifeline(Some(lifeline))
            }
            (Some(_), None) => {
                return Err(RouteDispatchError::Internal(format!(
                    "dispatch invoker for '{}': fixed-principal dispatch has no request \
                     workspace guard",
                    self.item_ref
                )));
            }
            (None, _) => provenance,
        };
        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: principal_id.clone(),
            caller_scopes: principal_scopes,
            engine: project_ctx.request_engine.clone(),
            plan_ctx,
            requested_call: None,
        };

        let (root_admission, root_dispatch_evidence) = match confinement {
            Some(confinement) => {
                let (admission, evidence) = admit_pinned_subject(
                    &self.item_ref,
                    item_ref.kind(),
                    &config,
                    &provenance,
                    &exec_ctx,
                    &ctx.state,
                    &confinement.subject,
                )
                .await?;
                (Some(admission), Some(evidence))
            }
            None => (None, None),
        };

        Ok(PreparedDispatch {
            item_ref,
            config,
            principal_id,
            effective_path: project_ctx.effective_path,
            provenance,
            lifecycle_authority: resolved_authority.lifecycle,
            exec_ctx,
            root_admission,
            root_dispatch_evidence,
        })
    }

    async fn invoke_with(
        &self,
        ctx: RouteInvocationContext,
        confinement: Option<FixedPrincipalConfinement>,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let prepared = self
            .prepare(&ctx, confinement.as_ref())
            .await
            .map_err(|error| unprepared_refusal(&self.item_ref, error, confinement.is_some()))?;
        let PreparedDispatch {
            item_ref,
            config,
            principal_id,
            effective_path,
            provenance,
            lifecycle_authority,
            exec_ctx,
            root_admission,
            root_dispatch_evidence,
        } = prepared;

        let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
            launch_mode: "wait",
            target_site_id: None,
            validate_only: false,
            params: config.parameters,
            ref_bindings: config.ref_bindings,
            product_selections: config.product_selections,
            acting_principal: principal_id.as_str(),
            project_path: &effective_path,
            provenance,
            lifecycle_authority,
            launch_timings: None,
            original_root_kind: item_ref.kind(),
            pre_minted_thread_id: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            previous_thread_id: None,
            root_admission,
            root_dispatch_evidence,
            parent_execution_context: None,
            effect_authority: None,
        };

        // The handler context handed to dispatch must be the authority the
        // request was sealed under. Under caller-principal authority that is
        // the transport-authenticated caller; under a fixed route principal
        // the route itself is the principal and the caller's context (if any)
        // would name a different fingerprint, so none is passed.
        let caller_context = match &self.authority {
            DispatchAuthority::CallerPrincipal => ctx
                .principal
                .as_ref()
                .map(|principal| principal.handler_context()),
            DispatchAuthority::FixedPrincipal {
                fingerprint: _,
                scopes: _,
            } => None,
        };
        let result = match caller_context {
            Some(handler_context) => {
                ryeos_executor::dispatch::dispatch_with_handler_context(
                    item_ref.as_str(),
                    handler_context,
                    &dispatch_req,
                    &exec_ctx,
                    &ctx.state,
                )
                .await
            }
            None => {
                ryeos_executor::dispatch::dispatch(
                    item_ref.as_str(),
                    &dispatch_req,
                    &exec_ctx,
                    &ctx.state,
                )
                .await
            }
        }
        .map_err(|e| RouteDispatchError::Internal(format!("dispatch failed: {e}")))?;

        Ok(RouteInvocationResult::Json {
            value: result,
            thread_id: None,
        })
    }
}

/// Refuse unless `path` canonicalizes to exactly the confined `workspace`.
fn require_confined_path(
    what: &str,
    path: &std::path::Path,
    workspace: &std::path::Path,
    item_ref: &str,
) -> Result<(), RouteDispatchError> {
    let canonical = path.canonicalize().map_err(|error| {
        RouteDispatchError::Internal(format!(
            "dispatch invoker for '{item_ref}': canonicalize {what} {}: {error}",
            path.display()
        ))
    })?;
    if canonical != workspace {
        tracing::warn!(
            item_ref,
            what,
            path = %canonical.display(),
            workspace = %workspace.display(),
            "fixed-principal dispatch refused: path is outside the request workspace"
        );
        return Err(fixed_principal_refusal());
    }
    Ok(())
}

/// The error a dispatch that could not be prepared returns. A
/// fixed-principal route's caller may be unauthenticated, and the reasons a
/// preparation fails name host paths whatever their variant: a bad request
/// such as a project that cannot be captured or an authority that cannot be
/// resolved for the request workspace, as much as an unverifiable or
/// shadowing item or a workspace that cannot be sealed. Every such reason is
/// logged and the body is the fixed refusal. A caller-principal dispatch is
/// authenticated and named its own project, so its error passes unchanged.
fn unprepared_refusal(
    item_ref: &str,
    error: RouteDispatchError,
    fixed_principal: bool,
) -> RouteDispatchError {
    if !fixed_principal {
        return error;
    }
    tracing::warn!(
        item_ref,
        %error,
        "fixed-principal dispatch refused before running"
    );
    fixed_principal_refusal()
}

/// The body a fixed-principal route returns when it refuses to run. The
/// caller may be unauthenticated, so the body names no path, digest or item;
/// the detailed reason goes to the daemon log.
pub(crate) const FIXED_PRINCIPAL_REFUSAL: &str =
    "handler route refused: the route's tool could not be admitted";

fn fixed_principal_refusal() -> RouteDispatchError {
    RouteDispatchError::Forbidden(FIXED_PRINCIPAL_REFUSAL.to_owned())
}

/// Admit the root item under the exact dispatch context and require it to be
/// the pinned item. Dispatch then consumes this admission for its first hop
/// instead of resolving the ref again, so the item that runs is the item that
/// was compared here.
async fn admit_pinned_subject(
    item_ref: &str,
    item_kind: &str,
    config: &DispatchSourceConfig,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    exec_ctx: &ryeos_executor::executor::ExecutionContext,
    state: &ryeos_app::state::AppState,
    pinned: &PinnedDispatchSubject,
) -> Result<
    (
        ryeos_app::thread_lifecycle::RootExecutionAdmission,
        ryeos_executor::dispatch::RootDispatchEvidence,
    ),
    RouteDispatchError,
> {
    let project_binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
        &exec_ctx.engine,
        &exec_ctx.plan_ctx,
        provenance,
    )
    .map_err(|error| {
        RouteDispatchError::Internal(format!(
            "dispatch invoker for '{item_ref}': seal handler project binding: {error:#}"
        ))
    })?;
    let preflight_item_ref = item_ref.to_owned();
    let preflight_kind = item_kind.to_owned();
    let preflight_parameters = config.parameters.clone();
    let preflight_ref_bindings = config.ref_bindings.clone();
    let preflight_product_selections = config.product_selections.clone();
    let preflight_exec_ctx = exec_ctx.clone();
    let preflight_state = state.clone();
    let preflight = tokio::task::spawn_blocking(move || {
        ryeos_executor::dispatch::preflight_root_dispatch(
            &preflight_item_ref,
            &preflight_kind,
            &preflight_parameters,
            &preflight_ref_bindings,
            &preflight_product_selections,
            None,
            None,
            &project_binding,
            &preflight_exec_ctx,
            &preflight_state,
            None,
        )
    })
    .await
    .map_err(|error| {
        RouteDispatchError::Internal(format!(
            "dispatch invoker for '{item_ref}': admission worker failed: {error}"
        ))
    })?
    .map_err(|error| {
        RouteDispatchError::Internal(format!(
            "dispatch invoker for '{item_ref}': admission failed: {error}"
        ))
    })?;

    let admission = preflight.root_admission.ok_or_else(|| {
        RouteDispatchError::Internal(format!(
            "dispatch invoker for '{item_ref}': admission returned no admitted root"
        ))
    })?;
    // Dispatch reuses the admitted root for its first hop only when the
    // admitted root is the requested ref; otherwise it would resolve again.
    for (role, resolved) in [
        ("requested item", &preflight.requested_subject.resolved),
        ("admitted root", &admission.verified_subject().resolved),
    ] {
        if resolved.canonical_ref.to_string() != item_ref {
            return Err(RouteDispatchError::Internal(format!(
                "dispatch invoker for '{item_ref}': admission named '{}' as the {role}",
                resolved.canonical_ref
            )));
        }
        if !pinned.matches(resolved) {
            tracing::warn!(
                item_ref,
                role,
                resolved_path = %resolved.source_path.display(),
                resolved_content = %resolved.content_hash,
                pinned_path = %pinned.source_path.display(),
                pinned_content = %pinned.content_hash,
                "fixed-principal dispatch refused: dispatch resolved a different item than the \
                 one the route admitted"
            );
            return Err(fixed_principal_refusal());
        }
    }
    Ok((admission, preflight.root_dispatch_evidence))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned_at(path: &std::path::Path) -> PinnedDispatchSubject {
        PinnedDispatchSubject {
            source_path: path.canonicalize().unwrap(),
            content_hash: "content".to_string(),
            raw_content_digest: "raw".to_string(),
        }
    }

    #[test]
    fn handler_pinned_subject_accepts_the_same_file_and_digests() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("callback.py");
        std::fs::write(&tool, "print()").unwrap();
        let pinned = pinned_at(&tool);
        assert!(pinned.matches_parts(&tool, "content", "raw"));
    }

    #[test]
    fn handler_pinned_subject_refuses_a_same_content_shadow_at_another_path() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_tool = dir.path().join("bundle.py");
        let user_tool = dir.path().join("user.py");
        std::fs::write(&bundle_tool, "print()").unwrap();
        std::fs::write(&user_tool, "print()").unwrap();
        let pinned = pinned_at(&bundle_tool);
        assert!(!pinned.matches_parts(&user_tool, "content", "raw"));
    }

    #[test]
    fn handler_pinned_subject_refuses_changed_content_at_the_pinned_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("callback.py");
        std::fs::write(&tool, "print()").unwrap();
        let pinned = pinned_at(&tool);
        assert!(!pinned.matches_parts(&tool, "other", "raw"));
        assert!(!pinned.matches_parts(&tool, "content", "other"));
    }

    #[test]
    fn handler_pinned_subject_refuses_a_vanished_source() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("callback.py");
        std::fs::write(&tool, "print()").unwrap();
        let pinned = pinned_at(&tool);
        std::fs::remove_file(&tool).unwrap();
        assert!(!pinned.matches_parts(&tool, "content", "raw"));
    }

    #[test]
    fn handler_unprepared_bad_request_names_no_host_path_on_a_fixed_principal_route() {
        let host_path = "/srv/node/state/tmp/ryeos-route-handler-1-abc";
        for error in [
            RouteDispatchError::BadRequest(format!(
                "capture dispatch project: canonicalize {host_path}: no such file"
            )),
            RouteDispatchError::BadRequest(format!(
                "live project authority for {host_path} is not available"
            )),
            RouteDispatchError::Internal(format!("canonicalize {host_path}: denied")),
        ] {
            match unprepared_refusal("tool:b/t", error, true) {
                RouteDispatchError::Forbidden(message) => {
                    assert_eq!(message, FIXED_PRINCIPAL_REFUSAL);
                    assert!(!message.contains(host_path), "{message}");
                }
                other => panic!("expected the fixed refusal, got {other:?}"),
            }
        }
    }

    #[test]
    fn handler_unprepared_caller_principal_error_passes_unchanged() {
        let message = "capture dispatch project: canonicalize /home/me/project: no such file";
        match unprepared_refusal(
            "tool:b/t",
            RouteDispatchError::BadRequest(message.to_owned()),
            false,
        ) {
            RouteDispatchError::BadRequest(passed) => assert_eq!(passed, message),
            other => panic!("expected the caller's own bad request, got {other:?}"),
        }
    }

    #[test]
    fn handler_confined_path_accepts_only_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let other = dir.path().join("other");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&other).unwrap();
        let workspace = workspace.canonicalize().unwrap();

        require_confined_path("project", &workspace, &workspace, "tool:b/t").unwrap();
        match require_confined_path("project", &other, &workspace, "tool:b/t") {
            Err(RouteDispatchError::Forbidden(message)) => {
                assert_eq!(message, FIXED_PRINCIPAL_REFUSAL);
                assert!(!message.contains(&*other.to_string_lossy()), "{message}");
            }
            other => panic!("expected a forbidden refusal, got {other:?}"),
        }
        match require_confined_path("project", &workspace.join("nested"), &workspace, "tool:b/t") {
            Err(RouteDispatchError::Internal(_)) => {}
            other => panic!("expected an unresolvable-path refusal, got {other:?}"),
        }
    }

    #[test]
    fn handler_confined_path_refuses_a_symlink_into_another_directory() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let other = dir.path().join("other");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&other).unwrap();
        let link = dir.path().join("link");
        lillux::PinnedDirectory::open(dir.path())
            .unwrap()
            .unwrap()
            .create_symlink(
                std::ffi::OsStr::new("link"),
                other.as_os_str().as_encoded_bytes(),
            )
            .unwrap();
        let workspace = workspace.canonicalize().unwrap();
        assert!(require_confined_path("project", &link, &workspace, "tool:b/t").is_err());
    }
}
