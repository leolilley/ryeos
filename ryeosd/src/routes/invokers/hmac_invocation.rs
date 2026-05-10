//! Generic, config-driven HMAC-SHA256 signature verifier as a compiled invoker.
//!
//! The daemon does not name vendors. Signing schemes are expressed as
//! YAML configurations that compose four primitives:
//!
//!   * `body` — raw request bytes (signed_payload only)
//!   * `header { header }` — entire HTTP header value
//!   * `header_pair { header, key }` — value of `<key>` in a
//!     `k=v,k=v,...` formatted header (e.g. composite-signature headers)
//!   * `body_json_path { path }` — string at top-level JSON body key
//!   * `synthesized { template }` — only for `delivery_id`; supports
//!     `${header.<name>}`, `${header_pair.<h>.<k>}`,
//!     `${signature_prefix:<N>}` substitutions
//!
//! Compile-time fail-closed: if `timestamp` is absent, `dedupe` MUST
//! be present. Secrets are read from `secret_env` at compile time only.

use std::sync::Arc;

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContract, RouteInvocationContext,
    RouteInvocationOutput, RouteInvocationResult, RoutePrincipal,
};
use crate::routes::webhook_dedupe::{DedupeConfig, DedupeOutcome};

const FORBIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "proxy-authorization",
    "proxy-authenticate",
];

// ── Compile function ─────────────────────────────────────────────────────

/// Compile an HMAC verifier from route auth_config.
///
/// Validates config, reads the secret from env, builds the compiled verifier.
/// Called once at route-table-build time.
pub fn compile_hmac_verifier(
    route_id: &str,
    auth_config: &Value,
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
    let cfg: RawHmacConfig = serde_json::from_value(auth_config.clone()).map_err(|e| {
        RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: "hmac_verifier".into(),
            reason: format!("{e}"),
        }
    })?;

    if cfg.secret_env.is_empty() {
        return Err(cfg_err(
            route_id,
            "secret_env must be a non-empty environment variable name",
        ));
    }
    let secret = std::env::var(&cfg.secret_env).map_err(|_| {
        cfg_err(
            route_id,
            &format!(
                "environment variable '{}' is not set; secret must be \
                 provided at daemon start",
                cfg.secret_env
            ),
        )
    })?;
    if secret.is_empty() {
        return Err(cfg_err(
            route_id,
            &format!("environment variable '{}' is empty", cfg.secret_env),
        ));
    }

    if cfg.signed_payload.is_empty() {
        return Err(cfg_err(route_id, "signed_payload must not be empty"));
    }
    let signed_payload: Vec<SignedPayloadItem> = cfg
        .signed_payload
        .iter()
        .enumerate()
        .map(|(i, raw_item)| compile_signed_payload_item(route_id, i, raw_item))
        .collect::<Result<_, _>>()?;

    let signature = compile_signature_spec(route_id, &cfg.signature)?;

    let timestamp = match &cfg.timestamp {
        Some(ts) => Some(compile_timestamp_spec(route_id, ts)?),
        None => None,
    };
    let delivery_id = match &cfg.delivery_id {
        Some(d) => Some(compile_delivery_id_spec(route_id, d)?),
        None => None,
    };

    let forwarded_headers = compile_forwarded_headers(route_id, &cfg.forwarded_headers)?;

    let dedupe = match &cfg.dedupe {
        Some(d) => Some(compile_dedupe_spec(route_id, d)?),
        None => None,
    };

    // Fail-closed: at least one replay-protection mechanism is required.
    if timestamp.is_none() && dedupe.is_none() {
        return Err(cfg_err(
            route_id,
            "at least one of `timestamp` or `dedupe` must be configured \
             to provide replay protection",
        ));
    }
    if dedupe.is_some() && delivery_id.is_none() {
        return Err(cfg_err(
            route_id,
            "`dedupe` requires `delivery_id` to be configured",
        ));
    }

    Ok(Arc::new(CompiledHmacVerifier {
        secret_bytes: secret.into_bytes(),
        signed_payload,
        signature,
        timestamp,
        delivery_id,
        forwarded_headers,
        dedupe,
    }))
}

// ── Raw (deserialization) shapes ─────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHmacConfig {
    secret_env: String,
    signed_payload: Vec<RawSignedPayloadItem>,
    signature: RawSignatureSpec,
    #[serde(default)]
    timestamp: Option<RawTimestampSpec>,
    #[serde(default)]
    delivery_id: Option<RawDeliveryIdSpec>,
    #[serde(default)]
    forwarded_headers: Vec<String>,
    #[serde(default)]
    dedupe: Option<RawDedupeSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSignedPayloadItem {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    literal: Option<String>,
    #[serde(default)]
    template: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSignatureSpec {
    header: String,
    encoding: String,
    select: RawSignatureSelect,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSignatureSelect {
    kind: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTimestampSpec {
    extract: RawValueExtract,
    tolerance_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeliveryIdSpec {
    extract: RawValueExtract,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawValueExtract {
    from: String,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    template: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDedupeSpec {
    ttl_secs: u64,
    max_entries: usize,
}

// ── Compiled (runtime) shapes ────────────────────────────────────────

enum SignedPayloadItem {
    Literal(Vec<u8>),
    Body,
    Header(axum::http::HeaderName),
    HeaderPair {
        header: axum::http::HeaderName,
        key: String,
    },
    BodyJsonPath {
        path: String,
    },
}

#[derive(Debug, Clone, Copy)]
enum Encoding {
    HexLower,
    HexUpper,
    Base64,
}

enum SignatureSelect {
    PairValues { key: String },
    Prefix { prefix: String },
    Exact,
}

struct SignatureSpec {
    header: axum::http::HeaderName,
    encoding: Encoding,
    select: SignatureSelect,
}

struct TimestampSpec {
    extract: ValueExtract,
    tolerance_secs: u64,
}

struct DeliveryIdSpec {
    extract: ValueExtract,
}

enum ValueExtract {
    Header(axum::http::HeaderName),
    HeaderPair {
        header: axum::http::HeaderName,
        key: String,
    },
    BodyJsonPath {
        path: String,
    },
    Synthesized(Vec<TemplatePart>),
}

enum TemplatePart {
    Literal(String),
    Header(axum::http::HeaderName),
    HeaderPair {
        header: axum::http::HeaderName,
        key: String,
    },
    SignaturePrefix(usize),
}

// ── Compiled verifier ────────────────────────────────────────────────────

pub(crate) struct CompiledHmacVerifier {
    secret_bytes: Vec<u8>,
    signed_payload: Vec<SignedPayloadItem>,
    signature: SignatureSpec,
    timestamp: Option<TimestampSpec>,
    delivery_id: Option<DeliveryIdSpec>,
    forwarded_headers: Vec<axum::http::HeaderName>,
    dedupe: Option<DedupeConfig>,
}

static HMAC_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Principal,
    principal: PrincipalPolicy::Forbidden,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledHmacVerifier {
    fn contract(&self) -> &'static RouteInvocationContract {
        &HMAC_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let route_id = &ctx.route_id;
        let headers = &ctx.headers;
        let body_raw = &ctx.body_raw;
        let state = &ctx.state;

        let now_unix = current_unix_secs().map_err(|e| {
            tracing::error!(route_id = %route_id, error = %e, "hmac clock read failed");
            RouteDispatchError::Unauthorized
        })?;

        // 1. Build signed payload bytes.
        let signed_bytes = match self.build_signed_payload(headers, body_raw) {
            Ok(b) => b,
            Err(reason) => {
                tracing::warn!(route_id = %route_id, error = %reason, "hmac signed_payload build failed");
                return Err(RouteDispatchError::Unauthorized);
            }
        };

        // 2. Compute HMAC-SHA256.
        let expected_bytes = hmac_sha256(&self.secret_bytes, &signed_bytes);

        // 3. Read signature header + extract candidates + match.
        let raw_header_value = match read_header(headers, &self.signature.header) {
            Ok(v) => v,
            Err(reason) => {
                tracing::warn!(route_id = %route_id, error = %reason, "hmac signature header missing");
                return Err(RouteDispatchError::Unauthorized);
            }
        };
        let candidates: Vec<&str> = match &self.signature.select {
            SignatureSelect::PairValues { key } => {
                let mut out = Vec::new();
                for part in raw_header_value.split(',') {
                    let part = part.trim();
                    if let Some((k, v)) = part.split_once('=') {
                        if k == key {
                            out.push(v);
                        }
                    }
                }
                if out.is_empty() {
                    tracing::warn!(
                        route_id = %route_id,
                        key = key.as_str(),
                        "hmac signature header has no matching key"
                    );
                    return Err(RouteDispatchError::Unauthorized);
                }
                out
            }
            SignatureSelect::Prefix { prefix } => {
                let v = raw_header_value
                    .strip_prefix(prefix.as_str())
                    .ok_or_else(|| {
                        tracing::warn!(
                            route_id = %route_id,
                            prefix = prefix.as_str(),
                            "hmac signature header missing prefix"
                        );
                        RouteDispatchError::Unauthorized
                    })?;
                vec![v]
            }
            SignatureSelect::Exact => vec![raw_header_value],
        };

        let expected_encoded = encode_signature(&expected_bytes, self.signature.encoding);
        let mut matched = false;
        for cand in &candidates {
            if ct_eq_str(cand, &expected_encoded) {
                matched = true;
                break;
            }
        }
        if !matched {
            tracing::warn!(route_id = %route_id, "hmac signature mismatch");
            return Err(RouteDispatchError::Unauthorized);
        }

        // 4. Timestamp window (optional).
        if let Some(ts_spec) = &self.timestamp {
            let v = match self.extract_value(headers, body_raw, &ts_spec.extract, raw_header_value) {
                Ok(v) => v,
                Err(reason) => {
                    tracing::warn!(route_id = %route_id, error = %reason, "hmac timestamp extract failed");
                    return Err(RouteDispatchError::Unauthorized);
                }
            };
            let ts: u64 = v.parse().map_err(|_| {
                tracing::warn!(route_id = %route_id, value = v.as_str(), "hmac timestamp parse failed");
                RouteDispatchError::Unauthorized
            })?;
            if let Err(reason) = check_timestamp_window(ts, now_unix, ts_spec.tolerance_secs) {
                tracing::warn!(route_id = %route_id, error = %reason, "hmac timestamp out of window");
                return Err(RouteDispatchError::Unauthorized);
            }
        }

        // 5. Delivery id (optional).
        let delivery_id = match &self.delivery_id {
            Some(d) => {
                let v = self
                    .extract_value(headers, body_raw, &d.extract, raw_header_value)
                    .map_err(|reason| {
                        tracing::warn!(route_id = %route_id, error = %reason, "hmac delivery_id extract failed");
                        RouteDispatchError::Unauthorized
                    })?;
                Some(v)
            }
            None => None,
        };

        // 6. Dedupe (optional).
        if let Some(dedupe_cfg) = &self.dedupe {
            let did = delivery_id.as_deref().expect(
                "dedupe is configured but delivery_id is absent — \
                 compile-time validation must enforce both together",
            );
            match state.webhook_dedupe.mark_seen(
                crate::routes::webhook_dedupe::RouteDedupeNamespace::for_route(route_id),
                did,
                now_unix,
                *dedupe_cfg,
            ) {
                DedupeOutcome::Fresh => {}
                DedupeOutcome::Replay => {
                    tracing::warn!(
                        route_id = %route_id,
                        delivery_id = did,
                        "hmac webhook replay rejected"
                    );
                    return Err(RouteDispatchError::Unauthorized);
                }
            }
        }

        // 7. Build principal metadata.
        let mut metadata = std::collections::BTreeMap::new();
        if let Some(did) = delivery_id {
            metadata.insert("delivery_id".into(), did);
        }
        for name in &self.forwarded_headers {
            if let Some(v) = headers.get(name) {
                if let Ok(s) = v.to_str() {
                    metadata.insert(format!("header.{}", name.as_str()), s.into());
                }
            }
        }

        Ok(RouteInvocationResult::Principal(RoutePrincipal {
            id: format!("webhook:hmac:{}", route_id),
            scopes: vec![],
            verifier_key: "hmac",
            verified: true,
            metadata,
        }))
    }
}

impl CompiledHmacVerifier {
    fn build_signed_payload(
        &self,
        headers: &axum::http::HeaderMap,
        body_raw: &[u8],
    ) -> Result<Vec<u8>, String> {
        let mut out: Vec<u8> = Vec::with_capacity(body_raw.len() + 64);
        for item in &self.signed_payload {
            match item {
                SignedPayloadItem::Literal(b) => out.extend_from_slice(b),
                SignedPayloadItem::Body => out.extend_from_slice(body_raw),
                SignedPayloadItem::Header(name) => {
                    let v = read_header(headers, name)?;
                    out.extend_from_slice(v.as_bytes());
                }
                SignedPayloadItem::HeaderPair { header, key } => {
                    let v = read_header(headers, header)?;
                    let pv = extract_pair_value(v, key).ok_or_else(|| {
                        format!(
                            "header '{}' has no pair with key '{}'",
                            header.as_str(),
                            key
                        )
                    })?;
                    out.extend_from_slice(pv.as_bytes());
                }
                SignedPayloadItem::BodyJsonPath { path } => {
                    let v = extract_body_json_string(body_raw, path)?;
                    out.extend_from_slice(v.as_bytes());
                }
            }
        }
        Ok(out)
    }

    fn extract_value(
        &self,
        headers: &axum::http::HeaderMap,
        body_raw: &[u8],
        extract: &ValueExtract,
        raw_signature_header: &str,
    ) -> Result<String, String> {
        match extract {
            ValueExtract::Header(name) => Ok(read_header(headers, name)?.to_string()),
            ValueExtract::HeaderPair { header, key } => {
                let v = read_header(headers, header)?;
                extract_pair_value(v, key)
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        format!(
                            "header '{}' has no pair with key '{}'",
                            header.as_str(),
                            key
                        )
                    })
            }
            ValueExtract::BodyJsonPath { path } => {
                Ok(extract_body_json_string(body_raw, path)?.to_string())
            }
            ValueExtract::Synthesized(parts) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        TemplatePart::Literal(s) => out.push_str(s),
                        TemplatePart::Header(name) => {
                            out.push_str(read_header(headers, name)?);
                        }
                        TemplatePart::HeaderPair { header, key } => {
                            let v = read_header(headers, header)?;
                            let pv = extract_pair_value(v, key).ok_or_else(|| {
                                format!(
                                    "header '{}' has no pair with key '{}'",
                                    header.as_str(),
                                    key
                                )
                            })?;
                            out.push_str(pv);
                        }
                        TemplatePart::SignaturePrefix(n) => {
                            let prefix: String =
                                raw_signature_header.chars().take(*n).collect();
                            out.push_str(&prefix);
                        }
                    }
                }
                Ok(out)
            }
        }
    }
}

// ── Compile-time validation helpers ──────────────────────────────────────

fn cfg_err(route_id: &str, reason: &str) -> RouteConfigError {
    RouteConfigError::InvalidSourceConfig {
        id: route_id.into(),
        src: "hmac_verifier".into(),
        reason: reason.into(),
    }
}

fn validate_header_name(
    route_id: &str,
    field_label: &str,
    name: &str,
) -> Result<axum::http::HeaderName, RouteConfigError> {
    if name.is_empty() {
        return Err(cfg_err(route_id, &format!("{field_label} must not be empty")));
    }
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(cfg_err(
            route_id,
            &format!("{field_label} '{name}' must be lowercase ASCII"),
        ));
    }
    if FORBIDDEN_HEADERS.contains(&name) {
        return Err(cfg_err(
            route_id,
            &format!(
                "{field_label} '{name}' is on the forbidden list \
                 (sensitive header that must not be referenced)"
            ),
        ));
    }
    axum::http::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
        cfg_err(
            route_id,
            &format!("{field_label} '{name}' is not a valid HTTP header name: {e}"),
        )
    })
}

fn compile_signed_payload_item(
    route_id: &str,
    idx: usize,
    raw: &RawSignedPayloadItem,
) -> Result<SignedPayloadItem, RouteConfigError> {
    let label = format!("signed_payload[{idx}]");

    if raw.literal.is_some() && raw.from.is_some() {
        return Err(cfg_err(
            route_id,
            &format!("{label} must specify exactly one of `literal` or `from`"),
        ));
    }
    if let Some(literal) = &raw.literal {
        if raw.header.is_some()
            || raw.key.is_some()
            || raw.path.is_some()
            || raw.template.is_some()
        {
            return Err(cfg_err(
                route_id,
                &format!("{label} `literal` must not be combined with other fields"),
            ));
        }
        return Ok(SignedPayloadItem::Literal(literal.as_bytes().to_vec()));
    }

    let from = raw.from.as_deref().ok_or_else(|| {
        cfg_err(
            route_id,
            &format!("{label} must specify either `literal` or `from`"),
        )
    })?;

    if from != "synthesized" && raw.template.is_some() {
        return Err(cfg_err(
            route_id,
            &format!("{label} `template` is only valid with from=synthesized"),
        ));
    }

    match from {
        "body" => {
            if raw.header.is_some() || raw.key.is_some() || raw.path.is_some() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=body takes no other fields"),
                ));
            }
            Ok(SignedPayloadItem::Body)
        }
        "header" => {
            if raw.key.is_some() || raw.path.is_some() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=header only accepts `header`"),
                ));
            }
            let name = raw
                .header
                .as_ref()
                .ok_or_else(|| {
                    cfg_err(route_id, &format!("{label} from=header requires `header`"))
                })?;
            let h = validate_header_name(route_id, &format!("{label}.header"), name)?;
            Ok(SignedPayloadItem::Header(h))
        }
        "header_pair" => {
            if raw.path.is_some() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=header_pair only accepts `header` and `key`"),
                ));
            }
            let name = raw.header.as_ref().ok_or_else(|| {
                cfg_err(
                    route_id,
                    &format!("{label} from=header_pair requires `header`"),
                )
            })?;
            let h = validate_header_name(route_id, &format!("{label}.header"), name)?;
            let key = raw.key.as_ref().ok_or_else(|| {
                cfg_err(
                    route_id,
                    &format!("{label} from=header_pair requires `key`"),
                )
            })?;
            if key.is_empty() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=header_pair `key` must not be empty"),
                ));
            }
            Ok(SignedPayloadItem::HeaderPair {
                header: h,
                key: key.clone(),
            })
        }
        "body_json_path" => {
            if raw.header.is_some() || raw.key.is_some() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=body_json_path only accepts `path`"),
                ));
            }
            let path = raw.path.as_ref().ok_or_else(|| {
                cfg_err(
                    route_id,
                    &format!("{label} from=body_json_path requires `path`"),
                )
            })?;
            validate_json_path(route_id, &format!("{label}.path"), path)?;
            Ok(SignedPayloadItem::BodyJsonPath {
                path: path.clone(),
            })
        }
        "synthesized" => Err(cfg_err(
            route_id,
            &format!("{label} from=synthesized is not allowed in signed_payload"),
        )),
        other => Err(cfg_err(
            route_id,
            &format!(
                "{label} unknown `from` '{other}'; allowed: body, header, header_pair, body_json_path"
            ),
        )),
    }
}

fn validate_json_path(
    route_id: &str,
    field_label: &str,
    path: &str,
) -> Result<(), RouteConfigError> {
    if path.is_empty() {
        return Err(cfg_err(
            route_id,
            &format!("{field_label} must not be empty"),
        ));
    }
    if path.contains('.') {
        return Err(cfg_err(
            route_id,
            &format!(
                "{field_label} must be a single top-level key (nested paths are not supported in v1)"
            ),
        ));
    }
    Ok(())
}

fn compile_signature_spec(
    route_id: &str,
    raw: &RawSignatureSpec,
) -> Result<SignatureSpec, RouteConfigError> {
    let header = validate_header_name(route_id, "signature.header", &raw.header)?;
    let encoding = match raw.encoding.as_str() {
        "hex_lower" => Encoding::HexLower,
        "hex_upper" => Encoding::HexUpper,
        "base64" => Encoding::Base64,
        other => {
            return Err(cfg_err(
                route_id,
                &format!(
                    "signature.encoding '{other}' is not supported; allowed: hex_lower, hex_upper, base64"
                ),
            ));
        }
    };
    let select = match raw.select.kind.as_str() {
        "pair_values" => {
            if raw.select.prefix.is_some() {
                return Err(cfg_err(
                    route_id,
                    "signature.select kind=pair_values does not accept `prefix`",
                ));
            }
            let key = raw.select.key.as_ref().ok_or_else(|| {
                cfg_err(
                    route_id,
                    "signature.select kind=pair_values requires `key`",
                )
            })?;
            if key.is_empty() {
                return Err(cfg_err(
                    route_id,
                    "signature.select.key must not be empty",
                ));
            }
            SignatureSelect::PairValues { key: key.clone() }
        }
        "prefix" => {
            if raw.select.key.is_some() {
                return Err(cfg_err(
                    route_id,
                    "signature.select kind=prefix does not accept `key`",
                ));
            }
            let prefix = raw.select.prefix.as_ref().ok_or_else(|| {
                cfg_err(route_id, "signature.select kind=prefix requires `prefix`")
            })?;
            if prefix.is_empty() {
                return Err(cfg_err(
                    route_id,
                    "signature.select.prefix must not be empty",
                ));
            }
            SignatureSelect::Prefix {
                prefix: prefix.clone(),
            }
        }
        "exact" => {
            if raw.select.key.is_some() || raw.select.prefix.is_some() {
                return Err(cfg_err(
                    route_id,
                    "signature.select kind=exact takes no other fields",
                ));
            }
            SignatureSelect::Exact
        }
        other => {
            return Err(cfg_err(
                route_id,
                &format!(
                    "signature.select.kind '{other}' is not supported; \
                     allowed: pair_values, prefix, exact"
                ),
            ));
        }
    };
    Ok(SignatureSpec {
        header,
        encoding,
        select,
    })
}

fn compile_timestamp_spec(
    route_id: &str,
    raw: &RawTimestampSpec,
) -> Result<TimestampSpec, RouteConfigError> {
    if raw.tolerance_secs == 0 {
        return Err(cfg_err(route_id, "timestamp.tolerance_secs must be > 0"));
    }
    let extract = compile_value_extract(
        route_id,
        "timestamp.extract",
        &raw.extract,
        /*allow_synthesized=*/ false,
    )?;
    Ok(TimestampSpec {
        extract,
        tolerance_secs: raw.tolerance_secs,
    })
}

fn compile_delivery_id_spec(
    route_id: &str,
    raw: &RawDeliveryIdSpec,
) -> Result<DeliveryIdSpec, RouteConfigError> {
    let extract = compile_value_extract(
        route_id,
        "delivery_id.extract",
        &raw.extract,
        /*allow_synthesized=*/ true,
    )?;
    Ok(DeliveryIdSpec { extract })
}

fn compile_value_extract(
    route_id: &str,
    label: &str,
    raw: &RawValueExtract,
    allow_synthesized: bool,
) -> Result<ValueExtract, RouteConfigError> {
    match raw.from.as_str() {
        "header" => {
            if raw.key.is_some() || raw.path.is_some() || raw.template.is_some() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=header only accepts `header`"),
                ));
            }
            let name = raw.header.as_ref().ok_or_else(|| {
                cfg_err(route_id, &format!("{label} from=header requires `header`"))
            })?;
            let h = validate_header_name(route_id, &format!("{label}.header"), name)?;
            Ok(ValueExtract::Header(h))
        }
        "header_pair" => {
            if raw.path.is_some() || raw.template.is_some() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=header_pair only accepts `header` and `key`"),
                ));
            }
            let name = raw.header.as_ref().ok_or_else(|| {
                cfg_err(
                    route_id,
                    &format!("{label} from=header_pair requires `header`"),
                )
            })?;
            let h = validate_header_name(route_id, &format!("{label}.header"), name)?;
            let key = raw.key.as_ref().ok_or_else(|| {
                cfg_err(
                    route_id,
                    &format!("{label} from=header_pair requires `key`"),
                )
            })?;
            if key.is_empty() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=header_pair `key` must not be empty"),
                ));
            }
            Ok(ValueExtract::HeaderPair {
                header: h,
                key: key.clone(),
            })
        }
        "body_json_path" => {
            if raw.header.is_some() || raw.key.is_some() || raw.template.is_some() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=body_json_path only accepts `path`"),
                ));
            }
            let path = raw.path.as_ref().ok_or_else(|| {
                cfg_err(
                    route_id,
                    &format!("{label} from=body_json_path requires `path`"),
                )
            })?;
            validate_json_path(route_id, &format!("{label}.path"), path)?;
            Ok(ValueExtract::BodyJsonPath {
                path: path.clone(),
            })
        }
        "synthesized" => {
            if !allow_synthesized {
                return Err(cfg_err(
                    route_id,
                    &format!(
                        "{label} from=synthesized is only allowed for delivery_id"
                    ),
                ));
            }
            if raw.header.is_some() || raw.key.is_some() || raw.path.is_some() {
                return Err(cfg_err(
                    route_id,
                    &format!("{label} from=synthesized only accepts `template`"),
                ));
            }
            let template = raw.template.as_ref().ok_or_else(|| {
                cfg_err(
                    route_id,
                    &format!("{label} from=synthesized requires `template`"),
                )
            })?;
            let parts =
                parse_synthesized_template(route_id, &format!("{label}.template"), template)?;
            Ok(ValueExtract::Synthesized(parts))
        }
        other => Err(cfg_err(
            route_id,
            &format!(
                "{label} unknown `from` '{other}'; allowed: header, header_pair, body_json_path{}",
                if allow_synthesized { ", synthesized" } else { "" }
            ),
        )),
    }
}

fn parse_synthesized_template(
    route_id: &str,
    label: &str,
    template: &str,
) -> Result<Vec<TemplatePart>, RouteConfigError> {
    let mut parts: Vec<TemplatePart> = Vec::new();
    let mut rest = template;

    while !rest.is_empty() {
        match rest.find("${") {
            None => {
                parts.push(TemplatePart::Literal(rest.to_string()));
                break;
            }
            Some(start) => {
                if start > 0 {
                    parts.push(TemplatePart::Literal(rest[..start].to_string()));
                }
                let after_open = &rest[start + 2..];
                let end = after_open.find('}').ok_or_else(|| {
                    cfg_err(
                        route_id,
                        &format!("{label} has unterminated `${{...}}` placeholder"),
                    )
                })?;
                let inner = &after_open[..end];
                let part = parse_template_variable(route_id, label, inner)?;
                parts.push(part);
                rest = &after_open[end + 1..];
            }
        }
    }

    Ok(parts)
}

fn parse_template_variable(
    route_id: &str,
    label: &str,
    inner: &str,
) -> Result<TemplatePart, RouteConfigError> {
    if let Some(rest) = inner.strip_prefix("header.") {
        let h = validate_header_name(
            route_id,
            &format!("{label} variable `header.{rest}`"),
            rest,
        )?;
        return Ok(TemplatePart::Header(h));
    }
    if let Some(rest) = inner.strip_prefix("header_pair.") {
        let (h_name, k) = rest.split_once('.').ok_or_else(|| {
            cfg_err(
                route_id,
                &format!(
                    "{label} variable `header_pair.{rest}` must be `header_pair.<header>.<key>`"
                ),
            )
        })?;
        if k.is_empty() {
            return Err(cfg_err(
                route_id,
                &format!("{label} variable `header_pair.{rest}` has empty key"),
            ));
        }
        if k.contains('.') {
            return Err(cfg_err(
                route_id,
                &format!(
                    "{label} variable `header_pair.{rest}` key must not contain '.' (use a different separator)"
                ),
            ));
        }
        let h = validate_header_name(
            route_id,
            &format!("{label} variable `header_pair.{rest}`"),
            h_name,
        )?;
        return Ok(TemplatePart::HeaderPair {
            header: h,
            key: k.to_string(),
        });
    }
    if let Some(rest) = inner.strip_prefix("signature_prefix:") {
        let n: usize = rest.parse().map_err(|_| {
            cfg_err(
                route_id,
                &format!(
                    "{label} variable `signature_prefix:{rest}` must be a non-negative integer"
                ),
            )
        })?;
        if n == 0 {
            return Err(cfg_err(
                route_id,
                &format!("{label} variable `signature_prefix:0` must be > 0"),
            ));
        }
        return Ok(TemplatePart::SignaturePrefix(n));
    }
    Err(cfg_err(
        route_id,
        &format!(
            "{label} unknown variable `${{{inner}}}`; allowed: header.<name>, \
             header_pair.<header>.<key>, signature_prefix:<N>"
        ),
    ))
}

fn compile_forwarded_headers(
    route_id: &str,
    raw: &[String],
) -> Result<Vec<axum::http::HeaderName>, RouteConfigError> {
    let mut out = Vec::with_capacity(raw.len());
    for name in raw {
        let h = validate_header_name(route_id, "forwarded_headers entry", name)?;
        out.push(h);
    }
    Ok(out)
}

fn compile_dedupe_spec(
    route_id: &str,
    raw: &RawDedupeSpec,
) -> Result<DedupeConfig, RouteConfigError> {
    if raw.ttl_secs == 0 {
        return Err(cfg_err(route_id, "dedupe.ttl_secs must be > 0"));
    }
    if raw.max_entries == 0 {
        return Err(cfg_err(route_id, "dedupe.max_entries must be > 0"));
    }
    Ok(DedupeConfig {
        ttl_secs: raw.ttl_secs,
        max_entries: raw.max_entries,
    })
}

// ── Helpers ──────────────────────────────────────────────────────────

fn current_unix_secs() -> Result<u64, std::time::SystemTimeError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
}

fn read_header<'a>(
    headers: &'a axum::http::HeaderMap,
    name: &axum::http::HeaderName,
) -> Result<&'a str, String> {
    headers
        .get(name)
        .ok_or_else(|| format!("missing header '{}'", name.as_str()))?
        .to_str()
        .map_err(|_| format!("header '{}' is not valid utf-8", name.as_str()))
}

fn extract_pair_value<'a>(header_value: &'a str, key: &str) -> Option<&'a str> {
    for part in header_value.split(',') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            if k == key {
                return Some(v);
            }
        }
    }
    None
}

fn extract_body_json_string(body: &[u8], path: &str) -> Result<String, String> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| format!("body is not valid JSON: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "body is not a JSON object".to_string())?;
    let field = obj
        .get(path)
        .ok_or_else(|| format!("body is missing top-level key '{path}'"))?;
    let s = field
        .as_str()
        .ok_or_else(|| format!("body key '{path}' is not a JSON string"))?;
    Ok(s.to_string())
}

fn hmac_sha256(secret: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn encode_signature(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::HexLower => hex_encode(bytes, false),
        Encoding::HexUpper => hex_encode(bytes, true),
        Encoding::Base64 => base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

fn hex_encode(bytes: &[u8], upper: bool) -> String {
    const LOWER: &[u8; 16] = b"0123456789abcdef";
    const UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let alpha = if upper { UPPER } else { LOWER };
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(alpha[(b >> 4) as usize] as char);
        out.push(alpha[(b & 0x0f) as usize] as char);
    }
    out
}

fn ct_eq_str(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn check_timestamp_window(ts: u64, now: u64, tolerance: u64) -> Result<(), String> {
    let drift = now.abs_diff(ts);
    if drift > tolerance {
        return Err(format!(
            "timestamp drift {drift}s exceeds tolerance {tolerance}s"
        ));
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use std::collections::BTreeMap;

    fn header_map(items: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in items {
            let name = axum::http::HeaderName::try_from(*k).unwrap();
            h.insert(name, v.parse().unwrap());
        }
        h
    }

    fn expect_err<T>(r: Result<T, RouteConfigError>) -> RouteConfigError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    fn expect_dispatch_err(
        r: Result<RouteInvocationResult, RouteDispatchError>,
    ) -> RouteDispatchError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected error, got Ok(result)"),
        }
    }

    async fn with_secret_env<F, Fut, R>(env_var: &str, value: &str, f: F) -> R
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        std::env::set_var(env_var, value);
        let result = f(env_var.to_string()).await;
        std::env::remove_var(env_var);
        result
    }

    fn with_secret_env_sync<F: FnOnce(&str) -> R, R>(env_var: &str, value: &str, f: F) -> R {
        std::env::set_var(env_var, value);
        let result = f(env_var);
        std::env::remove_var(env_var);
        result
    }

    fn build_test_state() -> (tempfile::TempDir, crate::state::AppState) {
        std::env::set_var("HOSTNAME", "testhost");
        let tmpdir = tempfile::TempDir::new().unwrap();
        let state_root = tmpdir.path().join(".ai").join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let key_path = tmpdir.path().join("identity").join("node-key.pem");
        let config = crate::config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: runtime_db_path.clone(),
            uds_path: tmpdir.path().join("test.sock"),
            system_space_dir: tmpdir.path().to_path_buf(),
            node_signing_key_path: key_path.clone(),
            user_signing_key_path: tmpdir.path().join("user-key.pem"),
            require_auth: false,
            authorized_keys_dir: tmpdir.path().join("auth"),
        };
        let identity = crate::identity::NodeIdentity::create(&key_path).unwrap();
        let signer = std::sync::Arc::new(
            crate::state_store::NodeIdentitySigner::from_identity(&identity),
        );
        let write_barrier = crate::write_barrier::WriteBarrier::new();
        let state_store = std::sync::Arc::new(
            crate::state_store::StateStore::new(
                state_root,
                runtime_db_path,
                signer,
                write_barrier.clone(),
            )
            .unwrap(),
        );
        let kind_profiles = std::sync::Arc::new(
            crate::kind_profiles::KindProfileRegistry::load_defaults(),
        );
        let events = std::sync::Arc::new(
            crate::services::event_store::EventStoreService::new(state_store.clone()),
        );
        let threads = std::sync::Arc::new(
            crate::services::thread_lifecycle::ThreadLifecycleService::new(
                state_store.clone(),
                kind_profiles.clone(),
                events.clone(),
            )
            .expect("HOSTNAME not set in test environment"),
        );
        let commands = std::sync::Arc::new(
            crate::services::command_service::CommandService::new(
                state_store.clone(),
                kind_profiles,
                events.clone(),
            ),
        );
        let engine = ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            None,
            Vec::new(),
        );
        let snapshot = crate::node_config::NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![],
            verbs: vec![],
            aliases: vec![],
        };
        let test_vr = std::sync::Arc::new(ryeos_runtime::verb_registry::VerbRegistry::from_records(&[
            ryeos_runtime::verb_registry::VerbDef { name: "execute".into(), execute: None },
            ryeos_runtime::verb_registry::VerbDef { name: "fetch".into(), execute: None },
            ryeos_runtime::verb_registry::VerbDef { name: "sign".into(), execute: Some("tool:ryeos/core/sign".into()) },
        ]).unwrap());
        let test_ar = std::sync::Arc::new(ryeos_runtime::alias_registry::AliasRegistry::from_records(&[]).unwrap());
        let test_auth = std::sync::Arc::new(ryeos_runtime::authorizer::Authorizer::new(test_vr.clone()));
        let state = crate::state::AppState {
            config: std::sync::Arc::new(config),
            state_store,
            engine: std::sync::Arc::new(engine),
            identity: std::sync::Arc::new(identity),
            threads,
            events,
            event_streams: std::sync::Arc::new(crate::event_stream::ThreadEventHub::new(16)),
            commands,
            callback_tokens: std::sync::Arc::new(
                crate::execution::callback_token::CallbackCapabilityStore::new(),
            ),
            thread_auth: std::sync::Arc::new(
                crate::execution::callback_token::ThreadAuthStore::new(),
            ),
            write_barrier: std::sync::Arc::new(write_barrier),
            started_at: std::time::Instant::now(),
            started_at_iso: String::new(),
            catalog_health: crate::state::CatalogHealth {
                status: "ok".into(),
                missing_services: vec![],
            },
            services: std::sync::Arc::new(crate::service_registry::build_service_registry()),
            node_config: std::sync::Arc::new(snapshot.clone()),
            route_table: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::routes::build_route_table_or_bail(&snapshot).unwrap(),
            )),
            webhook_dedupe: std::sync::Arc::new(
                crate::routes::webhook_dedupe::WebhookDedupeStore::new(),
            ),
            vault: std::sync::Arc::new(crate::vault::EmptyVault),
            verb_registry: test_vr,
            alias_registry: test_ar,
            authorizer: test_auth,
            scheduler_db: std::sync::Arc::new(crate::scheduler::db::SchedulerDb::new_in_memory().unwrap()),
            scheduler_reload_tx: None,
        };
        (tmpdir, state)
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn make_invocation_ctx(
        state: crate::state::AppState,
        headers: HeaderMap,
        body_raw: &[u8],
    ) -> RouteInvocationContext {
        RouteInvocationContext {
            route_id: "r1".into(),
            method: axum::http::Method::POST,
            uri: "/hook/test".parse().unwrap(),
            captures: BTreeMap::new(),
            headers,
            body_raw: body_raw.to_vec(),
            input: serde_json::Value::Null,
            principal: None,
            state,
        }
    }

    // ── Stripe-style end-to-end ────────────────────────────────

    #[tokio::test]
    async fn stripe_style_valid_signature_accepted() {
        with_secret_env("RYEOSD_TEST_HMAC_STRIPE_OK", "stripe-secret", |env| async move {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [
                    { "from": "header_pair", "header": "stripe-signature", "key": "t" },
                    { "literal": "." },
                    { "from": "body" },
                ],
                "signature": {
                    "header": "stripe-signature",
                    "encoding": "hex_lower",
                    "select": { "kind": "pair_values", "key": "v1" },
                },
                "timestamp": {
                    "extract": { "from": "header_pair", "header": "stripe-signature", "key": "t" },
                    "tolerance_secs": 300,
                },
                "delivery_id": {
                    "extract": { "from": "body_json_path", "path": "id" },
                },
            });
            let compiled = compile_hmac_verifier("r1", &cfg).unwrap();
            let now = now_unix();
            let body = b"{\"id\":\"evt_1\"}";
            let mut sp = Vec::new();
            sp.extend_from_slice(now.to_string().as_bytes());
            sp.push(b'.');
            sp.extend_from_slice(body);
            let sig = hex_encode(&hmac_sha256(b"stripe-secret", &sp), false);
            let header = format!("t={now},v1={sig}");
            let headers = header_map(&[("stripe-signature", &header)]);
            let (_tmp, state) = build_test_state();
            let ctx = make_invocation_ctx(state, headers, body);
            let result = compiled.invoke(ctx).await.unwrap();
            match result {
                RouteInvocationResult::Principal(p) => {
                    assert!(p.verified);
                    assert_eq!(p.verifier_key, "hmac");
                    assert_eq!(p.id, "webhook:hmac:r1");
                    assert_eq!(
                        p.metadata.get("delivery_id").map(String::as_str),
                        Some("evt_1")
                    );
                }
                other => panic!("expected Principal, got {:?}", other.variant_name()),
            }
        }).await;
    }

    #[tokio::test]
    async fn stripe_style_wrong_secret_rejected() {
        with_secret_env("RYEOSD_TEST_HMAC_STRIPE_BAD", "real-secret", |env| async move {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [
                    { "from": "header_pair", "header": "stripe-signature", "key": "t" },
                    { "literal": "." },
                    { "from": "body" },
                ],
                "signature": {
                    "header": "stripe-signature",
                    "encoding": "hex_lower",
                    "select": { "kind": "pair_values", "key": "v1" },
                },
                "timestamp": {
                    "extract": { "from": "header_pair", "header": "stripe-signature", "key": "t" },
                    "tolerance_secs": 300,
                },
            });
            let compiled = compile_hmac_verifier("r1", &cfg).unwrap();
            let now = now_unix();
            let body = b"{}";
            let mut sp = Vec::new();
            sp.extend_from_slice(now.to_string().as_bytes());
            sp.push(b'.');
            sp.extend_from_slice(body);
            let bad_sig = hex_encode(&hmac_sha256(b"different", &sp), false);
            let header = format!("t={now},v1={bad_sig}");
            let headers = header_map(&[("stripe-signature", &header)]);
            let (_tmp, state) = build_test_state();
            let ctx = make_invocation_ctx(state, headers, body);
            let err = expect_dispatch_err(compiled.invoke(ctx).await);
            assert!(matches!(err, RouteDispatchError::Unauthorized));
        }).await;
    }

    // ── GitHub-style end-to-end ────────────────────────────────

    #[tokio::test]
    async fn github_style_valid_and_replay() {
        with_secret_env("RYEOSD_TEST_HMAC_GH_OK", "gh-secret", |env| async move {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [ { "from": "body" } ],
                "signature": {
                    "header": "x-hub-signature-256",
                    "encoding": "hex_lower",
                    "select": { "kind": "prefix", "prefix": "sha256=" },
                },
                "delivery_id": {
                    "extract": { "from": "header", "header": "x-github-delivery" },
                },
                "dedupe": { "ttl_secs": 600, "max_entries": 1024 },
                "forwarded_headers": [ "x-github-event" ],
            });
            let compiled = compile_hmac_verifier("r1", &cfg).unwrap();
            let body = b"{\"action\":\"opened\"}";
            let sig = hex_encode(&hmac_sha256(b"gh-secret", body), false);
            let headers = header_map(&[
                ("x-hub-signature-256", &format!("sha256={sig}")),
                ("x-github-delivery", "abc-123"),
                ("x-github-event", "push"),
            ]);
            let (_tmp, state) = build_test_state();

            let ctx = make_invocation_ctx(state.clone(), headers.clone(), body);
            let result = compiled.invoke(ctx).await.unwrap();
            match result {
                RouteInvocationResult::Principal(p) => {
                    assert_eq!(
                        p.metadata.get("delivery_id").map(String::as_str),
                        Some("abc-123")
                    );
                    assert_eq!(
                        p.metadata.get("header.x-github-event").map(String::as_str),
                        Some("push")
                    );
                }
                other => panic!("expected Principal, got {:?}", other.variant_name()),
            }

            // Replay → Unauthorized.
            let ctx2 = make_invocation_ctx(state, headers, body);
            let err = expect_dispatch_err(compiled.invoke(ctx2).await);
            assert!(matches!(err, RouteDispatchError::Unauthorized));
        }).await;
    }

    // ── Compile-time validation ────────────────────────────────

    #[test]
    fn compile_rejects_missing_auth_config() {
        let err = expect_err(compile_hmac_verifier("r1", &serde_json::json!(null)));
        // null deserializes as unit, not as the expected struct
        assert!(format!("{err}").contains("invalid"), "got: {err}");
    }

    #[test]
    fn compile_rejects_no_timestamp_no_dedupe() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_NO_REPLAY", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(
                format!("{err}").contains("timestamp` or `dedupe`"),
                "got: {err}"
            );
        });
    }

    // ── Stale timestamp ──────────────────────────────────────

    #[tokio::test]
    async fn stripe_style_stale_timestamp_rejected() {
        with_secret_env("RYEOSD_TEST_HMAC_STRIPE_STALE", "stripe-secret", |env| async move {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [
                    { "from": "header_pair", "header": "stripe-signature", "key": "t" },
                    { "literal": "." },
                    { "from": "body" },
                ],
                "signature": {
                    "header": "stripe-signature",
                    "encoding": "hex_lower",
                    "select": { "kind": "pair_values", "key": "v1" },
                },
                "timestamp": {
                    "extract": { "from": "header_pair", "header": "stripe-signature", "key": "t" },
                    "tolerance_secs": 30,
                },
            });
            let compiled = compile_hmac_verifier("r1", &cfg).unwrap();
            let now = now_unix();
            let stale = now.saturating_sub(600);
            let body = b"{}";
            let mut sp = Vec::new();
            sp.extend_from_slice(stale.to_string().as_bytes());
            sp.push(b'.');
            sp.extend_from_slice(body);
            let sig = hex_encode(&hmac_sha256(b"stripe-secret", &sp), false);
            let header = format!("t={stale},v1={sig}");
            let headers = header_map(&[("stripe-signature", &header)]);
            let (_tmp, state) = build_test_state();
            let ctx = make_invocation_ctx(state, headers, body);
            let err = expect_dispatch_err(compiled.invoke(ctx).await);
            assert!(matches!(err, RouteDispatchError::Unauthorized));
        }).await;
    }

    // ── Slack-style end-to-end ──────────────────────────────

    #[tokio::test]
    async fn slack_style_valid_signature_with_synthesized_delivery_id() {
        with_secret_env("RYEOSD_TEST_HMAC_SLACK_OK", "slack-secret", |env| async move {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [
                    { "literal": "v0:" },
                    { "from": "header", "header": "x-slack-request-timestamp" },
                    { "literal": ":" },
                    { "from": "body" },
                ],
                "signature": {
                    "header": "x-slack-signature",
                    "encoding": "hex_lower",
                    "select": { "kind": "prefix", "prefix": "v0=" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-slack-request-timestamp" },
                    "tolerance_secs": 300,
                },
                "delivery_id": {
                    "extract": {
                        "from": "synthesized",
                        "template": "${header.x-slack-request-timestamp}:${signature_prefix:20}",
                    },
                },
            });
            let compiled = compile_hmac_verifier("r1", &cfg).unwrap();
            let now = now_unix();
            let body = b"token=xxx&team_id=T1";
            let mut sp = Vec::new();
            sp.extend_from_slice(b"v0:");
            sp.extend_from_slice(now.to_string().as_bytes());
            sp.push(b':');
            sp.extend_from_slice(body);
            let sig = hex_encode(&hmac_sha256(b"slack-secret", &sp), false);
            let sig_header = format!("v0={sig}");
            let headers = header_map(&[
                ("x-slack-request-timestamp", &now.to_string()),
                ("x-slack-signature", &sig_header),
            ]);
            let (_tmp, state) = build_test_state();
            let ctx = make_invocation_ctx(state, headers, body);
            let result = compiled.invoke(ctx).await.unwrap();
            match result {
                RouteInvocationResult::Principal(p) => {
                    let did = p
                        .metadata
                        .get("delivery_id")
                        .map(String::as_str)
                        .expect("delivery_id present");
                    let expected_prefix: String = sig_header.chars().take(20).collect();
                    assert_eq!(did, format!("{}:{}", now, expected_prefix));
                }
                other => panic!("expected Principal, got {:?}", other.variant_name()),
            }
        }).await;
    }

    // ── More compile-time validation ────────────────────────

    #[test]
    fn compile_rejects_unset_env() {
        std::env::remove_var("RYEOSD_TEST_HMAC_UNSET_VAR");
        let cfg = serde_json::json!({
            "secret_env": "RYEOSD_TEST_HMAC_UNSET_VAR",
            "signed_payload": [{ "from": "body" }],
            "signature": {
                "header": "x-sig",
                "encoding": "hex_lower",
                "select": { "kind": "exact" },
            },
            "timestamp": {
                "extract": { "from": "header", "header": "x-ts" },
                "tolerance_secs": 30,
            },
        });
        let err = expect_err(compile_hmac_verifier("r1", &cfg));
        assert!(format!("{err}").contains("is not set"), "got: {err}");
    }

    #[test]
    fn compile_rejects_empty_signed_payload() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_EMPTY_PAYLOAD", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(
                format!("{err}").contains("signed_payload must not be empty"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn compile_rejects_uppercase_header_name() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_UC_HEADER", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [
                    { "from": "header", "header": "X-Sig" },
                ],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(format!("{err}").contains("must be lowercase ASCII"), "got: {err}");
        });
    }

    #[test]
    fn compile_rejects_forbidden_header_name() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_FORBID", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "authorization",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(format!("{err}").contains("forbidden list"), "got: {err}");
        });
    }

    #[test]
    fn compile_rejects_bad_encoding() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_BAD_ENC", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "rot13",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(format!("{err}").contains("encoding 'rot13'"), "got: {err}");
        });
    }

    #[test]
    fn compile_rejects_select_kind_pair_values_without_key() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_SELECT_NO_KEY", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "pair_values" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(format!("{err}").contains("requires `key`"), "got: {err}");
        });
    }

    #[test]
    fn compile_rejects_select_kind_prefix_without_prefix() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_SELECT_NO_PREFIX", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "prefix" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(format!("{err}").contains("requires `prefix`"), "got: {err}");
        });
    }

    #[test]
    fn compile_rejects_select_kind_unknown() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_SELECT_UNK", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "magic" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(format!("{err}").contains("select.kind 'magic'"), "got: {err}");
        });
    }

    #[test]
    fn compile_rejects_zero_tolerance() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_ZERO_TOL", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 0,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(
                format!("{err}").contains("tolerance_secs must be > 0"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn compile_rejects_zero_dedupe_ttl() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_ZERO_TTL", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "delivery_id": {
                    "extract": { "from": "header", "header": "x-id" },
                },
                "dedupe": { "ttl_secs": 0, "max_entries": 10 },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(
                format!("{err}").contains("dedupe.ttl_secs must be > 0"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn compile_rejects_zero_dedupe_capacity() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_ZERO_CAP", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "delivery_id": {
                    "extract": { "from": "header", "header": "x-id" },
                },
                "dedupe": { "ttl_secs": 60, "max_entries": 0 },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(
                format!("{err}").contains("dedupe.max_entries must be > 0"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn compile_rejects_dedupe_without_delivery_id() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_DEDUP_NO_DID", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "dedupe": { "ttl_secs": 60, "max_entries": 10 },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(
                format!("{err}").contains("requires `delivery_id`"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn compile_rejects_synthesized_in_signed_payload() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_SYN_PAYLOAD", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [
                    { "from": "synthesized", "template": "${header.x-foo}" },
                ],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(
                format!("{err}").contains("from=synthesized is not allowed in signed_payload"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn compile_rejects_nested_json_path() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_NESTED_PATH", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
                "delivery_id": {
                    "extract": { "from": "body_json_path", "path": "data.id" },
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(
                format!("{err}").contains("nested paths are not supported"),
                "got: {err}"
            );
        });
    }

    #[test]
    fn compile_rejects_unknown_template_variable() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_BAD_TEMPLATE", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
                "delivery_id": {
                    "extract": {
                        "from": "synthesized",
                        "template": "${request.body}",
                    },
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(format!("{err}").contains("unknown variable"), "got: {err}");
        });
    }

    #[test]
    fn compile_rejects_unterminated_template() {
        with_secret_env_sync("RYEOSD_TEST_HMAC_UNTERM_TEMPL", "x", |env| {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "timestamp": {
                    "extract": { "from": "header", "header": "x-ts" },
                    "tolerance_secs": 30,
                },
                "delivery_id": {
                    "extract": {
                        "from": "synthesized",
                        "template": "${header.x-foo",
                    },
                },
            });
            let err = expect_err(compile_hmac_verifier("r1", &cfg));
            assert!(format!("{err}").contains("unterminated"), "got: {err}");
        });
    }

    // ── Template parser ─────────────────────────────────────

    #[test]
    fn template_parser_accepts_known_variables() {
        let parts =
            parse_synthesized_template("r1", "test", "${header.x-foo}-${signature_prefix:8}-tail")
                .unwrap();
        assert_eq!(parts.len(), 4);
        assert!(matches!(parts[0], TemplatePart::Header(_)));
        assert!(matches!(parts[1], TemplatePart::Literal(ref s) if s == "-"));
        assert!(matches!(parts[2], TemplatePart::SignaturePrefix(8)));
        assert!(matches!(parts[3], TemplatePart::Literal(ref s) if s == "-tail"));
    }

    #[test]
    fn template_parser_accepts_header_pair() {
        let parts = parse_synthesized_template("r1", "test", "${header_pair.x-foo.k}").unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            TemplatePart::HeaderPair { header, key } => {
                assert_eq!(header.as_str(), "x-foo");
                assert_eq!(key, "k");
            }
            _ => panic!("expected header_pair"),
        }
    }

    // ── Helper function tests ───────────────────────────────

    #[test]
    fn helper_check_timestamp_window() {
        assert!(check_timestamp_window(100, 200, 300).is_ok());
        assert!(check_timestamp_window(200, 100, 300).is_ok());
        assert!(check_timestamp_window(0, 1000, 300).is_err());
    }

    #[test]
    fn helper_hex_encode_round_trip() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab], false), "00ffab");
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab], true), "00FFAB");
    }

    #[test]
    fn helper_extract_pair_value() {
        assert_eq!(extract_pair_value("a=1,b=2,c=3", "b"), Some("2"));
        assert_eq!(extract_pair_value(" a=1 , b=2 ", "b"), Some("2"));
        assert_eq!(extract_pair_value("a=1", "z"), None);
    }

    #[test]
    fn helper_extract_body_json_string_ok() {
        let s = extract_body_json_string(b"{\"id\":\"x\"}", "id").unwrap();
        assert_eq!(s, "x");
    }

    #[test]
    fn helper_extract_body_json_string_missing() {
        let err = extract_body_json_string(b"{\"other\":1}", "id").unwrap_err();
        assert!(err.contains("missing top-level key"), "got: {err}");
    }

    // ── Principal / dedupe integration ──────────────────────

    #[test]
    fn anonymous_principal_uses_btreemap() {
        let p = RoutePrincipal::anonymous("x".into(), "test");
        assert_eq!(p.metadata, BTreeMap::new());
    }

    #[tokio::test]
    async fn dedupe_isolated_per_route() {
        with_secret_env("RYEOSD_TEST_HMAC_DEDUP_ISO", "secret", |env| async move {
            let cfg = serde_json::json!({
                "secret_env": env,
                "signed_payload": [{ "from": "body" }],
                "signature": {
                    "header": "x-sig",
                    "encoding": "hex_lower",
                    "select": { "kind": "exact" },
                },
                "delivery_id": { "extract": { "from": "header", "header": "x-id" } },
                "dedupe": { "ttl_secs": 600, "max_entries": 64 },
            });
            let compiled_a = compile_hmac_verifier("route_a", &cfg).unwrap();
            let compiled_b = compile_hmac_verifier("route_b", &cfg).unwrap();
            let body = b"";
            let sig = hex_encode(&hmac_sha256(b"secret", body), false);
            let headers = header_map(&[("x-sig", &sig), ("x-id", "evt_1")]);
            let (_tmp, state) = build_test_state();

            // Same delivery_id on two unrelated routes — both fresh.
            // Each context uses the correct route_id so dedupe is isolated.
            let mut ctx_a = make_invocation_ctx(state.clone(), headers.clone(), body);
            ctx_a.route_id = "route_a".into();
            let result_a = compiled_a.invoke(ctx_a).await.unwrap();
            assert!(matches!(result_a, RouteInvocationResult::Principal(_)));

            let mut ctx_b = make_invocation_ctx(state.clone(), headers.clone(), body);
            ctx_b.route_id = "route_b".into();
            let result_b = compiled_b.invoke(ctx_b).await.unwrap();
            assert!(matches!(result_b, RouteInvocationResult::Principal(_)));

            // Second hit on route_a → replay.
            let mut ctx_a2 = make_invocation_ctx(state, headers, body);
            ctx_a2.route_id = "route_a".into();
            let err = expect_dispatch_err(compiled_a.invoke(ctx_a2).await);
            assert!(matches!(err, RouteDispatchError::Unauthorized));
        }).await;
    }
}
