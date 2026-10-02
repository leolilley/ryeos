//! Exact, fail-closed Render Sandbox proxy destination policy.
//!
//! The pinned early-access CLI gives one run-URI example and uses a mock
//! upload URI of this shape. Neither is a normative provider guarantee. This
//! module does not contact that URI or establish that live Render will use it.
//! It only prevents a future activation implementation from forwarding a
//! short-lived bearer token to a different destination.

use anyhow::{Context as _, Result, anyhow, ensure};
use chrono::DateTime;
use serde::Deserialize;
use url::Url;
use zeroize::Zeroizing;

pub(crate) const MAX_CONNECT_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ConnectResponse {
    execution_id: String,
    expires_at: String,
    method: String,
    token: SecretToken,
    uri: String,
}

struct SecretToken(Zeroizing<String>);

impl<'de> Deserialize<'de> for SecretToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self(Zeroizing::new(String::deserialize(deserializer)?)))
    }
}

/// A validated, one-use proxy destination and short-lived bearer. The caller
/// must still hold the original activation mutation fence; this value does
/// not authorize a retry after an uncertain upload or run.
#[allow(dead_code)]
pub(crate) struct BoundConnectToken {
    pub(crate) execution_id: String,
    pub(crate) expires_at_ms: i64,
    pub(crate) method: String,
    pub(crate) route: Url,
    pub(crate) bearer: Zeroizing<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProxyOperation<'a> {
    UploadFile {
        remote_path: &'a str,
    },
    DownloadFile {
        remote_path: &'a str,
    },
    RunStream,
}

/// The exact Render API token-mint routes exposed by the pinned CLI. A token
/// mint can itself be contact-ambiguous, so this only constructs one request;
/// callers must retain their original durable activation claim and never
/// mint again during reconciliation.
#[allow(dead_code)]
pub(crate) fn connect_token_url(
    occurrence_id: &str,
    owner_id: &str,
    operation: ProxyOperation<'_>,
) -> Result<Url> {
    ensure!(
        valid_dns_label(occurrence_id, 128) && occurrence_id.starts_with("sbx-"),
        "Render token-mint occurrence is invalid"
    );
    ensure!(
        !owner_id.is_empty()
            && owner_id.len() <= 256
            && owner_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "Render token-mint owner is invalid"
    );
    let (suffix, remote_path) = match operation {
        ProxyOperation::UploadFile { remote_path } => {
            ensure!(
                valid_remote_path(remote_path),
                "Render upload path is invalid"
            );
            ("files/upload/token", Some(remote_path))
        }
        ProxyOperation::DownloadFile { remote_path } => {
            ensure!(
                valid_remote_path(remote_path),
                "Render download path is invalid"
            );
            ("files/download/token", Some(remote_path))
        }
        ProxyOperation::RunStream => ("runs/stream/token", None),
    };
    let mut url = Url::parse(&format!(
        "https://api.render.com/v1/sandboxes/{occurrence_id}/{suffix}"
    ))?;
    url.query_pairs_mut().append_pair("ownerId", owner_id);
    if let Some(remote_path) = remote_path {
        url.query_pairs_mut().append_pair("path", remote_path);
    }
    Ok(url)
}

#[allow(dead_code)]
pub(crate) fn bind_connect_response(
    response_bytes: &Zeroizing<Vec<u8>>,
    occurrence_id: &str,
    region: &str,
    operation: ProxyOperation<'_>,
    now_ms: i64,
) -> Result<BoundConnectToken> {
    ensure!(
        !response_bytes.is_empty() && response_bytes.len() <= MAX_CONNECT_RESPONSE_BYTES,
        "Render connect response exceeds its byte bound"
    );
    // This flat, deny-unknown-fields struct rejects duplicate known fields.
    // Deserialize directly so a generic Value cannot retain an unzeroized
    // copy of the bearer. Malformed-input errors are deliberately redacted.
    let mut decoder = serde_json::Deserializer::from_slice(response_bytes.as_slice());
    let mut response = ConnectResponse::deserialize(&mut decoder)
        .map_err(|_| anyhow!("Render connect response is invalid"))?;
    decoder
        .end()
        .map_err(|_| anyhow!("Render connect response has trailing data"))?;
    ensure!(
        valid_dns_label(&response.execution_id, 128)
            && response.execution_id.starts_with("exe-")
            && response.execution_id.len() > 4,
        "Render connect execution identity is invalid"
    );
    let expires_at_ms = DateTime::parse_from_rfc3339(&response.expires_at)
        .context("Render connect token expiry is invalid")?
        .timestamp_millis();
    ensure!(
        expires_at_ms > now_ms && expires_at_ms <= now_ms.saturating_add(24 * 60 * 60 * 1000),
        "Render connect token expiry is outside its bound"
    );
    ensure!(
        !response.token.0.is_empty()
            && response.token.0.len() <= 4096
            && response.token.0.bytes().all(|byte| byte.is_ascii_graphic()),
        "Render connect token is invalid"
    );
    let route = validate_proxy_route(
        &response.uri,
        &response.method,
        occurrence_id,
        region,
        operation,
    )?;
    Ok(BoundConnectToken {
        execution_id: std::mem::take(&mut response.execution_id),
        expires_at_ms,
        method: std::mem::take(&mut response.method),
        route,
        bearer: std::mem::replace(
            &mut response.token,
            SecretToken(Zeroizing::new(String::new())),
        )
        .0,
    })
}

pub(crate) fn validate_proxy_route(
    uri: &str,
    method: &str,
    occurrence_id: &str,
    region: &str,
    operation: ProxyOperation<'_>,
) -> Result<Url> {
    ensure!(
        valid_dns_label(occurrence_id, 128) && occurrence_id.starts_with("sbx-"),
        "Render proxy occurrence is invalid"
    );
    ensure!(
        valid_dns_label(region, 64),
        "Render proxy region is invalid"
    );
    ensure!(uri.len() <= 4096, "Render proxy URI exceeds its bound");

    // Construct rather than merely suffix-check the authority: the bearer
    // token must never be forwarded to a sibling Sandbox or another host.
    let host = format!("{occurrence_id}.{region}.sandbox.onrender.com");
    let mut expected = Url::parse(&format!("https://{host}/"))?;
    match operation {
        ProxyOperation::UploadFile { remote_path } => {
            ensure!(method == "PUT", "Render upload proxy method changed");
            ensure!(
                valid_remote_path(remote_path),
                "Render upload path is invalid"
            );
            expected.set_path("/files/upload");
            expected.query_pairs_mut().append_pair("path", remote_path);
        }
        ProxyOperation::DownloadFile { remote_path } => {
            ensure!(method == "GET", "Render download proxy method changed");
            ensure!(
                valid_remote_path(remote_path),
                "Render download path is invalid"
            );
            expected.set_path("/files/download");
            expected.query_pairs_mut().append_pair("path", remote_path);
        }
        ProxyOperation::RunStream => {
            ensure!(method == "POST", "Render run proxy method changed");
            expected.set_path("/runs/stream");
        }
    }
    // Exact serialization rejects credentials, ports, fragments, alternate
    // encodings, duplicate query parameters, and a changed provider route.
    ensure!(uri == expected.as_str(), "Render proxy URI changed");
    Ok(expected)
}

fn valid_dns_label(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_remote_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= 2048
        && path.split('/').skip(1).all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OCCURRENCE: &str = "sbx-abc123";
    const REGION: &str = "oregon";

    fn connect_response(uri: &str, method: &str, token: &str) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(
            serde_json::to_vec(&json!({
                "executionId": "exe-abc123",
                "expiresAt": "2026-09-26T00:15:00Z",
                "method": method,
                "token": token,
                "uri": uri,
            }))
            .unwrap(),
        )
    }

    #[test]
    fn connect_token_binds_exact_one_use_proxy_route() {
        let bytes = connect_response(
            "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
            "POST",
            "short-lived-token",
        );
        let bound = bind_connect_response(
            &bytes,
            OCCURRENCE,
            REGION,
            ProxyOperation::RunStream,
            1_790_380_800_000,
        )
        .unwrap();
        assert_eq!(bound.execution_id, "exe-abc123");
        assert_eq!(bound.method, "POST");
        assert_eq!(
            bound.route.as_str(),
            "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream"
        );
        assert_eq!(bound.bearer.as_str(), "short-lived-token");
    }

    #[test]
    fn token_mint_routes_bind_exact_owner_occurrence_and_operation() {
        let upload = connect_token_url(
            OCCURRENCE,
            "tea-owner_1",
            ProxyOperation::UploadFile {
                remote_path: "/ryeos/activation/guest-package",
            },
        )
        .unwrap();
        assert_eq!(
            upload.as_str(),
            "https://api.render.com/v1/sandboxes/sbx-abc123/files/upload/token?ownerId=tea-owner_1&path=%2Fryeos%2Factivation%2Fguest-package"
        );
        let run = connect_token_url(OCCURRENCE, "tea-owner_1", ProxyOperation::RunStream).unwrap();
        assert_eq!(
            run.as_str(),
            "https://api.render.com/v1/sandboxes/sbx-abc123/runs/stream/token?ownerId=tea-owner_1"
        );
        for (occurrence, owner, path) in [
            (
                "sbx-other/../sbx-abc123",
                "tea-owner_1",
                "/ryeos/activation/guest-package",
            ),
            (
                OCCURRENCE,
                "tea-owner_1&other=1",
                "/ryeos/activation/guest-package",
            ),
            (
                OCCURRENCE,
                "tea-owner_1",
                "/ryeos/activation/../guest-package",
            ),
        ] {
            assert!(
                connect_token_url(
                    occurrence,
                    owner,
                    ProxyOperation::UploadFile { remote_path: path },
                )
                .is_err()
            );
        }
    }

    #[test]
    fn download_token_binds_only_the_exact_occurrence_and_file() {
        let path = "/ryeos/activation/ready.json";
        let uri = "https://sbx-abc123.oregon.sandbox.onrender.com/files/download?path=%2Fryeos%2Factivation%2Fready.json";
        let bytes = connect_response(uri, "GET", "short-lived-token");
        let bound = bind_connect_response(
            &bytes,
            OCCURRENCE,
            REGION,
            ProxyOperation::DownloadFile { remote_path: path },
            1_790_380_800_000,
        )
        .unwrap();
        assert_eq!(bound.route.as_str(), uri);
        assert!(
            bind_connect_response(
                &bytes,
                OCCURRENCE,
                REGION,
                ProxyOperation::DownloadFile {
                    remote_path: "/ryeos/activation/other.json"
                },
                1_790_380_800_000,
            )
            .is_err()
        );
        assert!(
            bind_connect_response(
                &bytes,
                "sbx-other",
                REGION,
                ProxyOperation::DownloadFile { remote_path: path },
                1_790_380_800_000,
            )
            .is_err()
        );
        assert!(
            bind_connect_response(
                &bytes,
                OCCURRENCE,
                REGION,
                ProxyOperation::UploadFile { remote_path: path },
                1_790_380_800_000,
            )
            .is_err()
        );
    }

    #[test]
    fn connect_token_rejects_stale_secret_or_substituted_destination() {
        let route = "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream";
        let now_ms = 1_790_380_800_000;
        for (uri, method, token, now) in [
            (route, "POST", "short-lived-token", now_ms + 16 * 60 * 1000),
            (route, "POST", "with\r\nheader", now_ms),
            (
                "https://sbx-other.oregon.sandbox.onrender.com/runs/stream",
                "POST",
                "token",
                now_ms,
            ),
            (route, "PUT", "token", now_ms),
        ] {
            let bytes = connect_response(uri, method, token);
            assert!(
                bind_connect_response(&bytes, OCCURRENCE, REGION, ProxyOperation::RunStream, now)
                    .is_err()
            );
        }
    }

    #[test]
    fn connect_token_parser_rejects_duplicates_unknowns_and_trailing_data_without_echo() {
        let route = "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream";
        let response = format!(
            "{{\"executionId\":\"exe-abc123\",\"expiresAt\":\"2026-09-26T00:15:00Z\",\"method\":\"POST\",\"token\":\"private-sentinel\",\"uri\":\"{route}\"}}"
        );
        for raw in [
            response.replace("\"token\":", "\"token\":\"duplicate\",\"token\":"),
            response.replace("\"uri\":", "\"unknown\":1,\"uri\":"),
            format!("{response} true"),
        ] {
            let bytes = Zeroizing::new(raw.into_bytes());
            let error = bind_connect_response(
                &bytes,
                OCCURRENCE,
                REGION,
                ProxyOperation::RunStream,
                1_790_380_800_000,
            )
            .err()
            .expect("malformed connect response was accepted");
            assert!(!format!("{error:#}").contains("private-sentinel"));
        }
    }

    #[test]
    fn accepts_only_exact_example_proxy_routes() {
        assert!(
            validate_proxy_route(
                "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
                "POST",
                OCCURRENCE,
                REGION,
                ProxyOperation::RunStream,
            )
            .is_ok()
        );
        assert!(validate_proxy_route(
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2Fryeos-bootstrap",
            "PUT",
            OCCURRENCE,
            REGION,
            ProxyOperation::UploadFile { remote_path: "/tmp/ryeos-bootstrap" },
        )
        .is_ok());
    }

    #[test]
    fn rejects_proxy_authority_and_route_substitution() {
        let baseline = "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream";
        for candidate in [
            "http://sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
            "https://sbx-other.oregon.sandbox.onrender.com/runs/stream",
            "https://sbx-abc123.singapore.sandbox.onrender.com/runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com.evil.test/runs/stream",
            "https://user@sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com:444/runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com:443/runs/stream",
            "https://SBX-ABC123.oregon.sandbox.onrender.com/runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com./runs/stream",
            "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream?next=evil",
            "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream#fragment",
            "https://sbx-abc123.oregon.sandbox.onrender.com/runs/%73tream",
        ] {
            assert!(
                validate_proxy_route(
                    candidate,
                    "POST",
                    OCCURRENCE,
                    REGION,
                    ProxyOperation::RunStream
                )
                .is_err(),
                "accepted {candidate}"
            );
        }
        assert!(
            validate_proxy_route(
                baseline,
                "PUT",
                OCCURRENCE,
                REGION,
                ProxyOperation::RunStream
            )
            .is_err()
        );
        assert!(
            validate_proxy_route(
                baseline,
                "POST",
                "sbx-other",
                REGION,
                ProxyOperation::RunStream
            )
            .is_err()
        );
        assert!(
            validate_proxy_route(
                baseline,
                "POST",
                &format!("sbx-{}", "a".repeat(129)),
                REGION,
                ProxyOperation::RunStream
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_upload_path_and_query_ambiguity() {
        let baseline = "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2Fryeos-bootstrap";
        for path in [
            "tmp/file",
            "/",
            "//tmp/file",
            "/tmp/../file",
            "/tmp//file",
            "/tmp/a b",
        ] {
            assert!(
                validate_proxy_route(
                    baseline,
                    "PUT",
                    OCCURRENCE,
                    REGION,
                    ProxyOperation::UploadFile { remote_path: path },
                )
                .is_err()
            );
        }
        for uri in [
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2Fother",
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2Fryeos-bootstrap&path=%2Ftmp%2Fother",
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=/tmp/ryeos-bootstrap",
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/%75pload?path=%2Ftmp%2Fryeos-bootstrap",
            "https://sbx-abc123.oregon.sandbox.onrender.com/files/upload?path=%2Ftmp%2F%2e%2e%2Fryeos-bootstrap",
        ] {
            assert!(
                validate_proxy_route(
                    uri,
                    "PUT",
                    OCCURRENCE,
                    REGION,
                    ProxyOperation::UploadFile {
                        remote_path: "/tmp/ryeos-bootstrap"
                    },
                )
                .is_err()
            );
        }
        assert!(
            validate_proxy_route(
                "https://sbx-abc123.oregon.sandbox.onrender.com/runs/stream",
                "PUT",
                OCCURRENCE,
                REGION,
                ProxyOperation::UploadFile {
                    remote_path: "/tmp/ryeos-bootstrap"
                },
            )
            .is_err()
        );
    }
}
