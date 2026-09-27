//! One-shot Render guest activation contact, still disabled by the signed
//! provider profile until installed snapshot and lost-run-stream qualification.
//!
//! The daemon has already committed one durable activation claim before this
//! adapter is invoked. Every stage may have reached Render even if its reply
//! is lost. This module never retries and reconciliation must never call it.

use std::io::Read as _;

use anyhow::{Result, ensure};
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::MonotonicDeadline;
use ryeos_external_execution_contract::canonical_json;
use ryeos_http_transport::{
    Deadlines, Header, HttpClient, HttpRequest, HttpResponse, Limits, RequestBodySource,
};
use zeroize::Zeroizing;

use crate::proxy_route::{
    BoundConnectToken, MAX_CONNECT_RESPONSE_BYTES, ProxyOperation, bind_connect_response,
    connect_token_url, validate_proxy_route,
};
use crate::{
    ActivationDeliveryPlan, IDLE_TIMEOUT, SETUP_TIMEOUT, Settings, has_json_content_type,
    read_credential, send_api_request, tls_roots,
};

const MAX_UPLOAD_RESPONSE_BYTES: u64 = 16 * 1024;
const MAX_RUN_RESPONSE_BYTES: u64 = 1024 * 1024;

/// The test seam keeps order and at-most-once behavior independent of HTTP.
/// It is not an alternate lifecycle or a provider abstraction above this
/// Render adapter.
trait OneShotContact {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8], sha256: &str) -> Result<()>;
    fn upload_file(
        &mut self,
        path: &str,
        authority: &lillux::InheritedDescriptorAuthority,
        bytes: u64,
        sha256: &str,
    ) -> Result<()>;
    fn run(&mut self, command: &str) -> Result<()>;
}

fn contact_once(
    contact: &mut impl OneShotContact,
    delivery: &ActivationDeliveryPlan,
    signed_import: &[u8],
    package: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    ensure!(
        signed_import.len() as u64 == delivery.signed_import_bytes
            && lillux::sha256_hex(signed_import) == delivery.signed_import_sha256,
        "signed import changed before Render upload"
    );
    contact.upload_bytes(
        delivery.signed_import_remote_path,
        signed_import,
        &delivery.signed_import_sha256,
    )?;
    contact.upload_file(
        delivery.guest_package_remote_path,
        package,
        delivery.guest_package_bytes,
        &delivery.guest_package_sha256,
    )?;
    contact.run(&delivery.owner_command)
}

/// This function must only be called from the first claimed activation, never
/// reconciliation. Even a successful run response remains merely Pending:
/// only the independently authenticated supervisor channel can establish
/// Ready. The installed Render profile currently does not enable this call.
#[allow(dead_code)]
pub(crate) fn first_activation_contact(
    network: &NetworkContext,
    settings: &Settings,
    occurrence_id: &str,
    delivery: &ActivationDeliveryPlan,
    signed_import: &[u8],
    package: &lillux::InheritedDescriptorAuthority,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> Result<()> {
    let credential = read_credential()?;
    let mut contact = RenderContact {
        network,
        settings,
        occurrence_id,
        credential,
        deadline,
        cancellation,
    };
    contact_once(&mut contact, delivery, signed_import, package)
}

struct RenderContact<'a> {
    network: &'a NetworkContext,
    settings: &'a Settings,
    occurrence_id: &'a str,
    credential: Zeroizing<String>,
    deadline: MonotonicDeadline,
    cancellation: &'a NetworkCancellation,
}

impl RenderContact<'_> {
    fn mint(
        &self,
        operation: ProxyOperation<'_>,
        command: Option<&str>,
    ) -> Result<BoundConnectToken> {
        let url = connect_token_url(self.occurrence_id, &self.settings.owner_id, operation)?;
        let body = command
            .map(|command| canonical_json(&serde_json::json!({ "command": command })))
            .transpose()?;
        let response = send_api_request(
            self.network,
            &url,
            "POST",
            body,
            &self.credential,
            self.deadline,
            self.settings,
            self.cancellation,
        )?;
        ensure!(
            response.status == 201 && has_json_content_type(&response.headers),
            "Render token mint did not return the exact JSON success shape"
        );
        let mut bytes = Zeroizing::new(Vec::new());
        response
            .body
            .take(MAX_CONNECT_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut *bytes)
            .map_err(|_| anyhow::anyhow!("Render token mint body is unreadable"))?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CONNECT_RESPONSE_BYTES,
            "Render token mint body exceeds its bound"
        );
        bind_connect_response(
            &bytes,
            self.occurrence_id,
            &self.settings.region,
            operation,
            lillux::time::timestamp_millis(),
        )
    }

    fn send_proxy(
        &self,
        token: BoundConnectToken,
        operation: ProxyOperation<'_>,
        body: RequestBodySource,
        content_type: &'static str,
        accept: &'static str,
        response_limit: u64,
    ) -> Result<HttpResponse> {
        ensure!(
            token.expires_at_ms > lillux::time::timestamp_millis(),
            "Render proxy token expired before use"
        );
        validate_proxy_route(
            token.route.as_str(),
            &token.method,
            self.occurrence_id,
            &self.settings.region,
            operation,
        )?;
        let mut authorization = Zeroizing::new(b"Bearer ".to_vec());
        authorization.extend_from_slice(token.bearer.as_bytes());
        let mut limits = Limits::control_plane();
        limits.request_body_bytes = body.exact_len();
        limits.response_body_bytes = response_limit;
        limits.response_body_wire_bytes = response_limit.saturating_mul(2);
        let request = HttpRequest {
            method: token.method,
            url: token.route,
            headers: vec![
                Header::new_sensitive("Authorization", authorization),
                Header::new("Content-Type", content_type),
                Header::new("Accept", accept),
            ],
            body,
            tls_roots_der: tls_roots(self.settings)?,
            limits,
            deadlines: Deadlines::new(SETUP_TIMEOUT, IDLE_TIMEOUT, self.deadline),
            cancellation: self.cancellation.clone(),
        };
        HttpClient::new(self.network.clone())
            .execute(request)
            .map_err(anyhow::Error::from)
    }
}

impl OneShotContact for RenderContact<'_> {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8], sha256: &str) -> Result<()> {
        ensure!(
            bytes.len()
                <= ryeos_external_execution_contract::guest_import_authorization::MAX_GUEST_IMPORT_AUTHORIZATION_BYTES
                    + 256
                && lillux::sha256_hex(bytes) == sha256,
            "Render signed-import upload changed its exact bytes"
        );
        let operation = ProxyOperation::UploadFile { remote_path: path };
        let token = self.mint(operation, None)?;
        let response = self.send_proxy(
            token,
            operation,
            RequestBodySource::from_bytes(bytes.to_vec()),
            "application/octet-stream",
            "application/json",
            MAX_UPLOAD_RESPONSE_BYTES,
        )?;
        ensure!(
            (200..300).contains(&response.status),
            "Render signed-import upload did not succeed"
        );
        Ok(())
    }

    fn upload_file(
        &mut self,
        path: &str,
        authority: &lillux::InheritedDescriptorAuthority,
        bytes: u64,
        sha256: &str,
    ) -> Result<()> {
        let operation = ProxyOperation::UploadFile { remote_path: path };
        let token = self.mint(operation, None)?;
        let response = self.send_proxy(
            token,
            operation,
            RequestBodySource::from_inherited_regular_file(authority.clone(), bytes, sha256.into()),
            "application/octet-stream",
            "application/json",
            MAX_UPLOAD_RESPONSE_BYTES,
        )?;
        ensure!(
            (200..300).contains(&response.status),
            "Render exact guest-package upload did not succeed"
        );
        Ok(())
    }

    fn run(&mut self, command: &str) -> Result<()> {
        let operation = ProxyOperation::RunStream;
        let token = self.mint(operation, Some(command))?;
        let body = canonical_json(&serde_json::json!({ "command": command }))?;
        let response = self.send_proxy(
            token,
            operation,
            RequestBodySource::from_bytes(body),
            "application/json",
            "text/event-stream",
            MAX_RUN_RESPONSE_BYTES,
        )?;
        ensure!(
            response.status == 200,
            "Render run proxy did not return the expected stream status"
        );
        // Dropping the stream is deliberate only after an installed test proves
        // the guest owner survives lost client streams. This code is unreachable
        // from the current signed provider profile.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingContact {
        calls: Vec<&'static str>,
        fail_at: Option<&'static str>,
    }

    impl CountingContact {
        fn record(&mut self, stage: &'static str) -> Result<()> {
            self.calls.push(stage);
            ensure!(self.fail_at != Some(stage), "simulated lost response");
            Ok(())
        }
    }

    impl OneShotContact for CountingContact {
        fn upload_bytes(&mut self, _: &str, _: &[u8], _: &str) -> Result<()> {
            self.record("import")
        }
        fn upload_file(
            &mut self,
            _: &str,
            _: &lillux::InheritedDescriptorAuthority,
            _: u64,
            _: &str,
        ) -> Result<()> {
            self.record("package")
        }
        fn run(&mut self, _: &str) -> Result<()> {
            self.record("run")
        }
    }

    #[test]
    fn one_shot_sequence_stops_at_first_uncertain_contact() {
        let bytes = b"signed-import";
        let plan = ActivationDeliveryPlan {
            signed_import_remote_path: "/ryeos/activation/signed-import.json",
            signed_import_sha256: lillux::sha256_hex(bytes),
            signed_import_bytes: bytes.len() as u64,
            guest_package_remote_path: "/ryeos/activation/guest-package",
            guest_package_sha256: "a".repeat(64),
            guest_package_bytes: 1,
            owner_command: "exec /ryeos/guest-runtime/bin/owner --assignment-b64 abc".into(),
        };
        let parent = tempfile::tempdir().unwrap();
        let source = lillux::PinnedDirectory::open(parent.path())
            .unwrap()
            .unwrap();
        std::fs::write(parent.path().join("package"), b"x").unwrap();
        let package = source
            .open_pinned_regular(std::ffi::OsStr::new("package"), false)
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        for (fail_at, expected) in [
            (Some("import"), vec!["import"]),
            (Some("package"), vec!["import", "package"]),
            (Some("run"), vec!["import", "package", "run"]),
            (None, vec!["import", "package", "run"]),
        ] {
            let mut contact = CountingContact {
                calls: Vec::new(),
                fail_at,
            };
            let result = contact_once(&mut contact, &plan, bytes, &package);
            assert_eq!(result.is_err(), fail_at.is_some());
            assert_eq!(contact.calls, expected);
        }
        let mut contact = CountingContact {
            calls: Vec::new(),
            fail_at: None,
        };
        assert!(contact_once(&mut contact, &plan, b"changed", &package).is_err());
        assert!(contact.calls.is_empty());
    }
}
