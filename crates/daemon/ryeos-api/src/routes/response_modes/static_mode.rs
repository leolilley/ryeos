use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::route_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use ryeos_app::route_raw::{RawRequestBody, RawRouteSpec};

// ── Generic static asset provider trait ────────────────────────────────────

/// A static asset served by an injected provider.
pub struct StaticAsset {
    pub bytes: std::borrow::Cow<'static, [u8]>,
    pub content_type: &'static str,
    /// `Some("gzip")` when `bytes` are gzip-encoded: sent as they are to a
    /// client that accepts gzip, decoded for one that does not.
    pub content_encoding: Option<&'static str>,
    pub etag: String,
    /// Cache-Control header value.
    pub cache_control: &'static str,
}

/// Provider trait for resolving embedded static assets by path.
///
/// Implemented by UI/composition layers that own the actual asset bytes.
/// API's `static_mode` uses this at compile time (literal path validation)
/// and dispatch time (asset resolution).
pub trait StaticAssetProvider: Send + Sync {
    fn get(&self, path: &str) -> Option<StaticAsset>;
}

// ── StaticMode ─────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct StaticMode {
    /// Static asset providers keyed by source name (e.g. "embedded_asset").
    /// When empty, `source: embedded_asset` routes are rejected at compile time.
    providers: HashMap<String, Arc<dyn StaticAssetProvider>>,
}

impl StaticMode {
    /// Register a static asset provider under the given source name.
    pub fn register_provider(
        &mut self,
        name: impl Into<String>,
        provider: Arc<dyn StaticAssetProvider>,
    ) {
        let name = name.into();
        if self.providers.contains_key(&name) {
            panic!("StaticMode: duplicate asset provider `{name}`");
        }
        self.providers.insert(name, provider);
    }
}

/// Compiled static source — either inline (body_b64) or embedded asset.
enum CompiledStaticSource {
    /// Inline body decoded from `body_b64` at compile time.
    Inline { body: Vec<u8> },
    /// Embedded asset resolved at dispatch time by path.
    EmbeddedAsset {
        path_template: PathTemplate,
        provider: Arc<dyn StaticAssetProvider>,
    },
}

/// Path template for embedded asset lookup.
enum PathTemplate {
    /// Literal path, e.g. `"index.html"`.
    Literal(String),
    /// Path capture interpolation, e.g. `"${path.asset}"` → capture name `"asset"`.
    Capture(String),
}

pub struct CompiledStaticMode {
    status: StatusCode,
    content_type: Option<HeaderValue>,
    source: CompiledStaticSource,
}

/// Security headers applied to all embedded asset responses.
/// Whether an `Accept-Encoding` value admits gzip: named (or `*`) without a
/// zero quality.
fn accepts_gzip(accept_encoding: Option<&HeaderValue>) -> bool {
    let Some(value) = accept_encoding.and_then(|value| value.to_str().ok()) else {
        return false;
    };
    value.split(',').any(|coding| {
        let mut parts = coding.split(';').map(str::trim);
        let name = parts.next().unwrap_or_default();
        let refused = parts.any(|parameter| {
            parameter
                .strip_prefix("q=")
                .and_then(|quality| quality.parse::<f32>().ok())
                .is_some_and(|quality| quality <= 0.0)
        });
        (name.eq_ignore_ascii_case("gzip") || name == "*") && !refused
    })
}

/// The body to send for `asset` and its `Content-Encoding`, if any. A
/// gzip-encoded asset goes out as stored when the client accepts gzip and is
/// decoded otherwise.
fn encoded_body(
    asset: StaticAsset,
    accept_encoding: Option<&HeaderValue>,
) -> Result<(std::borrow::Cow<'static, [u8]>, Option<&'static str>), RouteDispatchError> {
    match asset.content_encoding {
        None => Ok((asset.bytes, None)),
        Some("gzip") if accepts_gzip(accept_encoding) => Ok((asset.bytes, Some("gzip"))),
        Some("gzip") => {
            use std::io::Read as _;
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(&*asset.bytes)
                .read_to_end(&mut decoded)
                .map_err(|error| {
                    RouteDispatchError::Internal(format!("stored gzip asset is corrupt: {error}"))
                })?;
            Ok((std::borrow::Cow::Owned(decoded), None))
        }
        Some(other) => Err(RouteDispatchError::Internal(format!(
            "unsupported stored asset encoding {other}"
        ))),
    }
}

static CSP_VALUE: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:";

impl ResponseMode for StaticMode {
    fn key(&self) -> &'static str {
        "static"
    }

    fn allows_zero_timeout(&self) -> bool {
        true
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.request.body == RawRequestBody::ObjectsPutV1 {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "static".into(),
                reason: "static mode does not accept the objects_put_v1 body profile".into(),
            });
        }
        let status_raw =
            raw.response
                .status
                .ok_or_else(|| RouteConfigError::InvalidResponseSpec {
                    id: raw.id.clone(),
                    mode: "static".into(),
                    reason: "missing 'status' field".into(),
                })?;

        let status = StatusCode::from_u16(status_raw).map_err(|_| {
            RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "static".into(),
                reason: format!("invalid HTTP status code: {status_raw}"),
            }
        })?;

        let source = match raw.response.source.as_deref() {
            None | Some("") => {
                // Inline body_b64 mode (no source specified).
                if raw.response.content_type.is_none() {
                    return Err(RouteConfigError::InvalidResponseSpec {
                        id: raw.id.clone(),
                        mode: "static".into(),
                        reason: "missing 'content_type' field".into(),
                    });
                }

                let body_b64 = raw.response.body_b64.as_ref().ok_or_else(|| {
                    RouteConfigError::InvalidResponseSpec {
                        id: raw.id.clone(),
                        mode: "static".into(),
                        reason: "missing 'body_b64' field".into(),
                    }
                })?;

                use base64::Engine;
                let body = base64::engine::general_purpose::STANDARD
                    .decode(body_b64)
                    .map_err(|e| RouteConfigError::InvalidResponseSpec {
                        id: raw.id.clone(),
                        mode: "static".into(),
                        reason: format!("body_b64 is not valid base64: {e}"),
                    })?;

                CompiledStaticSource::Inline { body }
            }
            Some(source_name) => {
                // Look up the source name in the provider registry.
                let provider = self.providers.get(source_name).ok_or_else(|| {
                    RouteConfigError::InvalidSourceConfig {
                        id: raw.id.clone(),
                        src: source_name.into(),
                        reason: format!(
                            "no static asset provider registered for source '{source_name}'"
                        ),
                    }
                })?;

                let path_val = raw
                    .response
                    .source_config
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                        id: raw.id.clone(),
                        src: source_name.into(),
                        reason: "missing 'path' in source_config".into(),
                    })?;

                // Check if it's a ${path.<name>} interpolation.
                let path_template = if let Some(rest) = path_val.strip_prefix("${path.") {
                    if let Some(name) = rest.strip_suffix('}') {
                        PathTemplate::Capture(name.to_string())
                    } else {
                        return Err(RouteConfigError::InvalidSourceConfig {
                            id: raw.id.clone(),
                            src: source_name.into(),
                            reason: format!("invalid path template: {path_val}"),
                        });
                    }
                } else {
                    // Validate the literal path resolves to an asset.
                    if provider.get(path_val).is_none() {
                        return Err(RouteConfigError::InvalidSourceConfig {
                            id: raw.id.clone(),
                            src: source_name.into(),
                            reason: format!("no embedded asset for path: {path_val}"),
                        });
                    }
                    PathTemplate::Literal(path_val.to_string())
                };

                // content_type is optional for embedded assets (auto-detected from extension).
                CompiledStaticSource::EmbeddedAsset {
                    path_template,
                    provider: provider.clone(),
                }
            }
        };

        // content_type may be provided explicitly or auto-detected.
        let content_type = raw
            .response
            .content_type
            .as_ref()
            .map(|ct| {
                ct.parse::<HeaderValue>()
                    .map_err(|_| RouteConfigError::InvalidResponseSpec {
                        id: raw.id.clone(),
                        mode: "static".into(),
                        reason: format!("invalid content_type: {ct}"),
                    })
            })
            .transpose()?;

        Ok(Arc::new(CompiledStaticMode {
            status,
            content_type,
            source,
        }))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledStaticMode {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<Response, RouteDispatchError> {
        match &self.source {
            CompiledStaticSource::Inline { body } => {
                let mut resp = (self.status, body.clone()).into_response();
                if let Some(ref ct) = self.content_type {
                    resp.headers_mut().insert(header::CONTENT_TYPE, ct.clone());
                }
                Ok(resp)
            }
            CompiledStaticSource::EmbeddedAsset {
                path_template,
                provider,
            } => {
                let path = match path_template {
                    PathTemplate::Literal(p) => p.clone(),
                    PathTemplate::Capture(name) => {
                        ctx.captures.get(name).cloned().unwrap_or_default()
                    }
                };

                let asset = provider.get(&path).ok_or(RouteDispatchError::NotFound)?;

                // ETag / If-None-Match → 304.
                if let Some(inm) = ctx.request_parts.headers.get(header::IF_NONE_MATCH)
                    && let Ok(inm_str) = inm.to_str()
                    && inm_str == asset.etag
                {
                    let mut resp = StatusCode::NOT_MODIFIED.into_response();
                    resp.headers_mut()
                        .insert(header::ETAG, asset.etag.parse().unwrap());
                    return Ok(resp);
                }

                let ct = self
                    .content_type
                    .clone()
                    .unwrap_or_else(|| asset.content_type.parse().unwrap());

                let cache_control = asset.cache_control;
                let etag = asset.etag.clone();
                let stored_encoded = asset.content_encoding.is_some();
                let (body, encoding) = encoded_body(
                    asset,
                    ctx.request_parts.headers.get(header::ACCEPT_ENCODING),
                )?;

                let mut resp = (self.status, body).into_response();
                let headers = resp.headers_mut();
                if let Some(encoding) = encoding {
                    headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
                }
                if stored_encoded {
                    headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
                }
                headers.insert(header::CONTENT_TYPE, ct);
                headers.insert(header::ETAG, etag.parse().unwrap());
                headers.insert(header::CACHE_CONTROL, cache_control.parse().unwrap());
                headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
                headers.insert(header::REFERRER_POLICY, "same-origin".parse().unwrap());
                headers.insert(
                    header::HeaderName::from_static("content-security-policy"),
                    CSP_VALUE.parse().unwrap(),
                );
                Ok(resp)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::route_raw::{
        RawLimits, RawRequest, RawRequestBody, RawResponseSpec, RawRouteSpec,
    };

    /// Fake provider for testing: knows two generic asset paths.
    struct FakeProvider;

    impl StaticAssetProvider for FakeProvider {
        fn get(&self, path: &str) -> Option<StaticAsset> {
            match path.trim_start_matches('/') {
                "index.html" | "app/index.html" => Some(StaticAsset {
                    bytes: std::borrow::Cow::Borrowed(b"<html></html>"),
                    content_type: "text/html; charset=utf-8",
                    content_encoding: None,
                    etag: "\"fake-etag-html\"".to_string(),
                    cache_control: "no-cache",
                }),
                "main.js" | "app/assets/main.js" => Some(StaticAsset {
                    bytes: std::borrow::Cow::Borrowed(b"// main"),
                    content_type: "application/javascript; charset=utf-8",
                    content_encoding: None,
                    etag: "\"fake-etag-js\"".to_string(),
                    cache_control: "no-cache",
                }),
                _ => None,
            }
        }
    }

    fn mode_with_provider() -> StaticMode {
        let mut mode = StaticMode::default();
        mode.register_provider("asset_source", Arc::new(FakeProvider));
        mode
    }

    #[test]
    #[should_panic(expected = "duplicate asset provider")]
    fn duplicate_provider_registration_panics() {
        let mut mode = StaticMode::default();
        mode.register_provider("asset_source", Arc::new(FakeProvider));
        mode.register_provider("asset_source", Arc::new(FakeProvider));
    }

    fn make_raw(
        status: Option<u16>,
        content_type: Option<&str>,
        body_b64: Option<&str>,
    ) -> RawRouteSpec {
        RawRouteSpec {
            id: "test-route".to_string(),
            path: "/test".to_string(),
            methods: ["GET".to_string()].into_iter().collect(),
            auth: "none".to_string(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "static".to_string(),
                source: None,
                source_config: serde_json::Value::Null,
                status,
                content_type: content_type.map(|s| s.to_string()),
                body_b64: body_b64.map(|s| s.to_string()),
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        }
    }

    fn make_embedded_raw(
        path: &str,
        status: Option<u16>,
        content_type: Option<&str>,
    ) -> RawRouteSpec {
        RawRouteSpec {
            id: "test-embedded".to_string(),
            path: "/test".to_string(),
            methods: ["GET".to_string()].into_iter().collect(),
            auth: "none".to_string(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "static".to_string(),
                source: Some("asset_source".to_string()),
                source_config: serde_json::json!({ "path": path }),
                status,
                content_type: content_type.map(|s| s.to_string()),
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        }
    }

    #[test]
    fn valid_static_compiles() {
        let mode = StaticMode::default();
        let raw = make_raw(Some(200), Some("text/plain"), Some("aGVsbG8="));
        let result = mode.compile(&raw);
        assert!(result.is_ok());
        let compiled = match result {
            Ok(c) => c,
            Err(e) => panic!("expected Ok, got: {e}"),
        };
        let inner = compiled
            .as_ref()
            .as_any()
            .downcast_ref::<CompiledStaticMode>()
            .expect("downcast to CompiledStaticMode");
        assert_eq!(inner.status, StatusCode::OK);
    }

    #[test]
    fn static_rejects_objects_put_v1_profile() {
        let mode = StaticMode::default();
        let mut raw = make_raw(Some(200), Some("text/plain"), Some("aGVsbG8="));
        raw.request.body = RawRequestBody::ObjectsPutV1;
        assert!(matches!(
            mode.compile(&raw),
            Err(RouteConfigError::InvalidResponseSpec { .. })
        ));
    }

    #[test]
    fn asset_source_literal_compiles() {
        let mode = mode_with_provider();
        let raw = make_embedded_raw("index.html", Some(200), None);
        let result = mode.compile(&raw);
        assert!(result.is_ok(), "asset source compile should succeed");
    }

    #[test]
    fn asset_source_capture_compiles() {
        let mode = mode_with_provider();
        let raw = make_embedded_raw("${path.asset}", Some(200), None);
        let result = mode.compile(&raw);
        assert!(
            result.is_ok(),
            "asset source capture compile should succeed"
        );
    }

    #[test]
    fn asset_source_unknown_path_rejected() {
        let mode = mode_with_provider();
        let raw = make_embedded_raw("nonexistent.css", Some(200), None);
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("no embedded asset"), "got: {msg}");
    }

    #[test]
    fn asset_source_missing_path_rejected() {
        let mode = mode_with_provider();
        let mut raw = make_embedded_raw("index.html", Some(200), None);
        raw.response.source_config = serde_json::json!({});
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("missing 'path'"), "got: {msg}");
    }

    #[test]
    fn asset_source_no_provider_rejected() {
        let mode = StaticMode::default();
        let raw = make_embedded_raw("index.html", Some(200), None);
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("no static asset provider"), "got: {msg}");
    }

    #[test]
    fn allows_zero_timeout() {
        assert!(StaticMode::default().allows_zero_timeout());
    }

    #[test]
    fn bad_base64_rejected() {
        let mode = StaticMode::default();
        let raw = make_raw(Some(200), Some("text/plain"), Some("not-valid-base64!!!"));
        let result = mode.compile(&raw);
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("base64"), "got: {msg}");
            }
        }
    }

    #[test]
    fn bad_status_code_rejected() {
        let mode = StaticMode::default();
        let raw = make_raw(Some(99), Some("text/plain"), Some("aGVsbG8="));
        let result = mode.compile(&raw);
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("invalid HTTP status code"), "got: {msg}");
            }
        }
    }

    #[test]
    fn missing_status_rejected() {
        let mode = StaticMode::default();
        let raw = make_raw(None, Some("text/plain"), Some("aGVsbG8="));
        let result = mode.compile(&raw);
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("missing 'status'"), "got: {msg}");
            }
        }
    }

    #[test]
    fn missing_content_type_rejected_inline() {
        let mode = StaticMode::default();
        let raw = make_raw(Some(200), None, Some("aGVsbG8="));
        let result = mode.compile(&raw);
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("missing 'content_type'"), "got: {msg}");
            }
        }
    }

    #[test]
    fn missing_body_b64_rejected_inline() {
        let mode = StaticMode::default();
        let raw = make_raw(Some(200), Some("text/plain"), None);
        let result = mode.compile(&raw);
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("missing 'body_b64'"), "got: {msg}");
            }
        }
    }

    #[test]
    fn gzip_is_accepted_unless_refused() {
        let accepts = |value: &str| accepts_gzip(Some(&HeaderValue::from_str(value).unwrap()));
        assert!(accepts("gzip, deflate, br"));
        assert!(accepts("br;q=1.0, GZIP;q=0.5"));
        assert!(accepts("*"));
        assert!(!accepts("gzip;q=0"));
        assert!(!accepts("br, deflate"));
        assert!(!accepts_gzip(None));
    }

    #[test]
    fn a_gzip_asset_is_sent_as_stored_or_decoded() {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(b"(module)").unwrap();
        let stored = encoder.finish().unwrap();
        let asset = || StaticAsset {
            bytes: std::borrow::Cow::Owned(stored.clone()),
            content_type: "application/wasm",
            content_encoding: Some("gzip"),
            etag: "\"e\"".to_string(),
            cache_control: "no-cache",
        };
        let gzip = HeaderValue::from_static("gzip");
        let (body, encoding) = encoded_body(asset(), Some(&gzip)).unwrap();
        assert_eq!((&*body, encoding), (stored.as_slice(), Some("gzip")));
        let (body, encoding) = encoded_body(asset(), None).unwrap();
        assert_eq!((&*body, encoding), (&b"(module)"[..], None));
    }
}
