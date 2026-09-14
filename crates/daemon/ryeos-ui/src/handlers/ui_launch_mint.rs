//! `ui.launch.mint` — mint a launch token bound to a session.
//!
//! Called by the web launcher binary with a verified signed caller. Creates
//! the exact signed surface/view closure into a session-bound binding and
//! and returns a one-shot launch token + the URL the browser should open.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::LaunchContext;
use crate::compiled_binding::{
    BindingCompileContext, CompiledUiDispatchClass, CompiledUiTarget, SessionCompiledUiBinding,
};
use crate::handlers::ui_invocations_dispatch::prepare_item_ref;
use crate::state::get_ui_state;

const UI_LAUNCH_MINT_CAP: &str = "ryeos.execute.service.ui/launch/mint";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub ui_binding_contract_revision: String,
    pub surface_ref: String,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub user_principal_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub ui_binding_contract_revision: &'static str,
    pub token: String,
    pub launch_url: String,
    pub session_id: String,
}

pub(crate) struct ProjectReplacement {
    pub(crate) launch_url: String,
    pub(crate) session_id: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    require_current_binding_contract(&req.ui_binding_contract_revision)?;

    // Require a verified signed caller. Hosted principal launches bind
    // principal storage to this caller's fingerprint.
    if !ctx.is_present() || !ctx.verified {
        return Err(HandlerError::Forbidden(
            "ui.launch.mint requires verified signed caller".into(),
        )
        .into());
    }
    if !ctx
        .scopes
        .iter()
        .any(|scope| ryeos_state::capability::grant_matches(scope, UI_LAUNCH_MINT_CAP))
    {
        return Err(
            HandlerError::Forbidden(format!("{UI_LAUNCH_MINT_CAP} capability required")).into(),
        );
    }
    let project_authority = req
        .project_path
        .as_deref()
        .map(|project_path| {
            super::ui_projects::authorize_launch_project(&ctx, &state, project_path)
        })
        .transpose()?
        .map(Arc::new);

    let user_principal_id = req
        .user_principal_id
        .clone()
        .map(|principal| {
            ryeos_app::principal::principal_storage_key(&principal)
                .map_err(|err| HandlerError::BadRequest(err.to_string()))?;
            if principal != ctx.fingerprint {
                return Err(HandlerError::Forbidden(
                    "user_principal_id must match verified caller".into(),
                ));
            }
            Ok::<_, HandlerError>(principal)
        })
        .transpose()?;

    let (compiled_binding, effective_surface) =
        compile_session_binding(&req, &ctx, &state, project_authority.as_deref())?;
    let launch_ctx = LaunchContext {
        compiled_binding: std::sync::Arc::new(compiled_binding),
        effective_surface,
        granted_caps: ctx.scopes.clone(),
        user_principal_id,
        project_authority,
    };

    let (session_id, token) = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .mint_token(launch_ctx);

    let bind = &state.config.bind;
    let launch_path = launch_path_for_token(&state, &token)?;
    let launch_url = format!("http://{bind}{launch_path}");

    let response = Response {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION,
        token,
        launch_url,
        session_id,
    };

    serde_json::to_value(response).map_err(Into::into)
}

/// Mint the successor of an already authenticated UI session after an
/// authority-bearing context change. The old binding is immutable; project
/// selection therefore creates a new binding under the durable principal and
/// grants retained by the server, never under browser-supplied authority.
pub(crate) fn mint_project_replacement(
    session: &crate::browser_session::BrowserSession,
    project_path: &str,
    state: &AppState,
) -> Result<ProjectReplacement> {
    let ctx = HandlerContext::new(
        session.compiled_binding.binding.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    );
    let project_authority = Arc::new(super::ui_projects::authorize_launch_project(
        &ctx,
        state,
        project_path,
    )?);
    let req = Request {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
        surface_ref: session.surface_ref.clone(),
        project_path: Some(project_path.to_string()),
        user_principal_id: session.user_principal_id.clone(),
    };
    let (compiled_binding, effective_surface) =
        compile_session_binding(&req, &ctx, state, Some(project_authority.as_ref()))?;
    let (session_id, token) = get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .mint_replacement_token(
            &session.session_id,
            LaunchContext {
                compiled_binding: Arc::new(compiled_binding),
                effective_surface,
                granted_caps: session.granted_caps.clone(),
                user_principal_id: session.user_principal_id.clone(),
                project_authority: Some(project_authority),
            },
        );
    Ok(ProjectReplacement {
        // This response is consumed by an already loaded browser. A relative
        // one-shot path preserves its authenticated origin through reverse
        // proxies and remote node front doors; the node's listen address is
        // not browser routing authority. Native clients use `session_id`.
        launch_url: launch_path_for_token(state, &token)?,
        session_id,
    })
}

fn compile_session_binding(
    req: &Request,
    ctx: &HandlerContext,
    state: &AppState,
    project_authority: Option<&lillux::PinnedDirectory>,
) -> Result<(SessionCompiledUiBinding, Value)> {
    use ryeos_engine::canonical_ref::CanonicalRef;
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use ryeos_engine::engine::EffectiveItemRequest;

    let project_path = match project_authority {
        Some(authority) => authority.descriptor_path()?,
        None => state.config.app_root.clone(),
    };
    let canonical_project_root =
        project_authority.map(|authority| authority.path().to_string_lossy().into_owned());
    let prepared = prepare_item_ref(ctx, state, &project_path)?;
    let surface_ref = CanonicalRef::parse(&req.surface_ref)
        .map_err(|error| HandlerError::BadRequest(format!("invalid surface ref: {error}")))?;
    let project_subject = project_authority.map(|_| project_path.as_path());

    prepared
        .exec_ctx
        .engine
        .with_checked_bundle_generation(|generation| -> Result<_> {
            let mut surface = generation.effective_item(EffectiveItemRequest {
                item_ref: surface_ref,
                expected_kind: Some("surface".to_string()),
                project_root: project_authority.map(|_| project_path.clone()),
                subject_resolution_authority:
                    SubjectResolutionAuthority::for_live_project_root(project_subject),
            })?;
            let embedded = ryeos_api::surface_views::embed_effective_surface_views_in_generation(
                generation,
                project_subject,
                &mut surface,
            );
            for (view_ref, reason) in &embedded.failures {
                tracing::warn!(%view_ref, %reason, "view binding degraded during session compilation");
            }

            let binding = SessionCompiledUiBinding::compile(
                BindingCompileContext {
                    contract_revision: crate::UI_BINDING_CONTRACT_REVISION,
                    principal_id: &ctx.fingerprint,
                    caller_scopes: &ctx.scopes,
                    project_root: canonical_project_root.as_deref(),
                    node_policy_generation_digest: state.node_policy.generation_digest(),
                    identities: embedded.identity,
                },
                &surface.composed_value,
                |target_ref| compile_target(generation, &prepared, target_ref),
            )?;
            let presentation = binding.sanitize_effective_surface(&surface.composed_value)?;
            Ok((binding, presentation))
        })
}

pub(crate) fn revalidate_binding_authority(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    compiled: &SessionCompiledUiBinding,
    resolution_root: Option<&std::path::Path>,
) -> Result<()> {
    use ryeos_engine::canonical_ref::CanonicalRef;
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use ryeos_engine::engine::EffectiveItemRequest;

    let project_root = resolution_root;
    let mut surface = generation.effective_item(EffectiveItemRequest {
        item_ref: CanonicalRef::parse(&compiled.binding.surface.canonical_ref)?,
        expected_kind: Some("surface".to_string()),
        project_root: project_root.map(std::path::Path::to_path_buf),
        subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
            project_root,
        ),
    })?;
    let current = ryeos_api::surface_views::embed_effective_surface_views_in_generation(
        generation,
        project_root,
        &mut surface,
    )
    .identity;
    if current.surface != compiled.binding.surface || current.views != compiled.binding.views {
        return Err(HandlerError::Structured {
            code: "ui_binding_stale".to_string(),
            status: 409,
            body: serde_json::json!({
                "code": "ui_binding_stale",
                "error": "the effective surface/view authority changed after session mint",
                "retryable": false,
                "remediation": "mint a new UI session"
            }),
        }
        .into());
    }
    Ok(())
}

pub(crate) fn compile_target(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    prepared: &crate::handlers::ui_invocations_dispatch::PreparedInvocation,
    target_ref: &str,
) -> Result<CompiledUiTarget> {
    use ryeos_app::service_registry::{
        StandaloneStateAccess, UiDispatchMode, extract_required_caps,
        extract_standalone_state_access, extract_ui_dispatch, extract_ui_read_only,
        extract_ui_result_effect,
    };
    use ryeos_engine::canonical_ref::CanonicalRef;
    use ryeos_engine::engine::EffectiveItemRequest;

    let item_ref = CanonicalRef::parse(target_ref)
        .with_context(|| format!("invalid binding target ref `{target_ref}`"))?;
    let effective = generation.effective_item(EffectiveItemRequest {
        item_ref: item_ref.clone(),
        expected_kind: None,
        project_root: Some(prepared.project.effective_path.clone()),
        subject_resolution_authority: prepared
            .exec_ctx
            .plan_ctx
            .subject_resolution_authority
            .clone(),
    })?;
    if !effective.trusted {
        anyhow::bail!("binding target `{target_ref}` is not trusted");
    }
    let verified = generation.verify(
        &prepared.exec_ctx.plan_ctx,
        generation.resolve(&prepared.exec_ctx.plan_ctx, &item_ref)?,
    )?;
    let metadata = &verified.resolved.metadata.extra;
    let dispatch_class = match extract_ui_dispatch(metadata)? {
        UiDispatchMode::Verified => CompiledUiDispatchClass::Verified,
        UiDispatchMode::SessionLocal => CompiledUiDispatchClass::SessionLocal,
    };
    let result_effect = extract_ui_result_effect(metadata)?.map(|effect| match effect {
        ryeos_app::service_registry::UiResultEffect::ReplaceSession => {
            crate::compiled_binding::CompiledUiResultEffect::ReplaceSession
        }
    });
    let declared_source_safe = extract_ui_read_only(metadata)?;
    let source_safe = match dispatch_class {
        CompiledUiDispatchClass::Verified => {
            declared_source_safe
                && extract_standalone_state_access(metadata)?
                    == StandaloneStateAccess::ReadOnlyExisting
        }
        CompiledUiDispatchClass::SessionLocal => declared_source_safe,
    };
    let parameter_schema = metadata
        .get("schema")
        .and_then(Value::as_object)
        .map(|schema| {
            schema
                .iter()
                .map(|(name, value)| {
                    Ok((
                        name.clone(),
                        value
                            .as_str()
                            .with_context(|| format!("target schema `{name}` is not a string"))?
                            .to_string(),
                    ))
                })
                .collect::<Result<std::collections::BTreeMap<_, _>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(CompiledUiTarget {
        identity: ryeos_api::surface_views::EffectiveUiItemIdentity {
            canonical_ref: effective.canonical_ref,
            effective_definition_digest: effective.effective_definition_digest,
            effective_trust_class: effective.trust_class,
        },
        required_caps: extract_required_caps(metadata),
        source_safe,
        dispatch_class,
        result_effect,
        parameter_schema,
    })
}

fn require_current_binding_contract(advertised: &str) -> std::result::Result<(), HandlerError> {
    if advertised != crate::UI_BINDING_CONTRACT_REVISION {
        return Err(HandlerError::BadRequest(format!(
            "UI binding contract mismatch: launcher advertised '{}', daemon requires '{}'",
            advertised,
            crate::UI_BINDING_CONTRACT_REVISION
        )));
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/launch/mint",
    endpoint: "ui.launch.mint",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[UI_LAUNCH_MINT_CAP],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = ryeos_app::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

fn launch_path_for_token(state: &AppState, token: &str) -> Result<String> {
    launch_path_from_routes(&state.node_config.routes, token)
}

fn launch_path_from_routes(
    routes: &[ryeos_app::route_raw::RawRouteSpec],
    token: &str,
) -> Result<String> {
    let route = routes
        .iter()
        .find(|route| {
            route.response.source.as_deref() == Some(super::ui_launch::DESCRIPTOR.service_ref)
        })
        .context("no route configured for service:ui/launch")?;

    let token_template = route
        .response
        .source_config
        .get("token")
        .and_then(|value| value.as_str())
        .context("ui.launch route source_config.token must reference a path capture")?;

    let capture = path_capture_name(token_template)
        .context("ui.launch route source_config.token must be ${path.<name>}")?;
    let placeholder = format!("{{{capture}}}");
    if !route.path.contains(&placeholder) {
        anyhow::bail!(
            "ui.launch route path '{}' does not declare token capture '{}'; source_config.token = '{}'",
            route.path,
            capture,
            token_template
        );
    }

    Ok(route.path.replace(&placeholder, token))
}

fn path_capture_name(template: &str) -> Option<&str> {
    let rest = template.trim().strip_prefix("${path.")?;
    rest.strip_suffix('}')
}

#[cfg(test)]
mod tests {
    use ryeos_app::route_raw::{
        RawLimits, RawRequest, RawRequestBody, RawResponseSpec, RawRouteSpec,
    };

    use super::*;

    fn make_launch_route(path: &str, token_template: &str) -> RawRouteSpec {
        RawRouteSpec {
            id: "ui/launch".into(),
            path: path.into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "none".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "json".into(),
                source: Some(super::super::ui_launch::DESCRIPTOR.service_ref.into()),
                source_config: serde_json::json!({ "token": token_template }),
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/ui_launch.yaml"),
        }
    }

    #[test]
    fn launch_path_is_rendered_from_route_snapshot() {
        let routes = vec![make_launch_route(
            "/custom/launch/{secret}",
            "${path.secret}",
        )];

        let path = launch_path_from_routes(&routes, "abc-123").unwrap();
        assert_eq!(path, "/custom/launch/abc-123");
    }

    #[test]
    fn launch_path_rejects_route_without_declared_capture() {
        let routes = vec![make_launch_route(
            "/custom/launch/{other}",
            "${path.secret}",
        )];

        let err =
            launch_path_from_routes(&routes, "abc-123").expect_err("route mismatch must fail");
        assert!(err.to_string().contains("does not declare token capture"));
    }

    #[test]
    fn launch_mint_has_no_caller_authored_posture() {
        let req: Request = serde_json::from_value(serde_json::json!({
            "ui_binding_contract_revision": crate::UI_BINDING_CONTRACT_REVISION,
            "surface_ref": "surface:ryeos/ui/base"
        }))
        .unwrap();

        assert_eq!(req.surface_ref, "surface:ryeos/ui/base");
        for forbidden in [
            serde_json::json!({
                "ui_binding_contract_revision": crate::UI_BINDING_CONTRACT_REVISION,
                "surface_ref": "surface:ryeos/ui/base",
                "read_only": true
            }),
            serde_json::json!({
                "ui_binding_contract_revision": crate::UI_BINDING_CONTRACT_REVISION,
                "surface_ref": "surface:ryeos/ui/base",
                "mode": "operate"
            }),
        ] {
            assert!(serde_json::from_value::<Request>(forbidden).is_err());
        }
    }

    #[test]
    fn launch_mint_requires_the_exact_binding_contract_revision() {
        let missing = serde_json::from_value::<Request>(serde_json::json!({
            "surface_ref": "surface:ryeos/ui/base"
        }));
        assert!(missing.is_err(), "old launchers must fail request decoding");

        let current: Request = serde_json::from_value(serde_json::json!({
            "ui_binding_contract_revision": crate::UI_BINDING_CONTRACT_REVISION,
            "surface_ref": "surface:ryeos/ui/base"
        }))
        .unwrap();
        assert_eq!(
            current.ui_binding_contract_revision,
            crate::UI_BINDING_CONTRACT_REVISION
        );
        assert!(require_current_binding_contract("ryeos.ui.binding.v2").is_err());
        assert!(require_current_binding_contract(crate::UI_BINDING_CONTRACT_REVISION).is_ok());
    }
}
