//! `remote/configure` — add or update a remote node config.
//!
//! Configure never trusts the key a remote shows it on first contact.
//! The caller must pin the remote node key with a descriptor
//! (`--descriptor`) or an expected fingerprint (`--expect-fingerprint`),
//! both obtained out of band. Without either, configure refuses with
//! `remote_key_unpinned` and names the fingerprint it saw so the operator
//! can verify it and rerun.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Name for the remote.
    #[serde(default)]
    pub remote: Option<String>,
    /// URL of the remote node (HTTPS required except for loopback).
    #[serde(default)]
    pub url: Option<String>,
    /// Path to a remote descriptor YAML. The descriptor is a trust pin,
    /// not a credential; configure still verifies the live node matches it.
    #[serde(default)]
    pub descriptor: Option<PathBuf>,
    /// Expected remote node fingerprint (64 lowercase hex, the SHA-256 of
    /// the node's Ed25519 public key), obtained out of band. An
    /// alternative pin to `descriptor`; when both are given both must hold.
    #[serde(default)]
    pub expect_fingerprint: Option<String>,
}

/// Stable refusal code: no descriptor and no expected fingerprint.
pub const REMOTE_KEY_UNPINNED: &str = "remote_key_unpinned";
/// Stable refusal code: `expect_fingerprint` is not 64 lowercase hex.
pub const REMOTE_EXPECTED_FINGERPRINT_INVALID: &str = "remote_expected_fingerprint_invalid";
/// Stable refusal code: the live node fingerprint differs from the pin.
pub const REMOTE_KEY_PIN_MISMATCH: &str = "remote_key_pin_mismatch";

/// How the remote node key was pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    Descriptor,
    ExpectedFingerprint,
    DescriptorAndExpectedFingerprint,
}

impl PinSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PinSource::Descriptor => "descriptor",
            PinSource::ExpectedFingerprint => "expected_fingerprint",
            PinSource::DescriptorAndExpectedFingerprint => "descriptor_and_expected_fingerprint",
        }
    }
}

fn is_fingerprint_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Check the caller's expected fingerprint is well formed before any
/// network traffic.
pub fn validate_expected_fingerprint(expect_fingerprint: Option<&str>) -> Result<(), HandlerError> {
    match expect_fingerprint {
        Some(expected) if !is_fingerprint_hex(expected) => Err(HandlerError::Structured {
            code: REMOTE_EXPECTED_FINGERPRINT_INVALID.to_string(),
            status: 400,
            body: serde_json::json!({
                "code": REMOTE_EXPECTED_FINGERPRINT_INVALID,
                "message": "expect_fingerprint must be 64 lowercase hex characters (the SHA-256 of the remote node public key)",
                "expect_fingerprint": expected,
            }),
        }),
        _ => Ok(()),
    }
}

/// Decide whether the live remote key is pinned by the caller.
///
/// `descriptor_fingerprint` is the fingerprint a descriptor pins, already
/// checked against the live key by the caller. There is no first-seen
/// fallback: with neither pin the remote is refused.
pub fn resolve_pin(
    remote_name: &str,
    descriptor_fingerprint: Option<&str>,
    expect_fingerprint: Option<&str>,
    live_fingerprint: &str,
) -> Result<PinSource, HandlerError> {
    validate_expected_fingerprint(expect_fingerprint)?;
    if let Some(expected) = expect_fingerprint
        && expected != live_fingerprint
    {
        return Err(HandlerError::Structured {
            code: REMOTE_KEY_PIN_MISMATCH.to_string(),
            status: 409,
            body: serde_json::json!({
                "code": REMOTE_KEY_PIN_MISMATCH,
                "message": format!(
                    "remote '{remote_name}' reports node fingerprint {live_fingerprint}, but expect_fingerprint pins {expected}; nothing was configured",
                ),
                "remote": remote_name,
                "expected_fingerprint": expected,
                "seen_fingerprint": live_fingerprint,
            }),
        });
    }
    match (descriptor_fingerprint, expect_fingerprint) {
        (Some(_), Some(_)) => Ok(PinSource::DescriptorAndExpectedFingerprint),
        (Some(_), None) => Ok(PinSource::Descriptor),
        (None, Some(_)) => Ok(PinSource::ExpectedFingerprint),
        (None, None) => Err(HandlerError::Structured {
            code: REMOTE_KEY_UNPINNED.to_string(),
            status: 400,
            body: serde_json::json!({
                "code": REMOTE_KEY_UNPINNED,
                "message": format!(
                    "refusing to trust remote '{remote_name}' on first sight: it reports node fingerprint {live_fingerprint}. Verify that fingerprint with the remote operator out of band, then rerun with --expect-fingerprint {live_fingerprint}, or pass --descriptor <file> with the remote's descriptor",
                ),
                "remote": remote_name,
                "seen_fingerprint": live_fingerprint,
                "cures": [
                    "--descriptor <remote descriptor file>",
                    "--expect-fingerprint <64-hex fingerprint verified out of band>",
                ],
            }),
        }),
    }
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let descriptor = req
        .descriptor
        .as_deref()
        .map(config::RemoteDescriptor::from_path)
        .transpose()?;

    let remote_name = req
        .remote
        .clone()
        .or_else(|| descriptor.as_ref().and_then(|d| d.name.clone()))
        .context("remote name required: pass a remote name or descriptor.name")?;
    let remote_url_input = req
        .url
        .clone()
        .or_else(|| descriptor.as_ref().map(|d| d.url.clone()))
        .context("remote URL required: pass --url or descriptor.url")?;
    let remote_url = config::normalize_url(&remote_url_input)?;
    if let Some(descriptor) = descriptor.as_ref()
        && descriptor.url != remote_url
    {
        anyhow::bail!(
            "remote descriptor URL is not canonical; expected '{}'",
            remote_url,
        );
    }

    validate_expected_fingerprint(req.expect_fingerprint.as_deref())?;

    // Discover the remote's public key
    let client = RemoteClient::new(&remote_url, "", state.identity.clone());
    let pubkey = client.get_public_key().await?;
    pubkey.validate_identity_binding()?;

    if let Some(descriptor) = descriptor.as_ref() {
        if pubkey.signing_key != descriptor.node.public_key {
            anyhow::bail!(
                "remote descriptor key mismatch for '{}': descriptor pins {}, live node reports {}",
                remote_name,
                descriptor.node.public_key,
                pubkey.signing_key,
            );
        }
        if pubkey.fingerprint != descriptor.node.fingerprint {
            anyhow::bail!(
                "remote descriptor fingerprint mismatch for '{}': descriptor pins {}, live node reports {}",
                remote_name,
                descriptor.node.fingerprint,
                pubkey.fingerprint,
            );
        }
    }
    let pin_source = resolve_pin(
        &remote_name,
        descriptor.as_ref().map(|d| d.node.fingerprint.as_str()),
        req.expect_fingerprint.as_deref(),
        &pubkey.fingerprint,
    )?;

    // Fetch the remote's ingest-ignore rules — required for push to work
    // correctly. Fail hard if unavailable.
    let ingest_ignore = client.get_ingest_ignore().await?;

    let app_root = state.config.runtime_root();
    let mut remotes = config::load_remotes(app_root.as_path())?;
    let existing_project_bindings = if let Some(remote) = remotes.get(&remote_name) {
        remote.project_bindings.clone()
    } else {
        Default::default()
    };
    let vault_fp = pubkey.vault_fingerprint.clone();
    let signing_key = pubkey.signing_key.clone();
    let remote_config = config::RemoteConfig {
        name: remote_name.clone(),
        url: remote_url.clone(),
        principal_id: pubkey.principal_id.clone(),
        signing_key: pubkey.signing_key,
        site_id: pubkey.site_id.clone(),
        vault_fingerprint: pubkey.vault_fingerprint,
        ingest_ignore: Some(ingest_ignore),
        project_bindings: existing_project_bindings,
    };
    remote_config.validate()?;
    remotes.insert(remote_name.clone(), remote_config);
    config::save_remotes(app_root.as_path(), &remotes)?;

    Ok(serde_json::json!({
        "configured": remote_name,
        "scope": "operator",
        "config_path": config::remotes_config_path(app_root.as_path()),
        "url": remote_url,
        "principal_id": pubkey.principal_id,
        "signing_key": signing_key,
        "fingerprint": pubkey.fingerprint,
        "vault_fingerprint": vault_fp,
        "site_id": pubkey.site_id,
        "descriptor_verified": descriptor.is_some(),
        "pin_source": pin_source.as_str(),
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/configure",
    endpoint: "remote.configure",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/configure"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";
    const OTHER: &str = "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22";

    fn code(err: HandlerError) -> (String, u16, serde_json::Value) {
        match err {
            HandlerError::Structured { code, status, body } => (code, status, body),
            other => panic!("expected a structured refusal, got {other:?}"),
        }
    }

    #[test]
    fn configure_without_pin_is_refused_with_seen_fingerprint() {
        let (code, status, body) = code(resolve_pin("prod", None, None, LIVE).unwrap_err());
        assert_eq!(code, REMOTE_KEY_UNPINNED);
        assert_eq!(status, 400);
        assert_eq!(body["seen_fingerprint"], LIVE);
        let message = body["message"].as_str().unwrap();
        assert!(message.contains("--descriptor"), "{message}");
        assert!(
            message.contains(&format!("--expect-fingerprint {LIVE}")),
            "{message}"
        );
        assert_eq!(body["cures"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn configure_with_matching_fingerprint_pins() {
        assert_eq!(
            resolve_pin("prod", None, Some(LIVE), LIVE).unwrap(),
            PinSource::ExpectedFingerprint
        );
    }

    #[test]
    fn configure_with_descriptor_pins() {
        assert_eq!(
            resolve_pin("prod", Some(LIVE), None, LIVE).unwrap(),
            PinSource::Descriptor
        );
        assert_eq!(
            resolve_pin("prod", Some(LIVE), Some(LIVE), LIVE).unwrap(),
            PinSource::DescriptorAndExpectedFingerprint
        );
    }

    #[test]
    fn configure_with_wrong_fingerprint_is_refused() {
        let (refusal, status, body) =
            code(resolve_pin("prod", None, Some(OTHER), LIVE).unwrap_err());
        assert_eq!(refusal, REMOTE_KEY_PIN_MISMATCH);
        assert_eq!(status, 409);
        assert_eq!(body["expected_fingerprint"], OTHER);
        assert_eq!(body["seen_fingerprint"], LIVE);
        // A descriptor does not excuse a wrong expected fingerprint.
        let (refusal, _, _) = code(resolve_pin("prod", Some(LIVE), Some(OTHER), LIVE).unwrap_err());
        assert_eq!(refusal, REMOTE_KEY_PIN_MISMATCH);
    }

    #[test]
    fn malformed_expected_fingerprint_is_refused() {
        for bad in [
            "",
            "AA11AA11AA11AA11AA11AA11AA11AA11AA11AA11AA11AA11AA11AA11AA11AA11",
            "fp:aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11",
            "aa11",
        ] {
            let (code, status, _) = code(validate_expected_fingerprint(Some(bad)).unwrap_err());
            assert_eq!(code, REMOTE_EXPECTED_FINGERPRINT_INVALID, "{bad:?}");
            assert_eq!(status, 400);
        }
        validate_expected_fingerprint(Some(LIVE)).unwrap();
        validate_expected_fingerprint(None).unwrap();
    }

    #[test]
    fn request_accepts_expect_fingerprint_field() {
        let req: Request = serde_json::from_value(serde_json::json!({
            "remote": "prod",
            "url": "https://ryeos.example.com",
            "expect_fingerprint": LIVE,
        }))
        .unwrap();
        assert_eq!(req.expect_fingerprint.as_deref(), Some(LIVE));
    }
}
