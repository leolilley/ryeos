//! `ui.launch.mint` — mint a launch token bound to a session.
//!
//! Called by the web launcher binary with a verified signed caller. Creates
//! a session record with full context (surface_ref, project_path, read_only)
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
use crate::state::get_ui_state;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub surface_ref: String,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default = "default_read_only")]
    pub read_only: bool,
    #[serde(default)]
    pub user_principal_id: Option<String>,
}

fn default_read_only() -> bool {
    true
}

fn launch_session_caps() -> Vec<String> {
    vec!["ui.read".into()]
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub token: String,
    pub launch_url: String,
    pub session_id: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // Require a verified signed caller. Hosted principal launches bind
    // principal storage to this caller's fingerprint.
    if !ctx.is_present() {
        return Err(HandlerError::Forbidden(
            "ui.launch.mint requires verified signed caller".into(),
        )
        .into());
    }

    let user_principal_id = req
        .user_principal_id
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

    let launch_ctx = LaunchContext {
        surface_ref: req.surface_ref,
        project_path: req.project_path,
        read_only: req.read_only,
        granted_caps: launch_session_caps(),
        user_principal_id,
    };

    let (session_id, token) = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .mint_token(launch_ctx);

    let bind = &state.config.bind;
    let launch_path = launch_path_for_token(&state, &token)?;
    let launch_url = format!("http://{bind}{launch_path}");

    let response = Response {
        token,
        launch_url,
        session_id,
    };

    serde_json::to_value(response).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/launch/mint",
    endpoint: "ui.launch.mint",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
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
    fn launch_mint_request_defaults_to_read_only() {
        let req: Request = serde_json::from_value(serde_json::json!({
            "surface_ref": "surface:ryeos/ui/base"
        }))
        .unwrap();

        assert!(req.read_only);
    }

    #[test]
    fn launch_sessions_only_grant_ui_read() {
        // `ui.invocations.dispatch` may execute a canonical item ref using a durable
        // session principal, so browser launch sessions must not silently grow
        // execution capabilities. Surface affordances remain the UI contract;
        // this is the daemon-side backstop that keeps cookie-authenticated
        // sessions read-only at the executor cap gate.
        assert_eq!(launch_session_caps(), vec!["ui.read".to_string()]);
    }
}
