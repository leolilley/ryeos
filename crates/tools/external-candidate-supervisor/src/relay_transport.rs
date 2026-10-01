//! Detached, descriptor-rooted guest half of the controller file relay.
//!
//! The shipped entrypoints still reject relay bootstraps. An outer launch
//! owner must first install and retain this original private mailbox, bind it
//! to a durable supervisor journal, and prove it is absent from candidate
//! mounts. Files and provider transfer results alone are not channel evidence.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_external_execution::transport::{
    ExternalExecutionChannelTransport, ExternalTransportExchange, ExternalTransportStepFailure,
};
use ryeos_state::external_execution::transport::{
    ExternalChannelAttachRequest, ExternalChannelAttachResponse, ExternalChannelExchangeRequest,
    ExternalChannelExchangeResponse, ExternalControllerFileRelayContract,
    ExternalRelayResponseEnvelope, ExternalRelayResponseKind, ExternalSupervisorBootstrap,
};
use ryeos_state::external_execution::{
    ChannelDirection, ExecutionChannelBinding, SignedExecutionFrame, encode_channel_public_key,
};

const ATTACH_REQUEST: &str = "attach.request.json";
const ATTACH_RESPONSE: &str = "attach.response.json";

struct MailboxFiles {
    root: lillux::PinnedDirectory,
    maximum_file_bytes: u64,
    poll_interval: Duration,
    poll_deadline: Duration,
    accepted_responses: BTreeMap<String, String>,
}

impl MailboxFiles {
    fn new(
        root: lillux::PinnedDirectory,
        relay: &ExternalControllerFileRelayContract,
    ) -> Result<Self> {
        relay.validate()?;
        root.require_owner_private_directory()?;
        root.ensure_path_binding()?;
        Ok(Self {
            root,
            maximum_file_bytes: relay.maximum_file_bytes,
            poll_interval: Duration::from_millis(u64::from(relay.poll_interval_ms)),
            poll_deadline: Duration::from_millis(u64::from(relay.poll_deadline_ms)),
            accepted_responses: BTreeMap::new(),
        })
    }

    /// Publish only once. A lost local return can accept the same exact file;
    /// a different incumbent, symlink, or partially written request is fatal.
    fn publish_exact(&self, name: &str, bytes: &[u8]) -> Result<()> {
        ensure!(
            !bytes.is_empty() && u64::try_from(bytes.len())? <= self.maximum_file_bytes,
            "external relay request exceeds its signed file bound"
        );
        self.root.ensure_path_binding()?;
        let name = OsStr::new(name);
        if self.root.open_pinned_regular(name, false)?.is_none() {
            match self.root.atomic_write_if_same(name, None, bytes, 0o600) {
                Ok(()) => {}
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|source| {
                            source.kind() == std::io::ErrorKind::AlreadyExists
                        }) => {}
                Err(error) => return Err(error).context("publish exact external relay request"),
            }
        }
        self.require_existing_exact(name, bytes)
    }

    /// Recovery may verify the original request slot, but must never recreate
    /// it after a journaled attachment has already made controller contact.
    fn require_existing_exact(&self, name: &OsStr, bytes: &[u8]) -> Result<()> {
        ensure!(
            !bytes.is_empty() && u64::try_from(bytes.len())? <= self.maximum_file_bytes,
            "external relay request exceeds its signed file bound"
        );
        self.root.ensure_path_binding()?;
        let incumbent = self
            .root
            .open_pinned_regular(name, false)?
            .context("retained external relay request slot is absent")?;
        let observation = incumbent.observation()?;
        ensure!(
            incumbent.read_stable_bounded(&observation, self.maximum_file_bytes)? == bytes,
            "external relay request slot contains different bytes"
        );
        self.root
            .ensure_regular_entry_matches(name, Some(&incumbent.try_clone_descriptor()?))?;
        self.root.ensure_path_binding()?;
        Ok(())
    }

    /// A visible file can still be an in-progress provider upload. Only a
    /// stable, complete JSON envelope advances to signature verification.
    fn read_complete_envelope_candidate(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.root.ensure_path_binding()?;
        let Some(file) = self.root.open_pinned_regular(OsStr::new(name), false)? else {
            return Ok(None);
        };
        let observation = file.observation()?;
        let bytes = match file.read_stable_bounded(&observation, self.maximum_file_bytes) {
            Ok(bytes) => bytes,
            Err(error) => {
                if file.observation()?.size() > self.maximum_file_bytes {
                    return Err(error).context("external relay response exceeds file bound");
                }
                return Ok(None);
            }
        };
        if serde_json::from_slice::<ExternalRelayResponseEnvelope>(&bytes).is_err() {
            return Ok(None);
        }
        self.root
            .ensure_regular_entry_matches(OsStr::new(name), Some(&file.try_clone_descriptor()?))?;
        self.root.ensure_path_binding()?;
        Ok(Some(bytes))
    }

    fn wait_verified<T: serde::de::DeserializeOwned>(
        &mut self,
        name: &str,
        bootstrap: &ExternalSupervisorBootstrap,
        kind: ExternalRelayResponseKind,
        request_digest: &str,
        binding_digest: Option<&str>,
        caller_deadline: MonotonicDeadline,
    ) -> std::result::Result<T, ExternalTransportStepFailure> {
        let deadline = caller_deadline.min(MonotonicDeadline::after(self.poll_deadline));
        loop {
            if let Some(bytes) = self
                .read_complete_envelope_candidate(name)
                .map_err(ExternalTransportStepFailure::Fatal)?
            {
                let response = ExternalRelayResponseEnvelope::decode_verified(
                    &bytes,
                    bootstrap,
                    kind,
                    request_digest,
                    binding_digest,
                )
                .map_err(ExternalTransportStepFailure::Fatal)?;
                let digest = lillux::sha256_hex(&bytes);
                if let Some(prior) = self.accepted_responses.get(name) {
                    if prior != &digest {
                        return Err(ExternalTransportStepFailure::Fatal(anyhow::anyhow!(
                            "external relay response slot changed after acceptance"
                        )));
                    }
                } else {
                    self.accepted_responses.insert(name.to_owned(), digest);
                }
                self.root
                    .ensure_path_binding()
                    .map_err(ExternalTransportStepFailure::Fatal)?;
                return Ok(response);
            }
            if deadline.has_elapsed() {
                return Err(ExternalTransportStepFailure::AmbiguousTransport(
                    anyhow::anyhow!(
                        "external relay response absent or incomplete at polling deadline"
                    ),
                ));
            }
            lillux::time::sleep(self.poll_interval.min(deadline.remaining()));
        }
    }
}

/// The original pinned, owner-private mailbox child. The caller supplies the
/// exact descriptor; this type never creates a replacement or opens an
/// ambient alternate root. The caller also proves disjointness from every
/// admitted directory mount before adopting this handle.
pub struct OriginalRelayMailbox {
    bootstrap: ExternalSupervisorBootstrap,
    files: MailboxFiles,
}

/// Only an authenticated response read from the same pinned mailbox can
/// produce this token. A caller cannot pass a fabricated raw binding into the
/// guest exchange transport.
pub struct AuthenticatedRelayAttachment {
    binding: ExecutionChannelBinding,
    bootstrap_digest: String,
    mailbox_identity: lillux::PinnedDirectoryIdentity,
    supervisor_public_key: String,
    response_digest: String,
}

impl AuthenticatedRelayAttachment {
    pub fn binding(&self) -> &ExecutionChannelBinding {
        &self.binding
    }

    pub fn response_digest(&self) -> &str {
        &self.response_digest
    }
}

impl OriginalRelayMailbox {
    pub fn adopt(
        bootstrap: ExternalSupervisorBootstrap,
        original_mailbox: lillux::PinnedDirectory,
        sealed_mailbox_identity: lillux::PinnedDirectoryIdentity,
        state_root: &lillux::PinnedDirectory,
        candidate_runtime: &lillux::PinnedDirectory,
        candidate_private_parent: &lillux::PinnedDirectory,
        staged_source: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        bootstrap.validate()?;
        let relay = bootstrap.controller.relay()?;
        ensure!(
            original_mailbox.path() == Path::new(&bootstrap.relay_mailbox_root()?)
                && original_mailbox.identity()? == sealed_mailbox_identity,
            "external relay mailbox descriptor differs from sealed occurrence path"
        );
        for protected in [
            state_root,
            candidate_runtime,
            candidate_private_parent,
            staged_source,
        ] {
            original_mailbox.require_disjoint_directory_tree(protected)?;
        }
        let files = MailboxFiles::new(original_mailbox, relay)?;
        Ok(Self { bootstrap, files })
    }

    /// Retry an exact attachment request, preserving the supervisor key and
    /// bytes. The response must carry an owner signature before binding
    /// validation; neither file presence nor a transfer return is authority.
    pub fn attach_exact(
        &mut self,
        supervisor_signing_key: &lillux::crypto::SigningKey,
        request: &ExternalChannelAttachRequest,
        deadline: MonotonicDeadline,
    ) -> Result<AuthenticatedRelayAttachment> {
        ensure!(
            !deadline.has_elapsed(),
            "external relay attachment deadline elapsed before file publication"
        );
        request.validate_for_bootstrap(&self.bootstrap)?;
        ensure!(
            request.supervisor_public_key
                == encode_channel_public_key(&supervisor_signing_key.verifying_key())?,
            "external relay attachment changed its original supervisor key"
        );
        let request_bytes = request.canonical_bytes()?;
        if self
            .files
            .root
            .open_pinned_regular(OsStr::new(ATTACH_REQUEST), false)?
            .is_none()
        {
            self.bootstrap
                .validate_at(lillux::time::timestamp_millis())?;
        }
        self.files.publish_exact(ATTACH_REQUEST, &request_bytes)?;
        let response: ExternalChannelAttachResponse = self.files.wait_verified(
            ATTACH_RESPONSE,
            &self.bootstrap,
            ExternalRelayResponseKind::Attach,
            &lillux::sha256_hex(&request_bytes),
            None,
            deadline,
        )?;
        response.validate_for_bootstrap(&self.bootstrap, &request.supervisor_public_key)?;
        Ok(AuthenticatedRelayAttachment {
            binding: response.binding,
            bootstrap_digest: self.bootstrap.digest()?,
            mailbox_identity: self.files.root.identity()?,
            supervisor_public_key: request.supervisor_public_key.clone(),
            response_digest: self
                .files
                .accepted_responses
                .get(ATTACH_RESPONSE)
                .context("authenticated relay attachment has no retained response digest")?
                .clone(),
        })
    }

    pub fn into_attached(
        self,
        attachment: AuthenticatedRelayAttachment,
    ) -> Result<AttachedFileRelayChannel> {
        ensure!(
            attachment.bootstrap_digest == self.bootstrap.digest()?
                && attachment.mailbox_identity == self.files.root.identity()?,
            "external relay attachment moved to a different sealed mailbox"
        );
        self.bootstrap
            .validate_attached_binding(&attachment.binding, &attachment.supervisor_public_key)?;
        Ok(AttachedFileRelayChannel {
            mailbox: self,
            binding: attachment.binding,
        })
    }

    /// Recover an already journaled binding only after rereading the exact
    /// authenticated attach response from the original mailbox. No new
    /// attachment request or replacement supervisor key is created.
    pub fn reconnect_retained(
        mut self,
        request: &ExternalChannelAttachRequest,
        binding: &ExecutionChannelBinding,
        expected_response_digest: &str,
        deadline: MonotonicDeadline,
    ) -> Result<AttachedFileRelayChannel> {
        request.validate_for_bootstrap(&self.bootstrap)?;
        let request_bytes = request.canonical_bytes()?;
        self.files
            .require_existing_exact(OsStr::new(ATTACH_REQUEST), &request_bytes)?;
        let response: ExternalChannelAttachResponse = self.files.wait_verified(
            ATTACH_RESPONSE,
            &self.bootstrap,
            ExternalRelayResponseKind::Attach,
            &lillux::sha256_hex(&request_bytes),
            None,
            deadline,
        )?;
        response.validate_for_bootstrap(&self.bootstrap, &request.supervisor_public_key)?;
        ensure!(
            response.binding == *binding,
            "external relay attachment response differs from retained binding"
        );
        ensure!(
            self.files
                .accepted_responses
                .get(ATTACH_RESPONSE)
                .map(String::as_str)
                == Some(expected_response_digest),
            "external relay attachment response differs from retained exact bytes"
        );
        let attachment = AuthenticatedRelayAttachment {
            binding: response.binding,
            bootstrap_digest: self.bootstrap.digest()?,
            mailbox_identity: self.files.root.identity()?,
            supervisor_public_key: request.supervisor_public_key.clone(),
            response_digest: expected_response_digest.to_owned(),
        };
        self.into_attached(attachment)
    }
}

pub struct AttachedFileRelayChannel {
    mailbox: OriginalRelayMailbox,
    binding: ExecutionChannelBinding,
}

impl ExternalExecutionChannelTransport for AttachedFileRelayChannel {
    fn fresh_idle_poll_interval(&self) -> Option<Duration> {
        Some(self.mailbox.files.poll_interval)
    }

    fn exchange(&mut self, canonical_supervisor_frame: &[u8]) -> Result<ExternalTransportExchange> {
        self.exchange_until(
            canonical_supervisor_frame,
            MonotonicDeadline::after(self.mailbox.files.poll_deadline),
        )
        .map_err(anyhow::Error::new)
    }

    fn exchange_until(
        &mut self,
        canonical_supervisor_frame: &[u8],
        deadline: MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportExchange, ExternalTransportStepFailure> {
        if deadline.has_elapsed() {
            return Err(ExternalTransportStepFailure::AmbiguousTransport(
                anyhow::anyhow!("external relay exchange deadline elapsed before file publication"),
            ));
        }
        let verified = SignedExecutionFrame::decode_and_verify(
            canonical_supervisor_frame,
            &self.binding,
            lillux::time::timestamp_millis(),
        )
        .map_err(ExternalTransportStepFailure::Fatal)?;
        if verified.frame().direction != ChannelDirection::SupervisorToOwner {
            return Err(ExternalTransportStepFailure::Fatal(anyhow::anyhow!(
                "external relay exchange received a non-supervisor frame"
            )));
        }
        let sequence = verified.frame().sequence;
        let relay = self
            .mailbox
            .bootstrap
            .controller
            .relay()
            .map_err(ExternalTransportStepFailure::Fatal)?;
        if sequence == 0 || sequence > u64::from(relay.maximum_frames) {
            return Err(ExternalTransportStepFailure::Fatal(anyhow::anyhow!(
                "external relay sequence exceeds signed file budget"
            )));
        }
        let request = ExternalChannelExchangeRequest::from_frame(
            self.mailbox.bootstrap.placement_thread_id.clone(),
            self.mailbox.bootstrap.occurrence_id.clone(),
            canonical_supervisor_frame,
        )
        .map_err(ExternalTransportStepFailure::Fatal)?;
        let request_bytes = lillux::canonical_json(
            &serde_json::to_value(&request)
                .map_err(anyhow::Error::new)
                .map_err(ExternalTransportStepFailure::Fatal)?,
        )
        .map_err(ExternalTransportStepFailure::Fatal)?
        .into_bytes();
        let request_name = format!("exchange-{sequence:05}.request.json");
        let response_name = format!("exchange-{sequence:05}.response.json");
        self.mailbox
            .files
            .publish_exact(&request_name, &request_bytes)
            .map_err(ExternalTransportStepFailure::Fatal)?;
        let binding_digest = self
            .binding
            .digest()
            .map_err(ExternalTransportStepFailure::Fatal)?;
        let response: ExternalChannelExchangeResponse = self.mailbox.files.wait_verified(
            &response_name,
            &self.mailbox.bootstrap,
            ExternalRelayResponseKind::Exchange,
            &lillux::sha256_hex(&request_bytes),
            Some(&binding_digest),
            deadline,
        )?;
        response
            .validate_shape()
            .map_err(ExternalTransportStepFailure::Fatal)?;
        if response.incoming_sequence != sequence
            || response.incoming_frame_digest != verified.digest()
        {
            return Err(ExternalTransportStepFailure::Fatal(anyhow::anyhow!(
                "external relay response changed its signed supervisor frame"
            )));
        }
        Ok(ExternalTransportExchange {
            schema: response.schema,
            incoming_new: response.incoming_new,
            incoming_sequence: response.incoming_sequence,
            incoming_frame_digest: response.incoming_frame_digest,
            acknowledgement_frame_digest: response.acknowledgement_frame_digest,
            outbound_frames: response
                .outbound_frames
                .into_iter()
                .map(super::decode_frame)
                .collect::<Result<Vec<_>>>()
                .map_err(ExternalTransportStepFailure::Fatal)?,
            urgent_revocation_frame: response
                .urgent_revocation_frame
                .map(super::decode_frame)
                .transpose()
                .map_err(ExternalTransportStepFailure::Fatal)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(root: &tempfile::TempDir) -> MailboxFiles {
        let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        pinned.tighten_owner_private_directory().unwrap();
        MailboxFiles {
            root: pinned,
            maximum_file_bytes: 1024,
            poll_interval: Duration::from_millis(1),
            poll_deadline: Duration::from_millis(20),
            accepted_responses: BTreeMap::new(),
        }
    }

    #[test]
    fn request_slot_accepts_exact_retry_and_rejects_replacement_or_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let mailbox = files(&root);
        mailbox
            .publish_exact("attach.request.json", b"{\"a\":1}")
            .unwrap();
        mailbox
            .publish_exact("attach.request.json", b"{\"a\":1}")
            .unwrap();
        assert!(
            mailbox
                .publish_exact("attach.request.json", b"{\"a\":2}")
                .is_err()
        );
        mailbox
            .root
            .atomic_write_if_same(
                OsStr::new("exchange-00001.request.json"),
                None,
                b"{\"partial\"",
                0o600,
            )
            .unwrap();
        assert!(
            mailbox
                .publish_exact("exchange-00001.request.json", b"{\"complete\":true}")
                .is_err()
        );
        mailbox
            .root
            .atomic_write_if_same(
                OsStr::new("attach.response.json"),
                None,
                b"{\"payload\":",
                0o600,
            )
            .unwrap();
        assert!(
            mailbox
                .read_complete_envelope_candidate("attach.response.json")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn retained_request_verification_never_publishes_an_absent_slot() {
        let root = tempfile::tempdir().unwrap();
        let mailbox = files(&root);
        assert!(
            mailbox
                .require_existing_exact(OsStr::new(ATTACH_REQUEST), b"{\"a\":1}")
                .is_err()
        );
        assert!(
            mailbox
                .root
                .open_pinned_regular(OsStr::new(ATTACH_REQUEST), false)
                .unwrap()
                .is_none()
        );
    }
}
