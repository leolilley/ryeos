//! `handler` response mode — fixed route target owns the HTTP response.
//!
//! This mode is for bundle-declared public HTTP endpoints such as webhooks,
//! tracking pixels, redirects, and small HTML/text responses. It differs from
//! `execute` because the target is fixed in the signed route descriptor, and it
//! differs from `json` because the target result is interpreted as an HTTP
//! response envelope instead of always being framed as JSON.
//!
//! Route YAML supplies execution target via `response.source` and request/result
//! mapping via `response.source_config`. It must not supply `project_path`,
//! `item_ref`, or `parameters`; those are generic execution concepts, not route
//! handler identity.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::route_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use crate::routes::invocation::{
    attach_recorded_thread_header, InvocationCheck, RouteInvocationContext, RouteInvocationOutput,
    RouteInvocationResult,
};
use crate::routes::invokers::dispatch_invocation::{CompiledDispatchInvoker, DispatchAuthority};
use ryeos_app::route_raw::{RawRequestBody, RawRouteSpec};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};

pub struct HandlerMode;

pub struct CompiledHandlerMode {
    source_ref: String,
    project_root: PathBuf,
    request_config: HandlerRequestConfig,
    result_config: HandlerResultConfig,
    request_body_mode: RawRequestBody,
    execution_principal_id: String,
    execution_scopes: Vec<String>,
    invoker: Arc<CompiledDispatchInvoker>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HandlerSourceConfig {
    #[serde(default)]
    request: HandlerRequestConfig,
    #[serde(default)]
    result: HandlerResultConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HandlerRequestConfig {
    #[serde(default)]
    query: bool,
    #[serde(default)]
    path_params: bool,
    #[serde(default)]
    body: bool,
    #[serde(default)]
    headers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandlerResultConfig {
    #[serde(default = "default_envelope_field")]
    envelope_field: String,
    #[serde(default = "default_response_bytes_max")]
    response_bytes_max: usize,
}

impl Default for HandlerResultConfig {
    fn default() -> Self {
        Self {
            envelope_field: default_envelope_field(),
            response_bytes_max: default_response_bytes_max(),
        }
    }
}

fn default_envelope_field() -> String {
    "response".to_string()
}

fn default_response_bytes_max() -> usize {
    1_048_576
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpResponseEnvelope {
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    body_base64: Option<String>,
    #[serde(default)]
    json: Option<Value>,
}

#[derive(Debug, Serialize)]
struct RouteEnvelope {
    route: RouteEnvelopeRoute,
    request: RequestEnvelope,
    principal: PrincipalEnvelope,
}

#[derive(Debug, Serialize)]
struct RouteEnvelopeRoute {
    id: String,
}

#[derive(Debug, Serialize)]
struct RequestEnvelope {
    method: String,
    path: String,
    uri: String,
    raw_query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_params: Option<BTreeMap<String, String>>,
    headers: BTreeMap<String, String>,
    body: BodyEnvelope,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BodyEnvelope {
    None,
    Json { json: Value },
    Text { text: String },
    Base64 { base64: String },
}

#[derive(Debug, Serialize)]
struct PrincipalEnvelope {
    id: String,
    verified: bool,
    verifier: &'static str,
    metadata: BTreeMap<String, String>,
}

impl ResponseMode for HandlerMode {
    fn key(&self) -> &'static str {
        "handler"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: self.key().into(),
                reason: "handler mode must not have a top-level 'execute' block".into(),
            });
        }
        if raw.response.status.is_some()
            || raw.response.content_type.is_some()
            || raw.response.body_b64.is_some()
        {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: self.key().into(),
                reason: "handler mode must not set static-mode fields \
                    (status / content_type / body_b64); handler output comes from \
                    the target's response envelope"
                    .into(),
            });
        }

        let source_ref = raw.response.source.as_deref().ok_or_else(|| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: self.key().into(),
                reason: "handler mode requires `response.source`".into(),
            }
        })?;
        let source_bundle_id = validate_handler_source(source_ref, &raw.id)?;

        let config: HandlerSourceConfig = if raw.response.source_config.is_null() {
            HandlerSourceConfig::default()
        } else {
            serde_json::from_value(raw.response.source_config.clone()).map_err(|err| {
                RouteConfigError::InvalidSourceConfig {
                    id: raw.id.clone(),
                    src: source_ref.into(),
                    reason: format!("invalid source_config: {err}"),
                }
            })?
        };
        validate_request_config(&config.request, raw)?;
        validate_result_config(&config.result, raw)?;

        let project_root = bundle_root_from_route_source(&raw.source_file).ok_or_else(|| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: source_ref.into(),
                reason: format!(
                    "handler mode could not derive bundle root from route source file '{}'; \
                     route files must live under <bundle>/.ai/node/routes",
                    raw.source_file.display()
                ),
            }
        })?;
        let route_bundle_id = bundle_id_from_bundle_root(&project_root).ok_or_else(|| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: source_ref.into(),
                reason: format!(
                    "handler mode could not derive bundle id from bundle root '{}'",
                    project_root.display()
                ),
            }
        })?;
        if route_bundle_id != source_bundle_id {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: source_ref.into(),
                reason: format!(
                    "handler source bundle '{}' must match route bundle '{}'",
                    source_bundle_id, route_bundle_id
                ),
            });
        }
        let execution_principal_id = format!("route-handler:{}:{}", route_bundle_id, raw.id);
        let execution_scopes = vec![handler_execute_scope(source_ref, &raw.id)?];

        Ok(Arc::new(CompiledHandlerMode {
            source_ref: source_ref.to_string(),
            project_root,
            request_config: config.request,
            result_config: config.result,
            request_body_mode: raw.request.body.clone(),
            execution_principal_id: execution_principal_id.clone(),
            execution_scopes: execution_scopes.clone(),
            invoker: Arc::new(CompiledDispatchInvoker {
                item_ref: source_ref.to_string(),
                authority: DispatchAuthority::FixedPrincipal {
                    fingerprint: execution_principal_id,
                    scopes: execution_scopes,
                },
            }),
        }))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledHandlerMode {
    fn is_streaming(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<Response, RouteDispatchError> {
        assert_resolved_handler_source_anchored(
            &ctx,
            &self.source_ref,
            &self.project_root,
            &self.execution_principal_id,
            &self.execution_scopes,
        )?;

        let envelope = build_route_envelope(
            compiled,
            &ctx,
            &self.request_config,
            &self.request_body_mode,
        )?;
        let parameters = serde_json::to_value(envelope).map_err(|err| {
            RouteDispatchError::Internal(format!(
                "failed to encode handler request envelope: {err}"
            ))
        })?;

        // The bundle root is resolution authority, not a writable project.
        // Handler mode invokes CompiledDispatchInvoker directly (it does not
        // pass through execute mode), so allocate a narrow request-owned
        // workspace here and hold its guard across the complete invocation.
        let workspace = std::env::temp_dir().join(format!(
            "ryeos-route-handler-{}-{:032x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        let workspace_guard = Arc::new(ryeos_app::temp_dir_guard::TempDirGuard::new(
            workspace.clone(),
        ));
        std::fs::create_dir(&workspace).map_err(|error| {
            RouteDispatchError::Internal(format!(
                "create isolated handler workspace {}: {error}",
                workspace.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    RouteDispatchError::Internal(format!(
                        "protect isolated handler workspace {}: {error}",
                        workspace.display()
                    ))
                },
            )?;
        }
        std::fs::create_dir_all(workspace.join(ryeos_engine::AI_DIR)).map_err(|error| {
            RouteDispatchError::Internal(format!(
                "create isolated handler workspace {}: {error}",
                workspace.display()
            ))
        })?;
        let input = json!({
            "project_path": workspace.to_string_lossy(),
            "parameters": parameters,
        });

        let inv_ctx = RouteInvocationContext {
            route_id: compiled.id.clone().into(),
            method: ctx.request_parts.method,
            uri: ctx.request_parts.uri,
            captures: BTreeMap::from_iter(ctx.captures),
            headers: ctx.request_parts.headers,
            body_raw: ctx.body_raw,
            input,
            principal: Some(ctx.principal),
            workspace_lifeline: Some(workspace_guard),
            launch_timings: None,
            state: ctx.state,
            webhook_dedupe: ctx.webhook_dedupe,
        };

        let result = crate::routes::invocation::invoke_checked(
            self.invoker.as_ref(),
            InvocationCheck {
                expected_output: RouteInvocationOutput::Json,
            },
            inv_ctx,
        )
        .await?;

        match result {
            RouteInvocationResult::Json { value, thread_id } => {
                let handler_value = handler_output_value(&value);
                let response_value = handler_value
                    .get(&self.result_config.envelope_field)
                    .ok_or_else(|| {
                        RouteDispatchError::Internal(format!(
                            "handler '{}' returned no '{}' response envelope",
                            self.source_ref, self.result_config.envelope_field
                        ))
                    })?;
                let envelope: HttpResponseEnvelope = serde_json::from_value(response_value.clone())
                    .map_err(|err| {
                        RouteDispatchError::Internal(format!(
                            "handler '{}' returned invalid HTTP response envelope: {err}",
                            self.source_ref
                        ))
                    })?;
                let mut response =
                    envelope_to_response(envelope, self.result_config.response_bytes_max)?;
                attach_recorded_thread_header(&mut response, thread_id.as_deref())?;
                Ok(response)
            }
            _ => unreachable!("invoke_checked enforces Json"),
        }
    }
}

fn assert_resolved_handler_source_anchored(
    ctx: &RouteDispatchContext,
    source_ref: &str,
    project_root: &Path,
    execution_principal_id: &str,
    execution_scopes: &[String],
) -> Result<(), RouteDispatchError> {
    let canonical_ref = CanonicalRef::parse(source_ref).map_err(|err| {
        RouteDispatchError::Internal(format!(
            "handler source '{source_ref}' is not a valid canonical ref: {err}"
        ))
    })?;
    let site_id = ctx.state.threads.site_id().to_string();
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: execution_principal_id.to_string(),
            scopes: execution_scopes.to_vec(),
        }),
        project_context: ProjectContext::LocalPath {
            path: project_root.to_path_buf(),
        },
        current_site_id: site_id.clone(),
        origin_site_id: site_id,
        execution_hints: Default::default(),
        validate_only: false,
    };

    let resolved = ctx
        .state
        .engine
        .resolve(&plan_ctx, &canonical_ref)
        .map_err(|err| {
            RouteDispatchError::Internal(format!(
                "handler source '{source_ref}' resolution failed: {err}"
            ))
        })?;

    validate_resolved_handler_source_path(&resolved.source_path, project_root, source_ref)
}

fn handler_execute_scope(source_ref: &str, route_id: &str) -> Result<String, RouteConfigError> {
    let (kind, subject) =
        source_ref
            .split_once(':')
            .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: source_ref.into(),
                reason: "handler source is not a valid canonical ref".into(),
            })?;
    Ok(ryeos_runtime::authorizer::canonical_cap(
        kind, subject, "execute",
    ))
}

fn validate_resolved_handler_source_path(
    source_path: &Path,
    project_root: &Path,
    source_ref: &str,
) -> Result<(), RouteDispatchError> {
    let expected_tools_root = project_root.join(ryeos_engine::AI_DIR).join("tools");
    if path_is_under(source_path, &expected_tools_root) {
        return Ok(());
    }

    Err(RouteDispatchError::Internal(format!(
        "handler source '{source_ref}' resolved to '{}' outside route bundle tools root '{}'",
        source_path.display(),
        expected_tools_root.display()
    )))
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => path.starts_with(root),
    }
}

fn validate_handler_source(source_ref: &str, route_id: &str) -> Result<String, RouteConfigError> {
    let parsed = crate::routes::parsed_ref::ParsedItemRef::parse(source_ref).map_err(|err| {
        RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: format!("source '{source_ref}' is not a valid canonical ref: {err}"),
        }
    })?;
    if parsed.kind() != "tool" {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: "handler mode currently requires a fixed `tool:` source".into(),
        });
    }
    let bare_id = source_ref.strip_prefix("tool:").unwrap_or_default();
    if !bare_id.contains('/') {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: "handler mode requires a bundle-qualified tool ref, e.g. \
                `tool:ryeos-email/webhook/track_click`"
                .into(),
        });
    }
    let bundle_id = ryeos_app::callback_token::effective_bundle_id_from_item_ref(source_ref)
        .filter(|bundle| !bundle.is_empty())
        .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: "handler mode requires a bundle-qualified tool ref, e.g. \
                `tool:ryeos-email/webhook/track_click`"
                .into(),
        })?;
    ryeos_state::objects::validate_bundle_identifier("bundle_id", &bundle_id).map_err(|err| {
        RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: err.to_string(),
        }
    })?;
    Ok(bundle_id)
}

fn validate_request_config(
    config: &HandlerRequestConfig,
    raw: &RawRouteSpec,
) -> Result<(), RouteConfigError> {
    for header in &config.headers {
        HeaderName::from_bytes(header.as_bytes()).map_err(|err| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: "handler".into(),
                reason: format!("invalid request header name '{header}': {err}"),
            }
        })?;
    }
    if config.body && raw.request.body == RawRequestBody::None {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "handler".into(),
            reason: "source_config.request.body is true but request.body is none".into(),
        });
    }
    Ok(())
}

fn validate_result_config(
    config: &HandlerResultConfig,
    raw: &RawRouteSpec,
) -> Result<(), RouteConfigError> {
    if config.envelope_field.trim().is_empty() {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "handler".into(),
            reason: "source_config.result.envelope_field must not be empty".into(),
        });
    }
    if config.response_bytes_max == 0 {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "handler".into(),
            reason: "source_config.result.response_bytes_max must be greater than zero".into(),
        });
    }
    Ok(())
}

fn bundle_root_from_route_source(source_file: &Path) -> Option<PathBuf> {
    let mut roots = Vec::new();
    for ai_dir in source_file.ancestors().filter(|path| {
        path.file_name().and_then(|name| name.to_str()) == Some(ryeos_engine::AI_DIR)
    }) {
        let relative = source_file.strip_prefix(ai_dir).ok()?;
        let mut components = relative.components();
        if components.next()?.as_os_str() == "node" && components.next()?.as_os_str() == "routes" {
            roots.push(ai_dir.parent()?.to_path_buf());
        }
    }
    match roots.as_slice() {
        [root] => Some(root.clone()),
        _ => None,
    }
}

fn bundle_id_from_bundle_root(bundle_root: &Path) -> Option<String> {
    bundle_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn handler_output_value(value: &Value) -> &Value {
    let dispatch_result = value.get("result").unwrap_or(value);
    dispatch_result.get("result").unwrap_or(dispatch_result)
}

fn build_route_envelope(
    compiled: &CompiledRoute,
    ctx: &RouteDispatchContext,
    config: &HandlerRequestConfig,
    body_mode: &RawRequestBody,
) -> Result<RouteEnvelope, RouteDispatchError> {
    let principal = &ctx.principal;
    let path = ctx.request_parts.uri.path().to_string();
    let (uri, raw_query) = request_uri_fields(&ctx.request_parts.uri, config.query);
    Ok(RouteEnvelope {
        route: RouteEnvelopeRoute {
            id: compiled.id.clone(),
        },
        request: RequestEnvelope {
            method: ctx.request_parts.method.to_string(),
            path,
            uri,
            raw_query,
            query: config
                .query
                .then(|| query_params_to_json(ctx.request_parts.uri.query())),
            path_params: config
                .path_params
                .then(|| BTreeMap::from_iter(ctx.captures.clone())),
            headers: collect_allowed_headers(&ctx.request_parts.headers, &config.headers),
            body: if config.body {
                body_envelope(body_mode, &ctx.body_raw)?
            } else {
                BodyEnvelope::None
            },
        },
        principal: PrincipalEnvelope {
            id: principal.id.clone(),
            verified: principal.verified,
            verifier: principal.verifier_key,
            metadata: principal.metadata.clone(),
        },
    })
}

fn request_uri_fields(uri: &Uri, include_query: bool) -> (String, Option<String>) {
    if include_query {
        (uri.to_string(), uri.query().map(str::to_string))
    } else {
        (uri.path().to_string(), None)
    }
}

fn collect_allowed_headers(headers: &HeaderMap, allowlist: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for header in allowlist {
        let Ok(name) = HeaderName::from_bytes(header.as_bytes()) else {
            continue;
        };
        if let Some(value) = headers.get(&name).and_then(|value| value.to_str().ok()) {
            out.insert(name.as_str().to_string(), value.to_string());
        }
    }
    out
}

fn body_envelope(mode: &RawRequestBody, raw: &[u8]) -> Result<BodyEnvelope, RouteDispatchError> {
    match mode {
        RawRequestBody::None => Ok(BodyEnvelope::None),
        RawRequestBody::Json => {
            let json = serde_json::from_slice(raw).map_err(|err| {
                RouteDispatchError::BadRequest(format!("invalid JSON body: {err}"))
            })?;
            Ok(BodyEnvelope::Json { json })
        }
        RawRequestBody::Text => {
            let text = std::str::from_utf8(raw)
                .map_err(|err| {
                    RouteDispatchError::BadRequest(format!("invalid UTF-8 body: {err}"))
                })?
                .to_string();
            Ok(BodyEnvelope::Text { text })
        }
        RawRequestBody::Raw => Ok(BodyEnvelope::Base64 {
            base64: base64::engine::general_purpose::STANDARD.encode(raw),
        }),
    }
}

fn query_params_to_json(query: Option<&str>) -> Value {
    let mut map = serde_json::Map::new();
    let Some(query) = query else {
        return Value::Object(map);
    };

    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        map.insert(key.into_owned(), Value::String(value.into_owned()));
    }

    Value::Object(map)
}

fn envelope_to_response(
    envelope: HttpResponseEnvelope,
    response_bytes_max: usize,
) -> Result<Response, RouteDispatchError> {
    let status_raw = envelope.status.unwrap_or(200);
    if !(200..=599).contains(&status_raw) {
        return Err(RouteDispatchError::Internal(format!(
            "handler response status must be in 200..=599; got {status_raw}"
        )));
    }
    let status = StatusCode::from_u16(status_raw).map_err(|err| {
        RouteDispatchError::Internal(format!("invalid handler response status: {err}"))
    })?;

    let body_fields = [
        envelope.json.is_some(),
        envelope.body.is_some(),
        envelope.body_base64.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if body_fields > 1 {
        return Err(RouteDispatchError::Internal(
            "handler response envelope must set at most one of json/body/body_base64".into(),
        ));
    }
    if matches!(status_raw, 204 | 205 | 304) && body_fields > 0 {
        return Err(RouteDispatchError::Internal(
            "handler response envelope must not include a body for 204/205/304".into(),
        ));
    }
    if envelope.json.is_some() && envelope.content_type.is_some() {
        return Err(RouteDispatchError::Internal(
            "handler response envelope must not set content_type with json; JSON responses use application/json".into(),
        ));
    }
    if body_fields == 0 && envelope.content_type.is_some() {
        return Err(RouteDispatchError::Internal(
            "handler response envelope must not set content_type without body or body_base64"
                .into(),
        ));
    }
    if let Some(content_type) = &envelope.content_type {
        HeaderValue::from_str(content_type).map_err(|err| {
            RouteDispatchError::Internal(format!(
                "handler response contains invalid content_type: {err}"
            ))
        })?;
    }

    let mut builder = Response::builder().status(status);
    for (name, value) in envelope.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
            RouteDispatchError::Internal(format!(
                "handler response contains invalid header name: {err}"
            ))
        })?;
        validate_response_header(&name)?;
        let value = HeaderValue::from_str(&value).map_err(|err| {
            RouteDispatchError::Internal(format!(
                "handler response contains invalid header value for '{}': {err}",
                name.as_str()
            ))
        })?;
        builder = builder.header(name, value);
    }

    let body = if let Some(json_body) = envelope.json {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        let body = serde_json::to_vec(&json_body).map_err(|err| {
            RouteDispatchError::Internal(format!("failed to encode handler JSON response: {err}"))
        })?;
        validate_response_size(body.len(), response_bytes_max)?;
        Body::from(body)
    } else if let Some(body) = envelope.body {
        if let Some(content_type) = &envelope.content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        validate_response_size(body.len(), response_bytes_max)?;
        Body::from(body)
    } else if let Some(body_base64) = envelope.body_base64 {
        if let Some(content_type) = &envelope.content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let body = base64::engine::general_purpose::STANDARD
            .decode(body_base64.as_bytes())
            .map_err(|err| {
                RouteDispatchError::Internal(format!(
                    "handler response body_base64 is invalid: {err}"
                ))
            })?;
        validate_response_size(body.len(), response_bytes_max)?;
        Body::from(body)
    } else {
        Body::empty()
    };

    builder.body(body).map_err(|err| {
        RouteDispatchError::Internal(format!("failed to build handler response: {err}"))
    })
}

fn validate_response_size(len: usize, max: usize) -> Result<(), RouteDispatchError> {
    if len > max {
        return Err(RouteDispatchError::Internal(format!(
            "handler response body is {len} bytes, exceeding response_bytes_max {max}"
        )));
    }
    Ok(())
}

fn validate_response_header(name: &HeaderName) -> Result<(), RouteDispatchError> {
    const DENIED: &[&str] = &[
        "connection",
        "content-length",
        "content-type",
        "host",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "set-cookie",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    if DENIED.contains(&name.as_str()) {
        return Err(RouteDispatchError::Internal(format!(
            "handler response header '{}' is not allowed",
            name.as_str()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use ryeos_app::route_raw::{RawLimits, RawRequest, RawResponseSpec};
    use std::fs;

    fn raw_handler_route(source_config: Value) -> RawRouteSpec {
        RawRouteSpec {
            id: "ryeos-email.track_click".into(),
            path: "/track/click".into(),
            methods: ["GET".to_string()].into_iter().collect(),
            auth: "none".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "handler".into(),
                source: Some("tool:ryeos-email/webhook/track_click".into()),
                source_config,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest::default(),
            source_file: PathBuf::from("/tmp/ryeos-email/.ai/node/routes/click.yaml"),
        }
    }

    #[test]
    fn compile_accepts_bundle_qualified_tool_source() {
        let raw = raw_handler_route(json!({
            "request": {
                "query": true,
                "headers": ["user-agent", "x-forwarded-for"]
            }
        }));

        HandlerMode.compile(&raw).unwrap();
    }

    #[test]
    fn compile_uses_fixed_route_handler_authority() {
        let raw = raw_handler_route(Value::Null);

        let compiled = HandlerMode.compile(&raw).unwrap();
        let handler = compiled
            .as_any()
            .downcast_ref::<CompiledHandlerMode>()
            .unwrap();

        assert_eq!(
            handler.execution_principal_id,
            "route-handler:ryeos-email:ryeos-email.track_click"
        );
        assert_eq!(
            handler.execution_scopes,
            vec!["ryeos.execute.tool.ryeos-email/webhook/track_click"]
        );
        match &handler.invoker.authority {
            DispatchAuthority::FixedPrincipal {
                fingerprint,
                scopes,
            } => {
                assert_eq!(fingerprint, &handler.execution_principal_id);
                assert_eq!(scopes, &handler.execution_scopes);
            }
            DispatchAuthority::CallerPrincipal => {
                panic!("handler mode must not use caller authority")
            }
        }
    }

    #[test]
    fn compile_rejects_source_config_project_path() {
        let raw = raw_handler_route(json!({
            "project_path": "/tmp/ryeos-email",
            "request": { "query": true }
        }));

        let err = match HandlerMode.compile(&raw) {
            Ok(_) => panic!("expected project_path to be rejected"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("invalid source_config"), "got: {err}");
        assert!(err.contains("unknown field `project_path`"), "got: {err}");
    }

    #[test]
    fn compile_rejects_non_tool_source() {
        let mut raw = raw_handler_route(Value::Null);
        raw.response.source = Some("service:health/status".into());

        let err = match HandlerMode.compile(&raw) {
            Ok(_) => panic!("expected non-tool source to be rejected"),
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("requires a fixed `tool:` source"),
            "got: {err}"
        );
    }

    #[test]
    fn compile_rejects_unqualified_tool_source() {
        let mut raw = raw_handler_route(Value::Null);
        raw.response.source = Some("tool:track_click".into());

        let err = match HandlerMode.compile(&raw) {
            Ok(_) => panic!("expected unqualified tool source to be rejected"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("bundle-qualified tool ref"), "got: {err}");
    }

    #[test]
    fn compile_rejects_cross_bundle_source() {
        let mut raw = raw_handler_route(Value::Null);
        raw.response.source = Some("tool:other-bundle/webhook/track_click".into());

        let err = match HandlerMode.compile(&raw) {
            Ok(_) => panic!("expected cross-bundle source to be rejected"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("must match route bundle"), "got: {err}");
    }

    #[test]
    fn compile_rejects_route_source_outside_node_routes() {
        let mut raw = raw_handler_route(Value::Null);
        raw.source_file = PathBuf::from("/tmp/ryeos-email/.ai/routes/click.yaml");

        let err = match HandlerMode.compile(&raw) {
            Ok(_) => panic!("expected non-node route source to be rejected"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains(".ai/node/routes"), "got: {err}");
    }

    #[test]
    fn compile_rejects_nested_node_routes_source() {
        let mut raw = raw_handler_route(Value::Null);
        raw.source_file =
            PathBuf::from("/tmp/ryeos-email/.ai/node/routes/nested/.ai/node/routes/click.yaml");

        let err = match HandlerMode.compile(&raw) {
            Ok(_) => panic!("expected nested route source to be rejected"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains(".ai/node/routes"), "got: {err}");
    }

    #[test]
    fn handler_output_value_unwraps_inline_dispatch_result() {
        let value = json!({
            "thread": {"thread_id": "T-test"},
            "result": {
                "outcome_code": "success",
                "result": {
                    "response": {
                        "status": 200,
                        "json": {"ok": true}
                    }
                },
                "error": null,
                "artifacts": []
            }
        });

        assert_eq!(handler_output_value(&value)["response"]["json"]["ok"], true);
    }

    #[test]
    fn query_params_remain_strings() {
        let value = query_params_to_json(Some("id=00123&enabled=false&count=42"));

        assert_eq!(value["id"], "00123");
        assert_eq!(value["enabled"], "false");
        assert_eq!(value["count"], "42");
    }

    #[test]
    fn request_uri_fields_omit_query_when_disabled() {
        let uri: Uri = "/track/click?id=00123&secret=token".parse().unwrap();

        let (sanitized_uri, raw_query) = request_uri_fields(&uri, false);

        assert_eq!(sanitized_uri, "/track/click");
        assert_eq!(raw_query, None);
    }

    #[test]
    fn request_uri_fields_include_query_when_enabled() {
        let uri: Uri = "/track/click?id=00123&secret=token".parse().unwrap();

        let (sanitized_uri, raw_query) = request_uri_fields(&uri, true);

        assert_eq!(sanitized_uri, "/track/click?id=00123&secret=token");
        assert_eq!(raw_query.as_deref(), Some("id=00123&secret=token"));
    }

    #[test]
    fn resolved_handler_source_path_accepts_bundle_tool() {
        let temp = tempfile::tempdir().unwrap();
        let bundle_root = temp.path().join("ryeos-email");
        let tool_path = bundle_root
            .join(ryeos_engine::AI_DIR)
            .join("tools")
            .join("ryeos-email/webhook/track_click.py");
        fs::create_dir_all(tool_path.parent().unwrap()).unwrap();
        fs::write(&tool_path, "# tool\n").unwrap();

        validate_resolved_handler_source_path(
            &tool_path,
            &bundle_root,
            "tool:ryeos-email/webhook/track_click",
        )
        .unwrap();
    }

    #[test]
    fn resolved_handler_source_path_rejects_external_winner() {
        let temp = tempfile::tempdir().unwrap();
        let bundle_root = temp.path().join("ryeos-email");
        let system_tool = temp
            .path()
            .join("system")
            .join(ryeos_engine::AI_DIR)
            .join("tools")
            .join("ryeos-email/webhook/track_click.py");
        fs::create_dir_all(system_tool.parent().unwrap()).unwrap();
        fs::write(&system_tool, "# shadowing system tool\n").unwrap();

        let err = validate_resolved_handler_source_path(
            &system_tool,
            &bundle_root,
            "tool:ryeos-email/webhook/track_click",
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("outside route bundle tools root"),
            "got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_handler_source_path_rejects_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let bundle_root = temp.path().join("ryeos-email");
        let external_dir = temp.path().join("external");
        let external_tool = external_dir.join("track_click.py");
        fs::create_dir_all(&external_dir).unwrap();
        fs::write(&external_tool, "# escaped tool\n").unwrap();

        let tools_root = bundle_root.join(ryeos_engine::AI_DIR).join("tools");
        fs::create_dir_all(&tools_root).unwrap();
        let symlink_path = tools_root.join("track_click.py");
        std::os::unix::fs::symlink(&external_tool, &symlink_path).unwrap();

        let err = validate_resolved_handler_source_path(
            &symlink_path,
            &bundle_root,
            "tool:ryeos-email/webhook/track_click",
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("outside route bundle tools root"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn response_envelope_builds_redirect() {
        let response = envelope_to_response(
            HttpResponseEnvelope {
                status: Some(302),
                headers: [("Location".to_string(), "https://example.com".to_string())]
                    .into_iter()
                    .collect(),
                content_type: None,
                body: None,
                body_base64: None,
                json: None,
            },
            default_response_bytes_max(),
        )
        .unwrap();

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "https://example.com"
        );
    }

    #[tokio::test]
    async fn response_envelope_builds_pixel_body() {
        let response = envelope_to_response(
            HttpResponseEnvelope {
                status: Some(200),
                headers: BTreeMap::new(),
                content_type: Some("image/gif".into()),
                body: None,
                body_base64: Some("R0lGODlhAQABAAAAACw=".into()),
                json: None,
            },
            default_response_bytes_max(),
        )
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/gif"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn response_envelope_rejects_set_cookie() {
        let err = envelope_to_response(
            HttpResponseEnvelope {
                status: Some(200),
                headers: [("Set-Cookie".to_string(), "a=b".to_string())]
                    .into_iter()
                    .collect(),
                content_type: None,
                body: None,
                body_base64: None,
                json: None,
            },
            default_response_bytes_max(),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("not allowed"), "got: {err}");
    }

    #[test]
    fn response_envelope_rejects_hop_by_hop_headers() {
        for denied in ["Keep-Alive", "Proxy-Authorization", "Transfer-Encoding"] {
            let err = envelope_to_response(
                HttpResponseEnvelope {
                    status: Some(200),
                    headers: [(denied.to_string(), "x".to_string())]
                        .into_iter()
                        .collect(),
                    content_type: None,
                    body: None,
                    body_base64: None,
                    json: None,
                },
                default_response_bytes_max(),
            )
            .unwrap_err()
            .to_string();

            assert!(err.contains("not allowed"), "header {denied} got: {err}");
        }
    }

    #[test]
    fn response_envelope_rejects_content_type_with_json() {
        let err = envelope_to_response(
            HttpResponseEnvelope {
                status: Some(200),
                headers: BTreeMap::new(),
                content_type: Some("application/vnd.custom+json".into()),
                body: None,
                body_base64: None,
                json: Some(json!({"ok": true})),
            },
            default_response_bytes_max(),
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("must not set content_type with json"),
            "got: {err}"
        );
    }

    #[test]
    fn response_envelope_rejects_oversized_body() {
        let err = envelope_to_response(
            HttpResponseEnvelope {
                status: Some(200),
                headers: BTreeMap::new(),
                content_type: Some("text/plain".into()),
                body: Some("too large".into()),
                body_base64: None,
                json: None,
            },
            4,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("exceeding response_bytes_max"), "got: {err}");
    }
}
