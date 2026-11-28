use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::route_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use ryeos_engine::route_raw::{RawRequestBody, RawRouteSpec};

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
    Inline { body: Vec<u8>, etag: String },
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

// ── Byte ranges ────────────────────────────────────────────────────────────

/// What a `Range` request asks of a body of `len` bytes.
#[derive(Debug, PartialEq, Eq)]
enum RangeAnswer {
    /// Send the whole body: no range, a range this mode does not serve
    /// (several ranges, a unit other than bytes, malformed), or an
    /// `If-Range` that does not match.
    Whole,
    /// Send bytes `start..=end`.
    Part { start: u64, end: u64 },
    /// No byte of the body is in the range.
    Unsatisfiable,
}

/// Read one `bytes=` range against a body of `len` bytes. Several ranges and
/// anything malformed are answered with the whole body, which RFC 9110 allows.
fn range_answer(range: &str, len: u64) -> RangeAnswer {
    let Some(spec) = range.trim().strip_prefix("bytes=") else {
        return RangeAnswer::Whole;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return RangeAnswer::Whole;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeAnswer::Whole;
    };
    let number = |text: &str| -> Option<u64> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    match (first.is_empty(), last.is_empty()) {
        // `-n`: the last n bytes.
        (true, false) => match number(last) {
            None => RangeAnswer::Whole,
            Some(0) => RangeAnswer::Unsatisfiable,
            Some(_) if len == 0 => RangeAnswer::Unsatisfiable,
            Some(suffix) => RangeAnswer::Part {
                start: len.saturating_sub(suffix),
                end: len - 1,
            },
        },
        // `a-`: from a to the end.
        (false, true) => match number(first) {
            None => RangeAnswer::Whole,
            Some(start) if start >= len => RangeAnswer::Unsatisfiable,
            Some(start) => RangeAnswer::Part {
                start,
                end: len - 1,
            },
        },
        // `a-b`.
        (false, false) => match (number(first), number(last)) {
            (Some(start), Some(end)) if start <= end => {
                if start >= len {
                    RangeAnswer::Unsatisfiable
                } else {
                    RangeAnswer::Part {
                        start,
                        end: end.min(len - 1),
                    }
                }
            }
            _ => RangeAnswer::Whole,
        },
        (true, true) => RangeAnswer::Whole,
    }
}

/// The response for `body` under the request's `Range` and `If-Range`.
///
/// Ranges are served only on a 200 body sent as stored without a transfer
/// coding (`rangeable`); otherwise the body goes out whole and no
/// `Accept-Ranges` is sent. An `If-Range` is honoured only against this
/// body's strong `etag`; anything else, a date included, sends it whole.
fn ranged_response(
    status: StatusCode,
    body: std::borrow::Cow<'static, [u8]>,
    request: &axum::http::HeaderMap,
    etag: Option<&str>,
    rangeable: bool,
) -> Response {
    if !rangeable || status != StatusCode::OK {
        return (status, body).into_response();
    }
    let len = body.len() as u64;
    let if_range_holds = match request.get(header::IF_RANGE) {
        None => true,
        Some(value) => match (value.to_str(), etag) {
            (Ok(given), Some(etag)) => !given.starts_with("W/") && given == etag,
            _ => false,
        },
    };
    let answer = match request.get(header::RANGE).map(|value| value.to_str()) {
        Some(Ok(range)) if if_range_holds => range_answer(range, len),
        _ => RangeAnswer::Whole,
    };
    let mut response = match answer {
        RangeAnswer::Whole => (status, body).into_response(),
        RangeAnswer::Part { start, end } => {
            let part = body[start as usize..=end as usize].to_vec();
            let mut response = (StatusCode::PARTIAL_CONTENT, part).into_response();
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{len}"))
                    .expect("a byte range is a valid header value"),
            );
            response
        }
        RangeAnswer::Unsatisfiable => {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{len}"))
                    .expect("a byte length is a valid header value"),
            );
            response
        }
    };
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
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
        refuse_unsafe_keyless(raw)?;
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

                let etag = format!("\"{}\"", lillux::cas::sha256_hex(&body));
                CompiledStaticSource::Inline { body, etag }
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

/// A static route anyone may call (`auth: none`) serves its signed bytes and
/// nothing else: GET or HEAD only, no request body, nothing executed and no
/// authority lent.
fn refuse_unsafe_keyless(raw: &RawRouteSpec) -> Result<(), RouteConfigError> {
    if raw.auth != "none" {
        return Ok(());
    }
    let refuse = |reason: String| {
        Err(RouteConfigError::InvalidResponseSpec {
            id: raw.id.clone(),
            mode: "static".into(),
            reason: format!("a keyless static route {reason}"),
        })
    };
    let mut methods: Vec<&String> = raw.methods.iter().collect();
    methods.sort();
    if let Some(method) = methods
        .iter()
        .find(|method| !matches!(method.as_str(), "GET" | "HEAD"))
    {
        return refuse(format!("accepts only GET and HEAD, not {method}"));
    }
    if raw.request.body != RawRequestBody::None || raw.limits.body_bytes_max != 0 {
        return refuse("accepts no request body (request.body none, body_bytes_max 0)".into());
    }
    if raw.execute.is_some() {
        return refuse("executes nothing".into());
    }
    if raw.authority.is_some() {
        return refuse("lends no authority".into());
    }
    Ok(())
}

/// Headers every static body carries, inline or embedded.
fn harden(headers: &mut axum::http::HeaderMap) {
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(CSP_VALUE),
    );
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
            CompiledStaticSource::Inline { body, etag } => {
                if let Some(inm) = ctx.request_parts.headers.get(header::IF_NONE_MATCH)
                    && inm.to_str().is_ok_and(|inm| inm == etag)
                {
                    let mut resp = StatusCode::NOT_MODIFIED.into_response();
                    resp.headers_mut()
                        .insert(header::ETAG, HeaderValue::from_str(etag).expect("hex etag"));
                    return Ok(resp);
                }
                let mut resp = ranged_response(
                    self.status,
                    std::borrow::Cow::Owned(body.clone()),
                    &ctx.request_parts.headers,
                    Some(etag),
                    true,
                );
                let headers = resp.headers_mut();
                if let Some(ref ct) = self.content_type {
                    headers.insert(header::CONTENT_TYPE, ct.clone());
                }
                headers.insert(header::ETAG, HeaderValue::from_str(etag).expect("hex etag"));
                headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                harden(headers);
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

                let mut resp = ranged_response(
                    self.status,
                    body,
                    &ctx.request_parts.headers,
                    Some(&etag),
                    !stored_encoded,
                );
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
                harden(headers);
                Ok(resp)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::route_raw::{
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
            limits: RawLimits {
                body_bytes_max: 0,
                ..RawLimits::default()
            },
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
            authority: None,
            public: false,
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
            limits: RawLimits {
                body_bytes_max: 0,
                ..RawLimits::default()
            },
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
            authority: None,
            public: false,
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

    fn headers(pairs: &[(header::HeaderName, &str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(name.clone(), value.parse().unwrap());
        }
        map
    }

    async fn body_of(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    const BODY: &[u8] = b"0123456789";
    const ETAG: &str = "\"body-etag\"";

    fn answer(request: &axum::http::HeaderMap, rangeable: bool) -> Response {
        ranged_response(
            StatusCode::OK,
            std::borrow::Cow::Borrowed(BODY),
            request,
            Some(ETAG),
            rangeable,
        )
    }

    #[test]
    fn range_specs_read_as_rfc_9110_says() {
        assert_eq!(
            range_answer("bytes=0-1", 10),
            RangeAnswer::Part { start: 0, end: 1 }
        );
        assert_eq!(
            range_answer("bytes=8-", 10),
            RangeAnswer::Part { start: 8, end: 9 }
        );
        assert_eq!(
            range_answer("bytes=-3", 10),
            RangeAnswer::Part { start: 7, end: 9 }
        );
        assert_eq!(
            range_answer("bytes=-30", 10),
            RangeAnswer::Part { start: 0, end: 9 }
        );
        assert_eq!(
            range_answer("bytes=5-500", 10),
            RangeAnswer::Part { start: 5, end: 9 }
        );
        assert_eq!(range_answer("bytes=10-", 10), RangeAnswer::Unsatisfiable);
        assert_eq!(range_answer("bytes=10-12", 10), RangeAnswer::Unsatisfiable);
        assert_eq!(range_answer("bytes=-0", 10), RangeAnswer::Unsatisfiable);
        assert_eq!(range_answer("bytes=0-", 0), RangeAnswer::Unsatisfiable);
        assert_eq!(range_answer("bytes=0-1,4-5", 10), RangeAnswer::Whole);
        assert_eq!(range_answer("items=0-1", 10), RangeAnswer::Whole);
        assert_eq!(range_answer("bytes=5-2", 10), RangeAnswer::Whole);
        assert_eq!(range_answer("bytes=a-b", 10), RangeAnswer::Whole);
        assert_eq!(range_answer("bytes=+1-2", 10), RangeAnswer::Whole);
        assert_eq!(range_answer("bytes=-", 10), RangeAnswer::Whole);
    }

    #[tokio::test]
    async fn one_range_is_answered_206_with_its_content_range() {
        let response = answer(&headers(&[(header::RANGE, "bytes=0-1")]), true);
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-1/10");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(body_of(response).await, b"01");
    }

    #[tokio::test]
    async fn an_unsatisfiable_range_is_answered_416_with_the_length() {
        let response = answer(&headers(&[(header::RANGE, "bytes=10-")]), true);
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert!(body_of(response).await.is_empty());
    }

    #[tokio::test]
    async fn several_ranges_are_answered_whole() {
        let response = answer(&headers(&[(header::RANGE, "bytes=0-1,4-5")]), true);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(response.headers().get(header::CONTENT_RANGE).is_none());
        assert_eq!(body_of(response).await, BODY);
    }

    #[tokio::test]
    async fn if_range_serves_the_range_only_for_this_bodys_strong_etag() {
        let matching = answer(
            &headers(&[(header::RANGE, "bytes=0-1"), (header::IF_RANGE, ETAG)]),
            true,
        );
        assert_eq!(matching.status(), StatusCode::PARTIAL_CONTENT);
        for stale in [
            "\"another\"",
            "W/\"body-etag\"",
            "Wed, 21 Oct 2015 07:28:00 GMT",
        ] {
            let response = answer(
                &headers(&[(header::RANGE, "bytes=0-1"), (header::IF_RANGE, stale)]),
                true,
            );
            assert_eq!(response.status(), StatusCode::OK, "If-Range {stale}");
            assert_eq!(body_of(response).await, BODY);
        }
    }

    #[tokio::test]
    async fn a_body_sent_with_a_coding_is_never_ranged() {
        let response = answer(&headers(&[(header::RANGE, "bytes=0-1")]), false);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::ACCEPT_RANGES).is_none());
        assert_eq!(body_of(response).await, BODY);
    }

    #[tokio::test]
    async fn only_a_200_body_is_ranged() {
        let response = ranged_response(
            StatusCode::NOT_FOUND,
            std::borrow::Cow::Borrowed(BODY),
            &headers(&[(header::RANGE, "bytes=0-1")]),
            Some(ETAG),
            true,
        );
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get(header::ACCEPT_RANGES).is_none());
    }

    #[tokio::test]
    async fn without_a_range_the_whole_body_says_ranges_are_accepted() {
        let response = answer(&headers(&[]), true);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(body_of(response).await, BODY);
    }

    #[test]
    fn a_keyless_static_route_serves_only_its_signed_bytes() {
        let mode = mode_with_provider();
        let base = || make_raw(Some(200), Some("text/plain"), Some("aGVsbG8="));
        assert!(mode.compile(&base()).is_ok());

        let mut post = base();
        post.methods = ["GET".to_string(), "POST".to_string()]
            .into_iter()
            .collect();
        let mut head = base();
        head.methods = ["GET".to_string(), "HEAD".to_string()]
            .into_iter()
            .collect();
        assert!(mode.compile(&head).is_ok(), "GET and HEAD are allowed");
        let mut body_limit = base();
        body_limit.limits.body_bytes_max = 1;
        let mut body = base();
        body.request.body = RawRequestBody::Json;
        let mut authority = base();
        authority.authority = Some(ryeos_engine::route_raw::RawRouteAuthority {
            capabilities: Vec::new(),
        });
        let mut execute = base();
        execute.execute = Some(ryeos_engine::route_raw::RawExecute {
            item_ref: "tool:demo/run".into(),
            params: serde_json::Value::Null,
        });
        for (case, raw) in [
            ("POST", post),
            ("a body limit", body_limit),
            ("a request body", body),
            ("an authority block", authority),
            ("an execute block", execute),
        ] {
            let error = mode
                .compile(&raw)
                .err()
                .unwrap_or_else(|| panic!("{case} compiled"));
            assert!(
                error.to_string().contains("a keyless static route"),
                "{case}: {error}"
            );
        }

        // A route that names its caller is not bound by the keyless rules.
        let mut keyed = base();
        keyed.auth = "browser_session".into();
        keyed.limits.body_bytes_max = 1;
        assert!(mode.compile(&keyed).is_ok());
    }
}
