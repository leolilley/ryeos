//! Controller-side Render file transport for a journal-claimed relay slot.
//!
//! The relay-file adapter operation requires a prior durable daemon claim.
//! In particular,
//! an ambiguous upload has no second PUT path here: its only possible
//! reconciliation is a bounded GET of the same immutable file slot. Neither
//! transfer completion nor a provider token is supervisor channel evidence.

use std::io::Read as _;

use anyhow::{Result, ensure};
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::MonotonicDeadline;
use ryeos_http_transport::{HttpClient, RequestBodySource};
use zeroize::Zeroizing;

use crate::activation_contact::proxy_request;
use crate::provider_spec::ProviderSpec;
use crate::proxy_route::{
    BoundConnectToken, MAX_CONNECT_RESPONSE_BYTES, ProxyOperation, bind_connect_response,
    connect_token_url,
};
use crate::{
    Settings, api_url, has_json_content_type, read_credential, send_api_request, validate_api_url,
};

const MAX_RELAY_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_UPLOAD_RESPONSE_BYTES: u64 = 16 * 1024;

/// An exact provider file path is supplied only by the controller transfer
/// journal after it has checked the original sealed bootstrap and occurrence.
/// These fields are private so activation cannot synthesize a relay operation.
pub(super) struct RenderRelayFileContact<'a> {
    network: &'a NetworkContext,
    provider_spec: &'a ProviderSpec,
    settings: &'a Settings,
    occurrence_id: &'a str,
    remote_path: &'a str,
    credential: Zeroizing<String>,
    deadline: MonotonicDeadline,
    cancellation: &'a NetworkCancellation,
    contacted: bool,
}

impl<'a> RenderRelayFileContact<'a> {
    /// Route and byte preflight must precede credential access. Construct one
    /// instance only under a journaled read or one-shot upload contact claim.
    pub(super) fn new(
        network: &'a NetworkContext,
        provider_spec: &'a ProviderSpec,
        settings: &'a Settings,
        occurrence_id: &'a str,
        remote_path: &'a str,
        deadline: MonotonicDeadline,
        cancellation: &'a NetworkCancellation,
    ) -> Result<Self> {
        ensure!(
            !deadline.has_elapsed(),
            "Render relay contact deadline elapsed"
        );
        for operation in [
            ProxyOperation::UploadFile { remote_path },
            ProxyOperation::DownloadFile { remote_path },
        ] {
            let route = match operation {
                ProxyOperation::UploadFile { .. } => provider_spec.relay_file_upload_route()?,
                ProxyOperation::DownloadFile { .. } => provider_spec.relay_file_download_route()?,
                ProxyOperation::RunStream => unreachable!(),
            };
            let (url, target) = api_url(
                provider_spec,
                route,
                Some(occurrence_id),
                settings,
                Some(remote_path),
            )?;
            validate_api_url(
                &url,
                &target.path_segments,
                target.owner_id_query.as_deref(),
                target.file_path_query.as_deref(),
            )?;
            ensure!(
                url == connect_token_url(occurrence_id, &settings.owner_id, operation)?,
                "signed Render relay token route differs from fixed proxy operation"
            );
        }
        Ok(Self {
            network,
            provider_spec,
            settings,
            occurrence_id,
            remote_path,
            credential: read_credential()?,
            deadline,
            cancellation,
            contacted: false,
        })
    }

    fn mint(&self, operation: ProxyOperation<'_>) -> Result<BoundConnectToken> {
        let route = match operation {
            ProxyOperation::UploadFile { remote_path } if remote_path == self.remote_path => {
                self.provider_spec.relay_file_upload_route()?
            }
            ProxyOperation::DownloadFile { remote_path } if remote_path == self.remote_path => {
                self.provider_spec.relay_file_download_route()?
            }
            _ => anyhow::bail!("Render relay may mint only its exact file slot"),
        };
        ensure!(
            !self.deadline.has_elapsed(),
            "Render relay contact deadline elapsed"
        );
        let (url, target) = api_url(
            self.provider_spec,
            route,
            Some(self.occurrence_id),
            self.settings,
            Some(self.remote_path),
        )?;
        validate_api_url(
            &url,
            &target.path_segments,
            target.owner_id_query.as_deref(),
            target.file_path_query.as_deref(),
        )?;
        ensure!(
            url == connect_token_url(self.occurrence_id, &self.settings.owner_id, operation)?,
            "signed Render relay token route changed"
        );
        let response = send_api_request(
            self.network,
            &url,
            "POST",
            None,
            &self.credential,
            self.deadline,
            self.settings,
            self.cancellation,
        )?;
        ensure!(
            response.status == 201 && has_json_content_type(&response.headers),
            "Render relay token mint did not return exact JSON success"
        );
        let mut bytes = Zeroizing::new(Vec::new());
        response
            .body
            .take(MAX_CONNECT_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut *bytes)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CONNECT_RESPONSE_BYTES,
            "Render relay token response exceeds bound"
        );
        bind_connect_response(
            &bytes,
            self.occurrence_id,
            &self.settings.region,
            operation,
            lillux::time::timestamp_millis(),
        )
    }

    /// One possible upload. A 2xx reply only reports a provider transport
    /// observation; the caller must separately read back the exact file.
    pub(super) fn upload_once(&mut self, bytes: &[u8], expected_sha256: &str) -> Result<()> {
        ensure!(
            !bytes.is_empty()
                && bytes.len() as u64 <= MAX_RELAY_FILE_BYTES
                && lillux::sha256_hex(bytes) == expected_sha256,
            "Render relay upload changed its exact committed bytes"
        );
        ensure!(
            !self.contacted,
            "Render relay contact object was already used"
        );
        self.contacted = true;
        let operation = ProxyOperation::UploadFile {
            remote_path: self.remote_path,
        };
        let token = self.mint(operation)?;
        let request = proxy_request(
            self.settings,
            self.occurrence_id,
            token,
            operation,
            RequestBodySource::from_bytes(bytes.to_vec()),
            "application/octet-stream",
            "application/json",
            MAX_UPLOAD_RESPONSE_BYTES,
            self.deadline,
            self.cancellation,
        )?;
        let response = HttpClient::new(self.network.clone()).execute(request)?;
        ensure!(
            (200..300).contains(&response.status),
            "Render relay file upload did not return transport success"
        );
        Ok(())
    }

    /// This GET may be repeated only after the daemon retained another read
    /// attempt on the *same* transfer. A missing, partial or oversized file is
    /// not a negative provider proof and never grants a replacement upload.
    pub(super) fn download_bounded(&mut self, maximum_file_bytes: u64) -> Result<Vec<u8>> {
        ensure!(
            (1..=MAX_RELAY_FILE_BYTES).contains(&maximum_file_bytes),
            "Render relay download exceeds signed file bound"
        );
        ensure!(
            !self.contacted,
            "Render relay contact object was already used"
        );
        self.contacted = true;
        let operation = ProxyOperation::DownloadFile {
            remote_path: self.remote_path,
        };
        let token = self.mint(operation)?;
        let request = proxy_request(
            self.settings,
            self.occurrence_id,
            token,
            operation,
            RequestBodySource::from_bytes(Vec::new()),
            "application/octet-stream",
            "application/octet-stream",
            maximum_file_bytes + 1,
            self.deadline,
            self.cancellation,
        )?;
        let response = HttpClient::new(self.network.clone()).execute(request)?;
        ensure!(
            response.status == 200,
            "Render relay file read is absent or ambiguous"
        );
        let mut bytes = Vec::new();
        response
            .body
            .take(maximum_file_bytes + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= maximum_file_bytes,
            "Render relay file read is empty or exceeds bound"
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(
            ryeos_external_execution_contract::canonical_json(&value)? == bytes,
            "Render relay file is not complete canonical JSON"
        );
        Ok(bytes)
    }
}
