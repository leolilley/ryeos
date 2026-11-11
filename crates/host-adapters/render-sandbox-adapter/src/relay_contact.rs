//! Controller-side Render file transport for a journal-claimed relay slot.
//!
//! The relay-file adapter operation requires a prior durable daemon claim.
//! In particular,
//! an ambiguous upload has no second PUT path here: its only possible
//! reconciliation is a bounded GET of the same immutable file slot. Neither
//! transfer completion nor a provider token is supervisor channel evidence.

use crate::fail_class::{Classify as _, FailClass, ensure_class};
use std::io::Read as _;

use anyhow::{Context as _, Result, ensure};
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::MonotonicDeadline;
use ryeos_external_execution_contract::relay_file::RelayDeliveryAccount;
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

/// One-member USTAR archive. The provider extracts it into the mailbox keeping
/// the member's owner and mode; a plain file upload would instead be owned by
/// the provider's file service and unreadable to the guest account.
fn delivery_archive(
    file_name: &str,
    bytes: &[u8],
    account: RelayDeliveryAccount,
) -> Result<Vec<u8>> {
    let RelayDeliveryAccount::Unix { uid, gid } = account;
    ensure!(
        !file_name.is_empty() && !file_name.contains('/'),
        "Render relay archive member name is invalid"
    );
    let mut header = tar::Header::new_ustar();
    header.set_path(file_name)?;
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_uid(u64::from(uid));
    header.set_gid(u64::from(gid));
    header.set_mtime(0);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.mode(tar::HeaderMode::Complete);
    builder.append(&header, bytes)?;
    Ok(builder.into_inner()?)
}

pub(super) enum RenderRelayDownload {
    Downloaded(Vec<u8>),
    /// Exact HTTP 404 from the selected file GET. This does not distinguish
    /// a not-yet-written file from a missing or unreachable sandbox.
    HttpNotFound,
    /// This read got no answer about the file. A download changes nothing, so
    /// the caller may read again; an upload never takes this path.
    Unavailable(String),
}

/// Statuses that say the provider could not answer now, not that a file is
/// absent or that the request is wrong.
fn download_status_is_transient(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// An exact provider file path is supplied only by the controller transfer
/// journal after it has checked the original sealed bootstrap and occurrence.
/// These fields are private so activation cannot synthesize a relay operation.
pub(super) struct RenderRelayFileContact<'a> {
    network: &'a NetworkContext,
    provider_spec: &'a ProviderSpec,
    settings: &'a Settings,
    occurrence_id: &'a str,
    remote_path: &'a str,
    /// The directory the signed upload route is bound to: the provider's file
    /// upload extracts an archive there, which is the only way it lets the
    /// delivered file keep the guest account as its owner.
    mailbox_root: &'a str,
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
        mailbox_root: &'a str,
        deadline: MonotonicDeadline,
        cancellation: &'a NetworkCancellation,
    ) -> Result<Self> {
        ensure!(
            !deadline.has_elapsed(),
            "Render relay contact deadline elapsed"
        );
        ensure!(
            remote_path
                .rsplit_once('/')
                .is_some_and(|(parent, name)| parent == mailbox_root && !name.is_empty()),
            "Render relay file is not directly inside its mailbox"
        );
        for operation in [
            ProxyOperation::UploadFile {
                remote_path: mailbox_root,
            },
            ProxyOperation::DownloadFile { remote_path },
        ] {
            let route = match operation {
                ProxyOperation::UploadFile { .. } => provider_spec.relay_file_upload_route()?,
                ProxyOperation::DownloadFile { .. } => provider_spec.relay_file_download_route()?,
                ProxyOperation::RunStream => unreachable!(),
            };
            let bound_path = match operation {
                ProxyOperation::UploadFile { remote_path }
                | ProxyOperation::DownloadFile { remote_path } => remote_path,
                ProxyOperation::RunStream => unreachable!(),
            };
            let (url, target) = api_url(
                provider_spec,
                route,
                Some(occurrence_id),
                settings,
                Some(bound_path),
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
            mailbox_root,
            credential: read_credential()?,
            deadline,
            cancellation,
            contacted: false,
        })
    }

    fn mint(&self, operation: ProxyOperation<'_>) -> Result<BoundConnectToken> {
        let (route, bound_path) = match operation {
            ProxyOperation::UploadFile { remote_path } if remote_path == self.mailbox_root => {
                (self.provider_spec.relay_file_upload_route()?, remote_path)
            }
            ProxyOperation::DownloadFile { remote_path } if remote_path == self.remote_path => {
                (self.provider_spec.relay_file_download_route()?, remote_path)
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
            Some(bound_path),
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
        )
        .classed(FailClass::RelayTransport)?;
        ensure_class!(
            FailClass::TokenStatus,
            response.status == 201 && has_json_content_type(&response.headers),
            "Render relay token mint did not return exact JSON success"
        );
        let mut bytes = Zeroizing::new(Vec::new());
        response
            .body
            .take(MAX_CONNECT_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
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
        .classed(FailClass::ProxyRoute)
    }

    /// One possible upload. A 2xx reply only reports a provider transport
    /// observation; the caller must separately read back the exact file.
    pub(super) fn upload_once(
        &mut self,
        bytes: &[u8],
        expected_sha256: &str,
        account: RelayDeliveryAccount,
    ) -> Result<()> {
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
        let file_name = self
            .remote_path
            .rsplit_once('/')
            .map(|(_, name)| name)
            .context("Render relay file has no name")?;
        let archive = delivery_archive(file_name, bytes, account)?;
        self.contacted = true;
        let operation = ProxyOperation::UploadFile {
            remote_path: self.mailbox_root,
        };
        let token = self.mint(operation).classed(FailClass::RelayContact)?;
        let request = proxy_request(
            self.settings,
            self.occurrence_id,
            token,
            operation,
            RequestBodySource::from_bytes(archive),
            "application/x-tar",
            "application/json",
            MAX_UPLOAD_RESPONSE_BYTES,
            self.deadline,
            self.cancellation,
        )
        .classed(FailClass::ProxyRoute)?;
        let response = HttpClient::new(self.network.clone())
            .execute(request)
            .map_err(anyhow::Error::from)
            .classed(FailClass::RelayTransport)?;
        ensure_class!(
            FailClass::RelayStatus,
            (200..300).contains(&response.status),
            "Render relay file upload did not return transport success"
        );
        Ok(())
    }

    /// This GET may be repeated only after the daemon retained another read
    /// attempt on the *same* transfer. A missing, partial or oversized file is
    /// not a negative provider proof and never grants a replacement upload.
    pub(super) fn download_bounded(
        &mut self,
        maximum_file_bytes: u64,
    ) -> Result<RenderRelayDownload> {
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
        let token = match self.mint(operation) {
            Ok(token) => token,
            Err(error) if FailClass::of(&error) == FailClass::RelayTransport => {
                return Ok(RenderRelayDownload::Unavailable(
                    "mint_transport".to_owned(),
                ));
            }
            Err(error) => return Err(error).classed(FailClass::RelayContact),
        };
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
        )
        .classed(FailClass::ProxyRoute)?;
        let Ok(response) = HttpClient::new(self.network.clone()).execute(request) else {
            return Ok(RenderRelayDownload::Unavailable(
                "read_transport".to_owned(),
            ));
        };
        if download_status_is_transient(response.status) {
            return Ok(RenderRelayDownload::Unavailable(format!(
                "read_status:{}",
                response.status
            )));
        }
        // Render's SDK names a missing-path error but does not document the
        // raw file-route status. Keep this exact 404 as a bounded pending
        // observation until an installed missing-file probe establishes it.
        if response.status == 404 {
            return Ok(RenderRelayDownload::HttpNotFound);
        }
        ensure_class!(
            FailClass::RelayStatus,
            response.status == 200,
            "Render relay file read is absent or ambiguous"
        );
        let bytes = (|| -> Result<Vec<u8>> {
            let mut bytes = Vec::new();
            response
                .body
                .take(maximum_file_bytes + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                !bytes.is_empty() && bytes.len() as u64 <= maximum_file_bytes,
                "Render relay file read is empty or exceeds bound"
            );
            // The guest publishes its mailbox files with lillux canonical
            // JSON; accept exactly that form.
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(
                lillux::canonical_json(&value)?.as_bytes() == bytes.as_slice(),
                "Render relay file is not complete canonical JSON"
            );
            Ok(bytes)
        })()
        .classed(FailClass::RelayBody)?;
        Ok(RenderRelayDownload::Downloaded(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_statuses_that_say_nothing_about_the_file_are_read_again() {
        for status in [429, 500, 502, 503, 504] {
            assert!(download_status_is_transient(status), "{status}");
        }
        for status in [200, 400, 401, 403, 404, 410] {
            assert!(!download_status_is_transient(status), "{status}");
        }
    }

    #[test]
    fn archive_is_one_private_member_owned_by_the_guest_account() {
        let bytes = br#"{"a":1}"#;
        let archive = delivery_archive(
            "attach.response.json",
            bytes,
            RelayDeliveryAccount::Unix {
                uid: 65534,
                gid: 65533,
            },
        )
        .unwrap();
        let mut reader = tar::Archive::new(archive.as_slice());
        let mut entries = reader.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        let header = entry.header();
        assert!(header.as_ustar().is_some());
        assert_eq!(header.uid().unwrap(), 65534);
        assert_eq!(header.gid().unwrap(), 65533);
        assert_eq!(header.mode().unwrap(), 0o600);
        assert_eq!(header.mtime().unwrap(), 0);
        assert_eq!(header.entry_type(), tar::EntryType::Regular);
        assert!(header.username().unwrap().unwrap_or_default().is_empty());
        assert!(header.groupname().unwrap().unwrap_or_default().is_empty());
        assert_eq!(
            &*entry.path().unwrap(),
            std::path::Path::new("attach.response.json")
        );
        let mut body = Vec::new();
        entry.read_to_end(&mut body).unwrap();
        assert_eq!(body, bytes);
        assert!(entries.next().is_none());
    }

    #[test]
    fn archive_member_must_be_a_bare_name() {
        let account = RelayDeliveryAccount::Unix { uid: 1, gid: 1 };
        assert!(delivery_archive("a/b.json", b"x", account).is_err());
        assert!(delivery_archive("", b"x", account).is_err());
    }

    #[test]
    fn upload_token_route_binds_the_mailbox_directory() {
        let root = "/ryeos/occurrences/sbx-abc/0123";
        let url = connect_token_url(
            "sbx-abc",
            "own-1",
            ProxyOperation::UploadFile { remote_path: root },
        )
        .unwrap();
        assert_eq!(
            url.query_pairs().find(|(k, _)| k == "path").unwrap().1,
            root
        );
        assert_ne!(
            url,
            connect_token_url(
                "sbx-abc",
                "own-1",
                ProxyOperation::UploadFile {
                    remote_path: "/ryeos/occurrences/sbx-abc/0123/attach.response.json"
                },
            )
            .unwrap()
        );
    }
}
