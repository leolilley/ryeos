//! Protected outer lifecycle journal for one external candidate supervisor.
//!
//! This store exists outside the candidate and its guest journal. It persists
//! the exact secret bootstrap and supervisor signing key before attachment,
//! the returned channel binding before any candidate preparation, and the
//! independently anchored guest-store/launcher intent before native spawn.
//! Reopen may continue only the exact pre-launch transition already authorized
//! by a retained `prepared` or `attached` record. Once launch intent exists,
//! reopen is recovery-only and cannot mint another attachment or launch.

use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux_sqlite::{
    OriginalNativeBoundMode, OriginalNativeConnection, OriginalNativeConnectionTerminal,
    OriginalNativeDestinationAuthority, OriginalNativeDomain, OriginalNativeFactory,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use zeroize::Zeroizing;

use super::guest_journal::GuestJournalStoreIdentity;
use super::native_journal::{self, NativeJournalConnection};
use super::transport::{
    ExternalCapturedNetworkInputs, ExternalChannelAttachRequest,
    ExternalControllerTransportSelection, ExternalSupervisorBootstrap,
    MAX_EXTERNAL_NETWORK_CAPTURE_JSON_BYTES,
};
use super::{ExecutionChannelBinding, encode_channel_public_key};
use crate::operational::RetainTerminal;
use crate::sqlite_schema::SchemaSqlDefinition;

const DATABASE_NAME: &str = "external-supervisor.sqlite3";
const APPLICATION_ID: i32 = 0x5259_4553; // RYES
// HTTP epoch 6 retains the exact policy-bound network capture. Relay epoch 7
// binds the original mailbox inode and exact authenticated attach response and
// is shared by Q bootstrap schema 8 and ordinary-session schema 9. Relay store
// identity schema 4 and mailbox capture schema 1 stay unchanged; bootstrap
// validation, not a journal migration, enforces the workload pair. Predecessor
// stores remain historical evidence; recovery never recaptures.
const HTTP_SCHEMA_EPOCH: i64 = 6;
const RELAY_SCHEMA_EPOCH: i64 = 7;

const SCHEMA: &str = r#"
CREATE TABLE external_supervisor_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    schema_epoch INTEGER NOT NULL CHECK(schema_epoch=6),
    journal_nonce TEXT NOT NULL UNIQUE,
    directory_identity_json TEXT NOT NULL,
    database_identity_json TEXT NOT NULL,
    bootstrap_digest TEXT NOT NULL UNIQUE,
    bootstrap_json TEXT NOT NULL,
    capture_digest TEXT NOT NULL,
    capture_json TEXT NOT NULL,
    supervisor_signing_seed_base64 TEXT NOT NULL,
    supervisor_public_key TEXT NOT NULL,
    attachment_request_digest TEXT NOT NULL UNIQUE,
    attachment_request_json TEXT NOT NULL,
    lifecycle TEXT NOT NULL CHECK(lifecycle IN ('prepared','attached','launch_intent'))
);
CREATE TABLE external_supervisor_binding (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    binding_digest TEXT NOT NULL UNIQUE,
    binding_json TEXT NOT NULL
);
CREATE TABLE external_supervisor_launch_intent (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    binding_digest TEXT NOT NULL,
    guest_store_identity_digest TEXT NOT NULL UNIQUE,
    guest_store_identity_json TEXT NOT NULL,
    launcher_spec_digest TEXT NOT NULL,
    launcher_bootstrap_digest TEXT NOT NULL,
    launcher_artifact_digest TEXT NOT NULL,
    FOREIGN KEY(binding_digest) REFERENCES external_supervisor_binding(binding_digest)
);
CREATE TRIGGER external_supervisor_meta_singleton
BEFORE INSERT ON external_supervisor_meta
WHEN NEW.singleton!=1 OR EXISTS(SELECT 1 FROM external_supervisor_meta)
BEGIN SELECT RAISE(ABORT, 'external supervisor metadata is a singleton'); END;
CREATE TRIGGER external_supervisor_meta_no_delete
BEFORE DELETE ON external_supervisor_meta
BEGIN SELECT RAISE(ABORT, 'external supervisor metadata cannot be deleted'); END;
CREATE TRIGGER external_supervisor_meta_transition
BEFORE UPDATE ON external_supervisor_meta
WHEN NEW.singleton!=OLD.singleton OR NEW.schema_epoch!=OLD.schema_epoch
 OR NEW.journal_nonce!=OLD.journal_nonce
 OR NEW.directory_identity_json!=OLD.directory_identity_json
 OR NEW.database_identity_json!=OLD.database_identity_json
 OR NEW.bootstrap_digest!=OLD.bootstrap_digest OR NEW.bootstrap_json!=OLD.bootstrap_json
 OR NEW.capture_digest!=OLD.capture_digest OR NEW.capture_json!=OLD.capture_json
 OR NEW.supervisor_signing_seed_base64!=OLD.supervisor_signing_seed_base64
 OR NEW.supervisor_public_key!=OLD.supervisor_public_key
 OR NEW.attachment_request_digest!=OLD.attachment_request_digest
 OR NEW.attachment_request_json!=OLD.attachment_request_json
 OR NOT ((OLD.lifecycle='prepared' AND NEW.lifecycle='attached')
      OR (OLD.lifecycle='attached' AND NEW.lifecycle='launch_intent'))
BEGIN SELECT RAISE(ABORT, 'external supervisor metadata cannot be rewritten'); END;
CREATE TRIGGER external_supervisor_binding_singleton
BEFORE INSERT ON external_supervisor_binding
WHEN NEW.singleton!=1 OR EXISTS(SELECT 1 FROM external_supervisor_binding)
BEGIN SELECT RAISE(ABORT, 'external supervisor binding is immutable'); END;
CREATE TRIGGER external_supervisor_binding_no_update
BEFORE UPDATE ON external_supervisor_binding
BEGIN SELECT RAISE(ABORT, 'external supervisor binding is immutable'); END;
CREATE TRIGGER external_supervisor_binding_no_delete
BEFORE DELETE ON external_supervisor_binding
BEGIN SELECT RAISE(ABORT, 'external supervisor binding cannot be deleted'); END;
CREATE TRIGGER external_supervisor_launch_singleton
BEFORE INSERT ON external_supervisor_launch_intent
WHEN NEW.singleton!=1 OR EXISTS(SELECT 1 FROM external_supervisor_launch_intent)
BEGIN SELECT RAISE(ABORT, 'external supervisor launch intent is immutable'); END;
CREATE TRIGGER external_supervisor_launch_no_update
BEFORE UPDATE ON external_supervisor_launch_intent
BEGIN SELECT RAISE(ABORT, 'external supervisor launch intent is immutable'); END;
CREATE TRIGGER external_supervisor_launch_no_delete
BEFORE DELETE ON external_supervisor_launch_intent
BEGIN SELECT RAISE(ABORT, 'external supervisor launch intent cannot be deleted'); END;
"#;

struct SupervisorStore {
    conn: NativeJournalConnection,
    /// Complete schema SQL of this store's epoch, captured from a closed
    /// native reference whose terminal is already in the host's census.
    schema_reference: SchemaSqlDefinition,
    directory: Arc<lillux::PinnedDirectory>,
    _lifetime_lock: lillux::PinnedDirectoryLock,
    bootstrap: ExternalSupervisorBootstrap,
    transport_capture: ExternalSupervisorTransportCapture,
    supervisor_signing_key: lillux::crypto::SigningKey,
    attachment_request: ExternalChannelAttachRequest,
    binding: Option<ExecutionChannelBinding>,
    launch_intent: Option<ExternalSupervisorLaunchIntent>,
    store_identity: ExternalSupervisorJournalStoreIdentity,
}

/// The relay stores the original descriptor identity and signed pathname,
/// never a sampled network file or a recreated mailbox on recovery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRelayMailboxCapture {
    schema: u32,
    mailbox_root: String,
    mailbox_identity: lillux::PinnedDirectoryIdentity,
}

impl ExternalRelayMailboxCapture {
    pub fn new(
        bootstrap: &ExternalSupervisorBootstrap,
        mailbox: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        let capture = Self {
            schema: 1,
            mailbox_root: bootstrap.relay_mailbox_root()?,
            mailbox_identity: mailbox.identity()?,
        };
        capture.validate_for(bootstrap)?;
        ensure!(
            mailbox.path() == std::path::Path::new(&capture.mailbox_root),
            "original relay mailbox path differs from signed occurrence"
        );
        mailbox.require_owner_private_directory()?;
        mailbox.ensure_path_binding()?;
        Ok(capture)
    }

    pub fn validate_for(&self, bootstrap: &ExternalSupervisorBootstrap) -> Result<()> {
        bootstrap.controller.relay()?;
        ensure!(
            self.schema == 1 && self.mailbox_root == bootstrap.relay_mailbox_root()?,
            "external relay mailbox capture differs from sealed bootstrap"
        );
        Ok(())
    }

    pub fn mailbox_identity(&self) -> lillux::PinnedDirectoryIdentity {
        self.mailbox_identity
    }

    pub fn mailbox_root(&self) -> &str {
        &self.mailbox_root
    }

    pub fn digest(&self) -> Result<String> {
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::json!({
                "domain":"ryeos.external-relay-mailbox-capture.v1",
                "capture":self,
            }))?
            .as_bytes(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalSupervisorTransportCapture {
    Http(ExternalCapturedNetworkInputs),
    Relay(ExternalRelayMailboxCapture),
}

impl ExternalSupervisorTransportCapture {
    fn digest(&self) -> Result<String> {
        match self {
            Self::Http(capture) => capture.digest(),
            Self::Relay(capture) => capture.digest(),
        }
    }
}

/// Independent outer anchor supplied again by the lifecycle owner on reopen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSupervisorJournalStoreIdentity {
    schema: u32,
    journal_nonce: String,
    directory_identity: lillux::PinnedDirectoryIdentity,
    database_identity: lillux::PinnedRegularFileIdentity,
    bootstrap_digest: String,
    capture_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSupervisorLaunchIntent {
    pub binding_digest: String,
    pub guest_store_identity_digest: String,
    pub guest_store_identity: GuestJournalStoreIdentity,
    pub launcher_spec_digest: String,
    pub launcher_bootstrap_digest: String,
    pub launcher_artifact_digest: String,
}

pub struct PreparedExternalSupervisorJournal(SupervisorStore);
pub struct AttachedExternalSupervisorJournal(SupervisorStore);
pub struct LaunchIntentExternalSupervisorJournal(SupervisorStore);
pub struct RecoveredExternalSupervisorJournal(SupervisorStore);

/// Exact stage recovered under the retained outer inode anchor. Prepared and
/// attached stages may continue only their already-authorized transition.
/// Once launch intent exists, recovery has no path back to live execution.
pub enum ExternalSupervisorJournalRecovery {
    Prepared(PreparedExternalSupervisorJournal),
    Attached(AttachedExternalSupervisorJournal),
    LaunchIntent(RecoveredExternalSupervisorJournal),
}

impl ExternalSupervisorJournalStoreIdentity {
    pub fn digest(&self) -> Result<String> {
        ensure!(
            self.schema == 3 || self.schema == 4,
            "unsupported supervisor journal identity schema"
        );
        hash(&self.journal_nonce, "supervisor journal nonce")?;
        hash(&self.bootstrap_digest, "supervisor bootstrap digest")?;
        hash(&self.capture_digest, "supervisor network capture digest")?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }
}

impl PreparedExternalSupervisorJournal {
    pub fn transport_capture(&self) -> &ExternalSupervisorTransportCapture {
        &self.0.transport_capture
    }

    pub fn captured_network_inputs(&self) -> Result<&ExternalCapturedNetworkInputs> {
        match &self.0.transport_capture {
            ExternalSupervisorTransportCapture::Http(value) => Ok(value),
            ExternalSupervisorTransportCapture::Relay(_) => {
                anyhow::bail!("relay journal has no network capture")
            }
        }
    }

    /// Create the journal on a fresh directory. Its schema reference closes
    /// into `retain` before the journal's own connection opens.
    pub fn create(
        directory: lillux::PinnedDirectory,
        bootstrap: ExternalSupervisorBootstrap,
        captured_network_inputs: ExternalCapturedNetworkInputs,
        supervisor_signing_key: lillux::crypto::SigningKey,
        factory: &Arc<OriginalNativeFactory>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
        retain: RetainTerminal<'_>,
    ) -> Result<Self> {
        bootstrap.validate()?;
        captured_network_inputs.validate_for(&bootstrap.controller.http()?.network_inputs)?;
        Self::create_with_capture(
            directory,
            bootstrap,
            ExternalSupervisorTransportCapture::Http(captured_network_inputs),
            supervisor_signing_key,
            factory,
            destination,
            retain,
        )
    }

    /// Relay form of [`Self::create`].
    pub fn create_relay(
        directory: lillux::PinnedDirectory,
        bootstrap: ExternalSupervisorBootstrap,
        mailbox: &lillux::PinnedDirectory,
        supervisor_signing_key: lillux::crypto::SigningKey,
        factory: &Arc<OriginalNativeFactory>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
        retain: RetainTerminal<'_>,
    ) -> Result<Self> {
        let capture = ExternalRelayMailboxCapture::new(&bootstrap, mailbox)?;
        Self::create_with_capture(
            directory,
            bootstrap,
            ExternalSupervisorTransportCapture::Relay(capture),
            supervisor_signing_key,
            factory,
            destination,
            retain,
        )
    }

    fn create_with_capture(
        directory: lillux::PinnedDirectory,
        bootstrap: ExternalSupervisorBootstrap,
        transport_capture: ExternalSupervisorTransportCapture,
        supervisor_signing_key: lillux::crypto::SigningKey,
        factory: &Arc<OriginalNativeFactory>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
        retain: RetainTerminal<'_>,
    ) -> Result<Self> {
        let (capture_digest, capture_json, schema_epoch, identity_schema) = match &transport_capture
        {
            ExternalSupervisorTransportCapture::Http(capture) => {
                capture.validate_for(&bootstrap.controller.http()?.network_inputs)?;
                (
                    capture.digest()?,
                    lillux::canonical_json(&serde_json::to_value(capture)?)?,
                    HTTP_SCHEMA_EPOCH,
                    3,
                )
            }
            ExternalSupervisorTransportCapture::Relay(capture) => {
                capture.validate_for(&bootstrap)?;
                (
                    capture.digest()?,
                    lillux::canonical_json(&serde_json::to_value(capture)?)?,
                    RELAY_SCHEMA_EPOCH,
                    4,
                )
            }
        };
        ensure!(
            capture_json.len() <= MAX_EXTERNAL_NETWORK_CAPTURE_JSON_BYTES,
            "external supervisor network capture JSON exceeds bounds"
        );
        directory.require_owner_private_directory()?;
        let lifetime_lock = directory
            .try_lock_exclusive()?
            .context("external supervisor directory already has a live owner")?;
        lifetime_lock.ensure_protects(&directory)?;
        ensure!(
            directory.entry_names()?.is_empty(),
            "external supervisor store directory is not fresh"
        );
        let attachment_request = bootstrap.attachment_request(&supervisor_signing_key)?;
        let bootstrap_digest = bootstrap.digest()?;
        let bootstrap_json = String::from_utf8(bootstrap.canonical_bytes()?)?;
        let attachment_request_digest = attachment_request.digest()?;
        let attachment_request_json = String::from_utf8(attachment_request.canonical_bytes()?)?;
        let supervisor_public_key =
            encode_channel_public_key(&supervisor_signing_key.verifying_key())?;
        let signing_seed = Zeroizing::new(STANDARD.encode(supervisor_signing_key.to_bytes()));
        // SAFETY: this journal holds the directory's exclusive lifetime lock
        // for the whole capture; the caller's factory and destination come
        // from the process's one funded native host.
        let schema_reference = unsafe {
            native_journal::capture_schema_reference(
                OriginalNativeDomain::ExternalSupervisorJournalSchemaReference,
                &schema_for(schema_epoch)?,
                factory,
                Arc::clone(&destination),
                retain,
            )
        }?;
        let directory = Arc::new(directory);
        // SAFETY: this journal holds the directory's exclusive lifetime lock
        // for the connection's whole life; the caller's factory and
        // destination come from the process's one funded native host.
        let conn = NativeJournalConnection::Live(unsafe {
            open_native(
                Arc::clone(&directory),
                OriginalNativeBoundMode::CreateExclusive,
                OriginalNativeDomain::ExternalSupervisorJournal,
                factory,
                destination,
            )
        }?);
        let initialized = (|| -> Result<ExternalSupervisorJournalStoreIdentity> {
            // The native keeper created the main inode exclusively; the outer
            // anchor is that exact inode.
            let store_identity = ExternalSupervisorJournalStoreIdentity {
                schema: identity_schema,
                journal_nonce: lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>()),
                directory_identity: directory.identity()?,
                database_identity: conn.main_identity()?,
                bootstrap_digest: bootstrap_digest.clone(),
                capture_digest: capture_digest.clone(),
            };
            store_identity.digest()?;
            conn.with_sql(|conn| {
                configure(conn)?;
                // Configuring reads the store, which creates its WAL
                // sidecars; their directory entries become durable before
                // the first commit appends to `-wal`.
                directory.sync()?;
                conn.execute_batch(&schema_for(schema_epoch)?)?;
                conn.pragma_update(None, "application_id", APPLICATION_ID)?;
                conn.pragma_update(None, "user_version", schema_epoch)?;
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                tx.execute(
                    "INSERT INTO external_supervisor_meta VALUES(1,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'prepared')",
                    params![
                        schema_epoch,
                        &store_identity.journal_nonce,
                        lillux::canonical_json(&serde_json::to_value(store_identity.directory_identity)?)?,
                        lillux::canonical_json(&serde_json::to_value(store_identity.database_identity)?)?,
                        &bootstrap_digest,
                        &bootstrap_json,
                        &capture_digest,
                        &capture_json,
                        signing_seed.as_str(),
                        &supervisor_public_key,
                        &attachment_request_digest,
                        &attachment_request_json,
                    ],
                )?;
                tx.commit()?;
                Ok(())
            })?;
            Ok(store_identity)
        })();
        let store_identity = match initialized {
            Ok(store_identity) => store_identity,
            Err(error) => return Err(conn.refuse(error)),
        };
        let store = SupervisorStore {
            conn,
            schema_reference,
            directory,
            _lifetime_lock: lifetime_lock,
            bootstrap,
            transport_capture,
            supervisor_signing_key,
            attachment_request,
            binding: None,
            launch_intent: None,
            store_identity,
        };
        if let Err(error) = store.validate() {
            return Err(store.refuse(error));
        }
        Ok(Self(store))
    }

    /// Close this journal's native connection; a refusal keeps the journal.
    pub fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.0
            .close()
            .map_err(|(store, error)| (Self(store), error))
    }

    pub fn store_identity(&self) -> &ExternalSupervisorJournalStoreIdentity {
        &self.0.store_identity
    }

    pub fn bootstrap(&self) -> &ExternalSupervisorBootstrap {
        &self.0.bootstrap
    }

    pub fn supervisor_signing_key(&self) -> &lillux::crypto::SigningKey {
        &self.0.supervisor_signing_key
    }

    pub fn attachment_request(&self) -> &ExternalChannelAttachRequest {
        &self.0.attachment_request
    }

    pub fn record_binding(
        self,
        binding: ExecutionChannelBinding,
    ) -> Result<AttachedExternalSupervisorJournal> {
        if !matches!(
            &self.0.transport_capture,
            ExternalSupervisorTransportCapture::Http(_)
        ) {
            return Err(self.0.refuse(anyhow::anyhow!(
                "relay attachment requires its exact authenticated response digest"
            )));
        }
        self.record_binding_inner(binding, None)
    }

    pub fn record_relay_binding(
        self,
        binding: ExecutionChannelBinding,
        response_digest: &str,
    ) -> Result<AttachedExternalSupervisorJournal> {
        if !matches!(
            &self.0.transport_capture,
            ExternalSupervisorTransportCapture::Relay(_)
        ) {
            return Err(self.0.refuse(anyhow::anyhow!(
                "HTTP attachment cannot retain a relay response digest"
            )));
        }
        if let Err(error) = hash(
            response_digest,
            "authenticated relay attachment response digest",
        ) {
            return Err(self.0.refuse(error));
        }
        self.record_binding_inner(binding, Some(response_digest))
    }

    /// A refusal consumes this stage: its connection closes and the terminal
    /// rides the returned error.
    fn record_binding_inner(
        mut self,
        binding: ExecutionChannelBinding,
        response_digest: Option<&str>,
    ) -> Result<AttachedExternalSupervisorJournal> {
        let recorded = (|| -> Result<()> {
            self.0.bootstrap.validate_attached_binding(
                &binding,
                &self.0.attachment_request.supervisor_public_key,
            )?;
            self.0.require_database_identity()?;
            let binding_digest = binding.digest()?;
            let binding_json = lillux::canonical_json(&serde_json::to_value(&binding)?)?;
            self.0.conn.with_sql(|conn| {
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                if let Some(response_digest) = response_digest {
                    tx.execute(
                        "INSERT INTO external_supervisor_binding VALUES(1,?1,?2,?3)",
                        params![&binding_digest, &binding_json, response_digest],
                    )?;
                } else {
                    tx.execute(
                        "INSERT INTO external_supervisor_binding VALUES(1,?1,?2)",
                        params![&binding_digest, &binding_json],
                    )?;
                }
                let changed = tx.execute(
                    "UPDATE external_supervisor_meta SET lifecycle='attached'
                     WHERE singleton=1 AND lifecycle='prepared'",
                    [],
                )?;
                ensure!(changed == 1, "external supervisor attachment is not fresh");
                tx.commit()?;
                Ok(())
            })
        })();
        if let Err(error) = recorded {
            return Err(self.0.refuse(error));
        }
        self.0.binding = Some(binding);
        if let Err(error) = self.0.validate() {
            return Err(self.0.refuse(error));
        }
        Ok(AttachedExternalSupervisorJournal(self.0))
    }
}

impl AttachedExternalSupervisorJournal {
    pub fn transport_capture(&self) -> &ExternalSupervisorTransportCapture {
        &self.0.transport_capture
    }

    pub fn captured_network_inputs(&self) -> Result<&ExternalCapturedNetworkInputs> {
        match &self.0.transport_capture {
            ExternalSupervisorTransportCapture::Http(value) => Ok(value),
            ExternalSupervisorTransportCapture::Relay(_) => {
                anyhow::bail!("relay journal has no network capture")
            }
        }
    }

    pub fn store_identity(&self) -> &ExternalSupervisorJournalStoreIdentity {
        &self.0.store_identity
    }

    pub fn bootstrap(&self) -> &ExternalSupervisorBootstrap {
        &self.0.bootstrap
    }

    pub fn binding(&self) -> &ExecutionChannelBinding {
        self.0.binding.as_ref().expect("attached stage has binding")
    }

    pub fn attachment_request(&self) -> &ExternalChannelAttachRequest {
        &self.0.attachment_request
    }

    pub fn relay_attach_response_digest(&self) -> Result<String> {
        ensure!(
            matches!(
                &self.0.transport_capture,
                ExternalSupervisorTransportCapture::Relay(_)
            ),
            "HTTP journal has no relay attachment response"
        );
        let digest: String = self.0.conn.with_sql(|conn| {
            Ok(conn.query_row(
                "SELECT attach_response_digest FROM external_supervisor_binding WHERE singleton=1",
                [],
                |row| row.get(0),
            )?)
        })?;
        hash(&digest, "retained relay attachment response digest")?;
        Ok(digest)
    }

    /// Close this journal's native connection; a refusal keeps the journal.
    pub fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.0
            .close()
            .map_err(|(store, error)| (Self(store), error))
    }

    pub fn supervisor_signing_key(&self) -> &lillux::crypto::SigningKey {
        &self.0.supervisor_signing_key
    }

    pub fn begin_launch(
        mut self,
        guest_store_identity: GuestJournalStoreIdentity,
        launcher_spec_digest: &str,
        launcher_bootstrap_digest: &str,
        launcher_artifact_digest: &str,
    ) -> Result<LaunchIntentExternalSupervisorJournal> {
        match self.record_launch_intent(
            guest_store_identity,
            launcher_spec_digest,
            launcher_bootstrap_digest,
            launcher_artifact_digest,
        ) {
            Ok(intent) => self.0.launch_intent = Some(intent),
            Err(error) => return Err(self.0.refuse(error)),
        }
        if let Err(error) = self.0.validate() {
            return Err(self.0.refuse(error));
        }
        Ok(LaunchIntentExternalSupervisorJournal(self.0))
    }

    fn record_launch_intent(
        &self,
        guest_store_identity: GuestJournalStoreIdentity,
        launcher_spec_digest: &str,
        launcher_bootstrap_digest: &str,
        launcher_artifact_digest: &str,
    ) -> Result<ExternalSupervisorLaunchIntent> {
        for (value, label) in [
            (launcher_spec_digest, "launcher specification digest"),
            (launcher_bootstrap_digest, "launcher bootstrap digest"),
            (launcher_artifact_digest, "launcher artifact digest"),
        ] {
            hash(value, label)?;
        }
        ensure!(
            launcher_spec_digest == launcher_bootstrap_digest,
            "launcher bootstrap changed its exact specification bytes"
        );
        let binding_digest = self.binding().digest()?;
        ensure!(
            guest_store_identity.binding_digest() == binding_digest
                && guest_store_identity.bootstrap_digest() == launcher_bootstrap_digest,
            "guest journal reservation changed launcher authority"
        );
        let guest_store_identity_digest = guest_store_identity.digest()?;
        let intent = ExternalSupervisorLaunchIntent {
            binding_digest: binding_digest.clone(),
            guest_store_identity_digest: guest_store_identity_digest.clone(),
            guest_store_identity,
            launcher_spec_digest: launcher_spec_digest.to_owned(),
            launcher_bootstrap_digest: launcher_bootstrap_digest.to_owned(),
            launcher_artifact_digest: launcher_artifact_digest.to_owned(),
        };
        let intent_json =
            lillux::canonical_json(&serde_json::to_value(&intent.guest_store_identity)?)?;
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO external_supervisor_launch_intent VALUES(1,?1,?2,?3,?4,?5,?6)",
                params![
                    &intent.binding_digest,
                    &intent.guest_store_identity_digest,
                    &intent_json,
                    &intent.launcher_spec_digest,
                    &intent.launcher_bootstrap_digest,
                    &intent.launcher_artifact_digest,
                ],
            )?;
            let changed = tx.execute(
                "UPDATE external_supervisor_meta SET lifecycle='launch_intent'
                 WHERE singleton=1 AND lifecycle='attached'",
                [],
            )?;
            ensure!(
                changed == 1,
                "external supervisor launch intent is not fresh"
            );
            tx.commit()?;
            Ok(())
        })?;
        Ok(intent)
    }
}

impl LaunchIntentExternalSupervisorJournal {
    pub fn transport_capture(&self) -> &ExternalSupervisorTransportCapture {
        &self.0.transport_capture
    }

    pub fn captured_network_inputs(&self) -> Result<&ExternalCapturedNetworkInputs> {
        match &self.0.transport_capture {
            ExternalSupervisorTransportCapture::Http(value) => Ok(value),
            ExternalSupervisorTransportCapture::Relay(_) => {
                anyhow::bail!("relay journal has no network capture")
            }
        }
    }

    pub fn store_identity(&self) -> &ExternalSupervisorJournalStoreIdentity {
        &self.0.store_identity
    }

    pub fn bootstrap(&self) -> &ExternalSupervisorBootstrap {
        &self.0.bootstrap
    }

    pub fn binding(&self) -> &ExecutionChannelBinding {
        self.0.binding.as_ref().expect("launch stage has binding")
    }

    pub fn supervisor_signing_key(&self) -> &lillux::crypto::SigningKey {
        &self.0.supervisor_signing_key
    }

    pub fn launch_intent(&self) -> &ExternalSupervisorLaunchIntent {
        self.0
            .launch_intent
            .as_ref()
            .expect("launch stage has intent")
    }

    pub fn validate(&self) -> Result<()> {
        self.0.validate()
    }

    /// Close this journal's native connection; a refusal keeps the journal.
    pub fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.0
            .close()
            .map_err(|(store, error)| (Self(store), error))
    }
}

impl ExternalSupervisorJournalRecovery {
    /// Reopen the retained journal under its outer anchor. Its schema
    /// reference closes into `retain` before the journal's own connection
    /// opens. A refusal raised after the native open carries the closed
    /// terminal in its error.
    pub fn open(
        directory: lillux::PinnedDirectory,
        store_identity: &ExternalSupervisorJournalStoreIdentity,
        factory: &Arc<OriginalNativeFactory>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
        retain: RetainTerminal<'_>,
    ) -> Result<Self> {
        store_identity.digest()?;
        let schema_epoch = schema_epoch_for_identity(store_identity.schema)?;
        directory.require_owner_private_directory()?;
        ensure!(
            directory.identity()? == store_identity.directory_identity,
            "external supervisor directory changed its outer identity"
        );
        let lifetime_lock = directory
            .try_lock_exclusive()?
            .context("external supervisor directory still has a live owner")?;
        lifetime_lock.ensure_protects(&directory)?;
        // WAL sidecars a crash left behind are admitted: the native keeper
        // pins them and SQLite replays their committed frames. A rollback
        // journal is refused.
        native_journal::require_wal_entries(
            &directory,
            DATABASE_NAME,
            "external supervisor store",
        )?;
        {
            // Metadata check before any SQLite effect: this descriptor is
            // closed before the native keeper takes its own.
            let database_file = directory
                .open_regular(OsStr::new(DATABASE_NAME), false)?
                .context("external supervisor database is absent")?;
            ensure!(
                lillux::matches_pinned_regular_file_identity(
                    &database_file,
                    store_identity.database_identity,
                )?,
                "external supervisor database changed its outer inode identity"
            );
            ensure!(
                lillux::observe_open_regular_file(&database_file)?.size() > 0,
                "external supervisor database was never initialized"
            );
        }
        // SAFETY: this journal holds the directory's exclusive lifetime lock
        // for the whole capture; the caller's factory and destination come
        // from the process's one funded native host.
        let schema_reference = unsafe {
            native_journal::capture_schema_reference(
                OriginalNativeDomain::ExternalSupervisorJournalSchemaReference,
                &schema_for(schema_epoch)?,
                factory,
                Arc::clone(&destination),
                retain,
            )
        }?;
        let directory = Arc::new(directory);
        // SAFETY: this journal holds the directory's exclusive lifetime lock
        // for the connection's whole life; the caller's factory and
        // destination come from the process's one funded native host.
        let conn = NativeJournalConnection::Live(unsafe {
            open_native(
                Arc::clone(&directory),
                OriginalNativeBoundMode::Existing,
                OriginalNativeDomain::ExternalSupervisorJournal,
                factory,
                destination,
            )
        }?);
        let loaded = (|| -> Result<_> {
            ensure!(
                conn.main_identity()? == store_identity.database_identity,
                "external supervisor database changed its outer inode identity"
            );
            let loaded = conn.with_sql(|conn| {
                configure(conn)?;
                validate_schema(conn, &directory, schema_epoch, &schema_reference)?;
                load_content(conn)
            })?;
            directory.sync()?;
            Ok(loaded)
        })();
        let (
            bootstrap,
            transport_capture,
            supervisor_signing_key,
            attachment_request,
            binding,
            launch_intent,
        ) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => return Err(conn.refuse(error)),
        };
        let store = SupervisorStore {
            conn,
            schema_reference,
            directory,
            _lifetime_lock: lifetime_lock,
            bootstrap,
            transport_capture,
            supervisor_signing_key,
            attachment_request,
            binding,
            launch_intent,
            store_identity: store_identity.clone(),
        };
        if let Err(error) = store.validate() {
            return Err(store.refuse(error));
        }
        Ok(if store.launch_intent.is_some() {
            Self::LaunchIntent(RecoveredExternalSupervisorJournal(store))
        } else if store.binding.is_some() {
            Self::Attached(AttachedExternalSupervisorJournal(store))
        } else {
            Self::Prepared(PreparedExternalSupervisorJournal(store))
        })
    }
}

impl RecoveredExternalSupervisorJournal {
    pub fn transport_capture(&self) -> &ExternalSupervisorTransportCapture {
        &self.0.transport_capture
    }

    pub fn captured_network_inputs(&self) -> Result<&ExternalCapturedNetworkInputs> {
        match &self.0.transport_capture {
            ExternalSupervisorTransportCapture::Http(value) => Ok(value),
            ExternalSupervisorTransportCapture::Relay(_) => {
                anyhow::bail!("relay journal has no network capture")
            }
        }
    }

    pub fn store_identity(&self) -> &ExternalSupervisorJournalStoreIdentity {
        &self.0.store_identity
    }

    pub fn bootstrap(&self) -> &ExternalSupervisorBootstrap {
        &self.0.bootstrap
    }

    pub fn binding(&self) -> &ExecutionChannelBinding {
        self.0
            .binding
            .as_ref()
            .expect("launch recovery has binding")
    }

    pub fn launch_intent(&self) -> &ExternalSupervisorLaunchIntent {
        self.0
            .launch_intent
            .as_ref()
            .expect("launch recovery has intent")
    }

    pub fn validate(&self) -> Result<()> {
        self.0.validate()
    }

    /// Close this journal's native connection; a refusal keeps the journal.
    pub fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.0
            .close()
            .map_err(|(store, error)| (Self(store), error))
    }
}

impl SupervisorStore {
    /// The native keeper still binds the database name to the anchored inode.
    fn require_database_identity(&self) -> Result<()> {
        ensure!(
            self.conn.main_identity()? == self.store_identity.database_identity,
            "external supervisor database identity changed"
        );
        Ok(())
    }

    fn close(self) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        let Self {
            conn,
            schema_reference,
            directory,
            _lifetime_lock,
            bootstrap,
            transport_capture,
            supervisor_signing_key,
            attachment_request,
            binding,
            launch_intent,
            store_identity,
        } = self;
        // The lifetime lock is released only after the connection closed.
        match conn.close() {
            Ok(terminal) => Ok(terminal),
            Err((conn, error)) => Err((
                Self {
                    conn,
                    schema_reference,
                    directory,
                    _lifetime_lock,
                    bootstrap,
                    transport_capture,
                    supervisor_signing_key,
                    attachment_request,
                    binding,
                    launch_intent,
                    store_identity,
                },
                error,
            )),
        }
    }

    /// Close this store because `error` refused it; the terminal, or the
    /// refused close owner, rides the returned error. The lifetime lock is
    /// released only after the close.
    fn refuse(self, error: anyhow::Error) -> anyhow::Error {
        let Self {
            conn,
            schema_reference,
            directory,
            _lifetime_lock,
            bootstrap,
            transport_capture,
            supervisor_signing_key,
            attachment_request,
            binding,
            launch_intent,
            store_identity,
        } = self;
        let error = conn.refuse(error);
        drop((
            schema_reference,
            directory,
            _lifetime_lock,
            bootstrap,
            transport_capture,
            supervisor_signing_key,
            attachment_request,
            binding,
            launch_intent,
            store_identity,
        ));
        error
    }

    fn validate(&self) -> Result<()> {
        self.require_database_identity()?;
        self.store_identity.digest()?;
        ensure!(
            matches!(
                (
                    &self.transport_capture,
                    &self.bootstrap.controller,
                    self.store_identity.schema
                ),
                (
                    ExternalSupervisorTransportCapture::Http(_),
                    ExternalControllerTransportSelection::HttpJson(_),
                    3
                ) | (
                    ExternalSupervisorTransportCapture::Relay(_),
                    ExternalControllerTransportSelection::ControllerFileRelay(_),
                    4
                )
            ),
            "external supervisor transport capture changed journal identity kind"
        );
        // The retained row is checked against the independent outer anchor
        // first, so a replaced capture is named as an anchor change.
        self.conn
            .with_sql(|conn| validate_store_identity_row(conn, &self.store_identity))?;
        ensure!(
            self.store_identity.directory_identity == self.directory.identity()?
                && self.store_identity.bootstrap_digest == self.bootstrap.digest()?
                && self.store_identity.capture_digest == self.transport_capture.digest()?,
            "external supervisor store changed its retained identity"
        );
        let (
            loaded_bootstrap,
            loaded_capture,
            loaded_key,
            loaded_request,
            loaded_binding,
            loaded_launch,
        ) = self.conn.with_sql(|conn| {
            validate_schema(
                conn,
                &self.directory,
                schema_epoch_for_identity(self.store_identity.schema)?,
                &self.schema_reference,
            )?;
            load_content(conn)
        })?;
        ensure!(
            loaded_bootstrap.canonical_bytes()? == self.bootstrap.canonical_bytes()?
                && loaded_capture == self.transport_capture
                && loaded_request.canonical_bytes()?
                    == self.attachment_request.canonical_bytes()?
                && loaded_key.to_bytes() == self.supervisor_signing_key.to_bytes()
                && loaded_binding == self.binding
                && loaded_launch == self.launch_intent,
            "external supervisor database changed its retained content"
        );
        Ok(())
    }
}

/// The stored identity, epoch and integrity, then the complete schema SQL
/// against the definition captured for `epoch` from a native reference.
fn validate_schema(
    conn: &Connection,
    directory: &lillux::PinnedDirectory,
    epoch: i64,
    reference: &SchemaSqlDefinition,
) -> Result<()> {
    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let quick_check: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(
        application_id == i64::from(APPLICATION_ID) && user_version == epoch && quick_check == "ok",
        "external supervisor database is not the exact current schema"
    );
    reference.require_matches(conn, &directory.path().join(DATABASE_NAME))
}

/// The journal epoch an outer store identity selects: an HTTP identity
/// (schema 3) stores the HTTP epoch and a relay identity (schema 4) the
/// relay epoch, exactly as creation pairs them.
fn schema_epoch_for_identity(identity_schema: u32) -> Result<i64> {
    match identity_schema {
        3 => Ok(HTTP_SCHEMA_EPOCH),
        4 => Ok(RELAY_SCHEMA_EPOCH),
        _ => anyhow::bail!("unsupported supervisor journal identity schema"),
    }
}

fn schema_for(epoch: i64) -> Result<String> {
    match epoch {
        HTTP_SCHEMA_EPOCH => Ok(SCHEMA.to_owned()),
        RELAY_SCHEMA_EPOCH => {
            let binding_column = "binding_json TEXT NOT NULL\n);";
            ensure!(
                SCHEMA.matches(binding_column).count() == 1,
                "relay schema lost its binding table anchor"
            );
            Ok(SCHEMA
                .replace("CHECK(schema_epoch=6)", "CHECK(schema_epoch=7)")
                .replace(
                    binding_column,
                    "binding_json TEXT NOT NULL,\n    attach_response_digest TEXT NOT NULL\n);",
                ))
        }
        _ => anyhow::bail!("unsupported external supervisor journal schema"),
    }
}

fn validate_store_identity_row(
    conn: &Connection,
    expected: &ExternalSupervisorJournalStoreIdentity,
) -> Result<()> {
    let (journal_nonce, directory_json, database_json, bootstrap_digest, capture_digest): (
        String,
        String,
        String,
        String,
        String,
    ) = conn.query_row(
        "SELECT journal_nonce,directory_identity_json,database_identity_json,bootstrap_digest,capture_digest
         FROM external_supervisor_meta WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    let directory_identity: lillux::PinnedDirectoryIdentity =
        serde_json::from_str(&directory_json)?;
    let database_identity: lillux::PinnedRegularFileIdentity =
        serde_json::from_str(&database_json)?;
    ensure!(
        lillux::canonical_json(&serde_json::to_value(directory_identity)?)? == directory_json
            && lillux::canonical_json(&serde_json::to_value(database_identity)?)? == database_json
            && journal_nonce == expected.journal_nonce
            && directory_identity == expected.directory_identity
            && database_identity == expected.database_identity
            && bootstrap_digest == expected.bootstrap_digest
            && capture_digest == expected.capture_digest,
        "external supervisor database changed its outer anchor"
    );
    Ok(())
}

fn load_content(
    conn: &Connection,
) -> Result<(
    ExternalSupervisorBootstrap,
    ExternalSupervisorTransportCapture,
    lillux::crypto::SigningKey,
    ExternalChannelAttachRequest,
    Option<ExecutionChannelBinding>,
    Option<ExternalSupervisorLaunchIntent>,
)> {
    let (
        journal_nonce,
        bootstrap_digest,
        bootstrap_json,
        capture_digest,
        capture_json,
        signing_seed_base64,
        supervisor_public_key,
        attachment_request_digest,
        attachment_request_json,
        lifecycle,
    ): (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = conn.query_row(
        "SELECT journal_nonce,bootstrap_digest,bootstrap_json,capture_digest,capture_json,
                supervisor_signing_seed_base64,supervisor_public_key,
                attachment_request_digest,attachment_request_json,lifecycle
         FROM external_supervisor_meta WHERE singleton=1",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                bounded_capture_json(row)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        },
    )?;
    hash(&journal_nonce, "supervisor journal nonce")?;
    let bootstrap: ExternalSupervisorBootstrap = serde_json::from_str(&bootstrap_json)?;
    ensure!(
        bootstrap.canonical_bytes()? == bootstrap_json.as_bytes()
            && bootstrap.digest()? == bootstrap_digest,
        "external supervisor bootstrap changed its canonical identity"
    );
    let signing_seed_base64 = Zeroizing::new(signing_seed_base64);
    let schema_epoch: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let transport_capture = match (schema_epoch, &bootstrap.controller) {
        (HTTP_SCHEMA_EPOCH, ExternalControllerTransportSelection::HttpJson(http)) => {
            let capture: ExternalCapturedNetworkInputs = serde_json::from_str(&capture_json)
                .map_err(|_| {
                    anyhow::anyhow!("external supervisor network capture JSON is invalid")
                })?;
            capture.validate_for(&http.network_inputs)?;
            ensure!(
                lillux::canonical_json(&serde_json::to_value(&capture)?)? == capture_json
                    && capture.digest()? == capture_digest,
                "external supervisor network capture changed its canonical identity"
            );
            ExternalSupervisorTransportCapture::Http(capture)
        }
        (RELAY_SCHEMA_EPOCH, ExternalControllerTransportSelection::ControllerFileRelay(_)) => {
            let capture: ExternalRelayMailboxCapture = serde_json::from_str(&capture_json)
                .map_err(|_| {
                    anyhow::anyhow!("external supervisor relay capture JSON is invalid")
                })?;
            capture.validate_for(&bootstrap)?;
            ensure!(
                lillux::canonical_json(&serde_json::to_value(&capture)?)? == capture_json
                    && capture.digest()? == capture_digest,
                "external supervisor relay capture changed its canonical identity"
            );
            ExternalSupervisorTransportCapture::Relay(capture)
        }
        _ => anyhow::bail!("external supervisor journal transport differs from schema epoch"),
    };
    let signing_seed = Zeroizing::new(
        STANDARD
            .decode(signing_seed_base64.as_bytes())
            .map_err(|_| anyhow::anyhow!("external supervisor signing seed is invalid"))?,
    );
    let signing_seed =
        Zeroizing::new(<[u8; 32]>::try_from(signing_seed.as_slice()).map_err(|_| {
            anyhow::anyhow!("external supervisor signing seed has the wrong length")
        })?);
    let supervisor_signing_key = lillux::crypto::SigningKey::from_bytes(&signing_seed);
    ensure!(
        encode_channel_public_key(&supervisor_signing_key.verifying_key())?
            == supervisor_public_key,
        "external supervisor signing key changed its public identity"
    );
    let attachment_request: ExternalChannelAttachRequest =
        serde_json::from_str(&attachment_request_json)?;
    ensure!(
        attachment_request.canonical_bytes()? == attachment_request_json.as_bytes()
            && attachment_request.digest()? == attachment_request_digest
            && attachment_request.supervisor_public_key == supervisor_public_key,
        "external supervisor attachment request changed its exact bytes"
    );
    attachment_request.validate_for_bootstrap(&bootstrap)?;
    let binding_sql = if schema_epoch == RELAY_SCHEMA_EPOCH {
        "SELECT binding_digest,binding_json,attach_response_digest FROM external_supervisor_binding WHERE singleton=1"
    } else {
        "SELECT binding_digest,binding_json,NULL FROM external_supervisor_binding WHERE singleton=1"
    };
    let binding = conn
        .query_row(binding_sql, [], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .optional()?
        .map(|(digest, json, response_digest)| -> Result<_> {
            if schema_epoch == RELAY_SCHEMA_EPOCH {
                hash(
                    response_digest
                        .as_deref()
                        .context("relay attachment lost response digest")?,
                    "retained relay attachment response digest",
                )?;
            } else {
                ensure!(
                    response_digest.is_none(),
                    "HTTP attachment gained a relay response"
                );
            }
            let binding: ExecutionChannelBinding = serde_json::from_str(&json)?;
            ensure!(
                lillux::canonical_json(&serde_json::to_value(&binding)?)? == json
                    && binding.digest()? == digest,
                "external supervisor binding changed its exact bytes"
            );
            bootstrap.validate_attached_binding(&binding, &supervisor_public_key)?;
            Ok(binding)
        })
        .transpose()?;
    let launch_intent = conn
        .query_row(
            "SELECT binding_digest,guest_store_identity_digest,guest_store_identity_json,
                    launcher_spec_digest,launcher_bootstrap_digest,launcher_artifact_digest
             FROM external_supervisor_launch_intent WHERE singleton=1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?, row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?
        .map(|(binding_digest, guest_digest, guest_json, spec, launcher_bootstrap, artifact)| -> Result<_> {
            let guest_store_identity: GuestJournalStoreIdentity = serde_json::from_str(&guest_json)?;
            ensure!(
                lillux::canonical_json(&serde_json::to_value(&guest_store_identity)?)? == guest_json
                    && guest_store_identity.digest()? == guest_digest,
                "external supervisor guest anchor changed its exact bytes"
            );
            for (value, label) in [
                (binding_digest.as_str(), "launch binding digest"),
                (spec.as_str(), "launcher specification digest"),
                (launcher_bootstrap.as_str(), "launcher bootstrap digest"),
                (artifact.as_str(), "launcher artifact digest"),
            ] {
                hash(value, label)?;
            }
            ensure!(
                binding.as_ref().is_some_and(|value| value.digest().ok().as_deref() == Some(&binding_digest))
                    && guest_store_identity.binding_digest() == binding_digest
                    && guest_store_identity.bootstrap_digest() == launcher_bootstrap
                    && spec == launcher_bootstrap,
                "external supervisor launch intent changed its authority"
            );
            Ok(ExternalSupervisorLaunchIntent {
                binding_digest,
                guest_store_identity_digest: guest_digest,
                guest_store_identity,
                launcher_spec_digest: spec,
                launcher_bootstrap_digest: launcher_bootstrap,
                launcher_artifact_digest: artifact,
            })
        })
        .transpose()?;
    ensure!(
        matches!(
            (
                lifecycle.as_str(),
                binding.is_some(),
                launch_intent.is_some()
            ),
            ("prepared", false, false) | ("attached", true, false) | ("launch_intent", true, true)
        ),
        "external supervisor lifecycle contradicts its retained facts"
    );
    Ok((
        bootstrap,
        transport_capture,
        supervisor_signing_key,
        attachment_request,
        binding,
        launch_intent,
    ))
}

/// Check borrowed SQLite bytes before allocating an owned string. Never retain
/// raw protected bytes or parser diagnostics in an error chain.
fn bounded_capture_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<String> {
    let invalid = || {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "external supervisor network capture JSON is invalid or exceeds bounds",
            )),
        )
    };
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(4)? else {
        return Err(invalid());
    };
    if bytes.len() > MAX_EXTERNAL_NETWORK_CAPTURE_JSON_BYTES {
        return Err(invalid());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    Ok(text.to_owned())
}

fn hash(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not a canonical SHA-256 digest"
    );
    Ok(())
}

/// WAL, `synchronous=FULL` and foreign keys, each read back. The crash
/// argument for this contract is in [`super::native_journal`].
fn configure(conn: &Connection) -> Result<()> {
    native_journal::configure(conn, "external supervisor journal")
}

/// Open the supervisor database on one bound ticket of `factory`.
///
/// # Safety
/// The caller holds the exclusive lifetime lock of `directory` for as long as
/// the returned connection lives, and `factory` and `destination` come from
/// the one native host funded for this supervisor process.
unsafe fn open_native(
    directory: Arc<lillux::PinnedDirectory>,
    mode: OriginalNativeBoundMode,
    domain: OriginalNativeDomain,
    factory: &Arc<OriginalNativeFactory>,
    destination: Arc<dyn OriginalNativeDestinationAuthority>,
) -> Result<OriginalNativeConnection> {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        native_journal::open_bound(
            directory,
            OsStr::new(DATABASE_NAME),
            mode,
            domain,
            factory,
            destination,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::external_execution::admission::{
        AdmittedExternalCandidateProgram, ExternalCandidateProcFilesystem,
        ExternalCandidateRequirement, ExternalCandidateRuntimeRecipe, PROTOCOL,
    };
    use crate::external_execution::guest_journal::PreparedGuestJournal;
    use crate::external_execution::native_journal::testing::{census, refusal, retire, root};
    use crate::external_execution::transport::{
        EXTERNAL_CHANNEL_ROUTE_CONTRACT, ExternalControllerTransportContract,
        external_tls_root_bundle_digest,
    };
    use crate::test_support::shared_test_host;
    use lillux_sqlite::test_host::TestNativeHost;

    fn bootstrap() -> ExternalSupervisorBootstrap {
        let recipe = ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        };
        let recipe_digest = recipe.digest().unwrap();
        let roots = vec![STANDARD.encode(b"fixture root")];
        let guest_inputs = ryeos_external_execution_contract::ExternalGuestInputProjection {
            workspace_view: ryeos_external_execution_contract::GuestWorkspaceView::FullSnapshot,
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: ryeos_external_execution_contract::GuestBaseSnapshotInput {
                descriptor: 55,
                snapshot_hash: "c".repeat(64),
                closure_digest: "5".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![ryeos_external_execution_contract::GuestMountInput {
                role: ryeos_external_execution_contract::GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: 64,
                destination: "/runtime".into(),
                kind: ryeos_external_execution_contract::GuestMountKind::Directory,
                access: ryeos_external_execution_contract::GuestMountAccess::ReadOnly,
                normalized_mode: None,
                content_authority:
                    ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                        manifest_kind:
                            ryeos_external_execution_contract::GuestProductManifestKind::Content,
                        manifest_hash: "e".repeat(64),
                        manifest_descriptor: 65,
                        manifest_bytes: 256,
                    },
                bytes: 1,
            }],
            executable_search: vec!["/runtime/bin".into()],
            environment: BTreeMap::new(),
        };
        let guest_input_identity = guest_inputs.identity_digest().unwrap();
        let requirement = ExternalCandidateRequirement {
            schema: 7,
            protocol: PROTOCOL.into(),
            connector_protocol: crate::external_execution::admission::CONNECTOR_PROTOCOL.into(),
            execution_route:
                crate::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
            required_lifecycle_capabilities: std::collections::BTreeSet::new(),
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "runtime".into(),
            runtime_authority: crate::external_execution::admission::ExternalCandidateRuntimeAuthority::CapturedProduct,
            runtime_recipe: recipe,
        };
        let qualification_use =
            crate::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        ExternalSupervisorBootstrap {
            schema: super::super::transport::EXTERNAL_SUPERVISOR_HTTP_BOOTSTRAP_SCHEMA,
            controller: ExternalControllerTransportContract {
                schema: 2,
                network_inputs: super::super::transport::ExternalNetworkInputPolicy {
                    resolver: super::super::transport::ExternalNetworkInputSelection {
                        source: "/etc/resolv.conf".into(),
                        max_bytes: 65536,
                    },
                    hosts: super::super::transport::ExternalNetworkInputSelection {
                        source: "/etc/hosts".into(),
                        max_bytes: 65536,
                    },
                },
                https_origin: "https://controller.example".into(),
                route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
                tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
                connect_timeout_ms: 1_000,
                request_timeout_ms: 2_000,
                maximum_response_bytes: 64 * 1024,
            }.into(),
            tls_root_certificates_der_base64: roots,
            placement_thread_id: "T-supervisor-journal".into(),
            occurrence_id: "occurrence-supervisor-journal".into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            launcher_artifact_hash: "4".repeat(64),
            candidate_program: AdmittedExternalCandidateProgram {
                requirement,
                qualification_use,
                runtime_manifest_kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
                runtime_manifest_hash: "e".repeat(64),
                runtime_source: crate::external_execution::admission::ExternalCandidateRuntimeSource::CapturedProduct { witness_hash: "1".repeat(64) },
                qualification_attestation_hash: "2".repeat(64),
                selection_identity_digest: "3".repeat(64),
                runtime_recipe_digest: recipe_digest,
            }
            .into(),
            guest_input_identity,
            guest_inputs,
            owner_public_key: encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[11; 32]).verifying_key(),
            )
            .unwrap(),
            bootstrap_capability: STANDARD.encode([12_u8; 32]),
            attachment_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap()
                + 60_000,
            execution_timeout_seconds: 60,
            post_execution_timeout_seconds: 120,
            candidate_export_max_bytes: 512 * 1024,
            channel_max_bytes: 1024 * 1024,
            native_journal: crate::external_execution::transport::ExternalSupervisorNativeEnvelope {
                storage_bytes: 67_108_864,
                allocator_steps: 2_000_000_000,
                connections: 16,
                operation_millis: 4_860_000,
                cleanup_millis: 120_000,
                cleanup_steps: 3_000_000,
            },
        }
    }

    fn binding(
        bootstrap: &ExternalSupervisorBootstrap,
        supervisor: &lillux::crypto::SigningKey,
    ) -> ExecutionChannelBinding {
        let issued_at_ms = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        ExecutionChannelBinding {
            schema: crate::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
            placement_thread_id: bootstrap.placement_thread_id.clone(),
            allocation_request_digest: bootstrap.allocation_request_digest.clone(),
            occurrence_id: bootstrap.occurrence_id.clone(),
            admitted_capsule_hash: bootstrap.admitted_capsule_hash.clone(),
            base_snapshot_hash: bootstrap.base_snapshot_hash.clone(),
            execution_binding_hash: bootstrap.execution_binding_hash.clone(),
            supervisor_runtime_hash: bootstrap.supervisor_runtime_hash.clone(),
            candidate_program_digest: bootstrap.candidate_program.digest().unwrap(),
            channel_nonce: "f".repeat(64),
            owner_public_key: bootstrap.owner_public_key.clone(),
            supervisor_public_key: encode_channel_public_key(&supervisor.verifying_key()).unwrap(),
            issued_at_ms,
            execution_deadline_ms: issued_at_ms + 60_000,
            expires_at_ms: issued_at_ms + 180_000,
            candidate_export_max_bytes: bootstrap.candidate_export_max_bytes,
            max_frames: bootstrap.binding_max_frames().unwrap(),
            max_bytes: bootstrap.channel_max_bytes,
        }
    }

    fn state_authority() -> (tempfile::TempDir, crate::PinnedStateAuthority) {
        let root = tempfile::tempdir().unwrap();
        let db = crate::test_support::TestState::open(
            root.path(),
            std::sync::Arc::new(crate::TrustStore::new()),
        )
        .unwrap();
        let authority = db.pinned_authority().unwrap();
        drop(db);
        (root, authority)
    }

    fn create(
        host: &TestNativeHost,
        directory: lillux::PinnedDirectory,
        bootstrap: ExternalSupervisorBootstrap,
        capture: ExternalCapturedNetworkInputs,
        supervisor: lillux::crypto::SigningKey,
    ) -> Result<PreparedExternalSupervisorJournal> {
        PreparedExternalSupervisorJournal::create(
            directory,
            bootstrap,
            capture,
            supervisor,
            host.factory(),
            host.destination(),
            &census(host),
        )
    }

    fn recover(
        host: &TestNativeHost,
        root: &tempfile::TempDir,
        identity: &ExternalSupervisorJournalStoreIdentity,
    ) -> Result<ExternalSupervisorJournalRecovery> {
        ExternalSupervisorJournalRecovery::open(
            lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
            identity,
            host.factory(),
            host.destination(),
            &census(host),
        )
    }

    fn close_recovery(host: &TestNativeHost, recovery: ExternalSupervisorJournalRecovery) {
        match recovery {
            ExternalSupervisorJournalRecovery::Prepared(journal) => retire(host, journal.close()),
            ExternalSupervisorJournalRecovery::Attached(journal) => retire(host, journal.close()),
            ExternalSupervisorJournalRecovery::LaunchIntent(journal) => {
                retire(host, journal.close())
            }
        }
    }

    fn recovery_error(
        host: &TestNativeHost,
        root: &tempfile::TempDir,
        identity: &ExternalSupervisorJournalStoreIdentity,
    ) -> String {
        refusal(host, recover(host, root, identity))
    }

    /// Run `work` on the closed journal's database through the native gate.
    fn with_journal<T>(
        host: &TestNativeHost,
        root_dir: &tempfile::TempDir,
        work: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> T {
        host.with_existing(
            root(root_dir.path()),
            OsStr::new(DATABASE_NAME),
            OriginalNativeDomain::ExternalSupervisorJournal,
            work,
        )
    }

    fn mutate_meta_without_guard(
        host: &TestNativeHost,
        root: &tempfile::TempDir,
        mutation: impl FnOnce(&Connection),
    ) {
        with_journal(host, root, |conn| {
            let trigger_sql: String = conn.query_row(
                "SELECT sql FROM sqlite_schema
                 WHERE type='trigger' AND name='external_supervisor_meta_transition'",
                [],
                |row| row.get(0),
            )?;
            conn.execute_batch("DROP TRIGGER external_supervisor_meta_transition")?;
            mutation(conn);
            conn.execute_batch(&trigger_sql)?;
            Ok(())
        })
    }

    fn capture() -> ExternalCapturedNetworkInputs {
        ExternalCapturedNetworkInputs::from_bytes(
            &bootstrap().controller.http().unwrap().network_inputs,
            b"nameserver 127.0.0.1\n",
            b"127.0.0.1 localhost\n",
        )
        .unwrap()
    }

    #[test]
    fn invalid_capture_is_refused_before_store_writes() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let mut invalid = capture();
        invalid.policy_digest = "0".repeat(64);
        refusal(
            &host,
            create(
                &host,
                directory,
                bootstrap(),
                invalid,
                lillux::crypto::SigningKey::from_bytes(&[13; 32]),
            ),
        );
        assert!(
            lillux::PinnedDirectory::open(root.path())
                .unwrap()
                .unwrap()
                .entry_names()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn network_capture_survives_recovery_and_immutable_updates_are_refused() {
        let host = shared_test_host();
        let (root, identity) = fresh_journal(&host);
        with_journal(&host, &root, |conn| {
            for sql in [
                "UPDATE external_supervisor_meta SET capture_json='{}' WHERE singleton=1",
                "UPDATE external_supervisor_meta SET capture_digest='changed' WHERE singleton=1",
            ] {
                assert!(conn.execute(sql, []).is_err());
            }
            Ok(())
        });
        let ExternalSupervisorJournalRecovery::Prepared(recovered) =
            recover(&host, &root, &identity).unwrap()
        else {
            panic!("wrong recovered state")
        };
        assert_eq!(
            recovered
                .captured_network_inputs()
                .unwrap()
                .digest()
                .unwrap(),
            capture().digest().unwrap()
        );
        retire(&host, recovered.close());
    }

    #[test]
    fn network_capture_missing_tampered_or_replaced_cannot_recover() {
        let host = shared_test_host();
        for mutation in 0..5 {
            let (root, identity) = fresh_journal(&host);
            mutate_meta_without_guard(&host, &root, |conn| {
                if mutation == 0 {
                    conn.execute(
                        "UPDATE external_supervisor_meta SET capture_json='{}' WHERE singleton=1",
                        [],
                    )
                    .unwrap();
                    return;
                }
                let mut replacement = capture();
                if mutation == 1 {
                    replacement.resolver_base64 = STANDARD.encode(b"tampered");
                } else if mutation == 2 {
                    let mut policy = bootstrap()
                        .controller
                        .http()
                        .unwrap()
                        .network_inputs
                        .clone();
                    policy.hosts.source = "/other/hosts".into();
                    replacement =
                        ExternalCapturedNetworkInputs::from_bytes(&policy, b"", b"").unwrap();
                } else if mutation == 3 {
                    // Internally valid replacement with a matching row digest must
                    // still fail the independently retained outer anchor.
                    replacement = ExternalCapturedNetworkInputs::from_bytes(
                        &bootstrap().controller.http().unwrap().network_inputs,
                        b"different",
                        b"",
                    )
                    .unwrap();
                }
                let mut json =
                    lillux::canonical_json(&serde_json::to_value(&replacement).unwrap()).unwrap();
                if mutation == 4 {
                    json.push(' ');
                }
                let digest = if mutation == 1 {
                    identity.capture_digest.clone()
                } else {
                    replacement.digest().unwrap()
                };
                conn.execute("UPDATE external_supervisor_meta SET capture_json=?1,capture_digest=?2 WHERE singleton=1", params![json, digest]).unwrap();
            });
            let error = recovery_error(&host, &root, &identity);
            if mutation == 3 {
                assert!(error.contains("outer anchor"), "{error}");
            }
        }
    }

    #[test]
    fn malformed_network_capture_recovery_errors_are_bounded_and_private() {
        const SENTINEL: &str = "PROTECTED_NETWORK_INPUT_SENTINEL";
        let host = shared_test_host();
        let mut unknown = serde_json::to_value(capture()).unwrap();
        unknown[SENTINEL] = serde_json::json!(SENTINEL);
        let mut wrong_scalar = serde_json::to_value(capture()).unwrap();
        wrong_scalar["schema"] = serde_json::json!(SENTINEL);
        let mut oversized =
            lillux::canonical_json(&serde_json::to_value(capture()).unwrap()).unwrap();
        oversized.push_str(SENTINEL);
        oversized.push_str(&" ".repeat(MAX_EXTERNAL_NETWORK_CAPTURE_JSON_BYTES));
        for json in [
            serde_json::to_string(&unknown).unwrap(),
            serde_json::to_string(&wrong_scalar).unwrap(),
            serde_json::to_string(SENTINEL).unwrap(),
            oversized,
        ] {
            let oversized = json.len() > MAX_EXTERNAL_NETWORK_CAPTURE_JSON_BYTES;
            let (root, identity) = fresh_journal(&host);
            mutate_meta_without_guard(&host, &root, |conn| {
                conn.execute(
                    "UPDATE external_supervisor_meta SET capture_json=?1 WHERE singleton=1",
                    params![json],
                )
                .unwrap();
            });
            let error = match recover(&host, &root, &identity) {
                Ok(recovery) => {
                    close_recovery(&host, recovery);
                    panic!("malformed capture recovered")
                }
                Err(error) => error,
            };
            for rendered in [
                format!("{error}"),
                format!("{error:#}"),
                format!("{error:?}"),
                format!("{error:#?}"),
            ] {
                assert!(!rendered.contains(SENTINEL));
                assert!(rendered.contains("network capture JSON"));
                assert!(rendered.len() < 2048);
                if oversized {
                    assert!(rendered.contains("exceeds bounds"));
                }
            }
            for cause in error.chain() {
                assert!(!format!("{cause:?}").contains(SENTINEL));
            }
            refusal(&host, Err::<(), _>(error));
        }
    }

    #[test]
    fn predecessor_or_missing_capture_identity_is_refused() {
        let host = shared_test_host();
        let (root, mut identity) = fresh_journal(&host);
        let mut value = serde_json::to_value(&identity).unwrap();
        value.as_object_mut().unwrap().remove("capture_digest");
        assert!(serde_json::from_value::<ExternalSupervisorJournalStoreIdentity>(value).is_err());
        identity.schema = 1;
        assert!(recovery_error(&host, &root, &identity).contains("identity schema"));
    }

    fn fresh_journal(
        host: &TestNativeHost,
    ) -> (tempfile::TempDir, ExternalSupervisorJournalStoreIdentity) {
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let journal = create(
            host,
            directory,
            bootstrap(),
            capture(),
            lillux::crypto::SigningKey::from_bytes(&[13; 32]),
        )
        .unwrap();
        let identity = journal.store_identity().clone();
        retire(host, journal.close());
        (root, identity)
    }

    fn launched_journal(
        host: &TestNativeHost,
    ) -> (tempfile::TempDir, ExternalSupervisorJournalStoreIdentity) {
        let outer = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(outer.path())
            .unwrap()
            .unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let bootstrap = bootstrap();
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[13; 32]);
        let expected_binding = binding(&bootstrap, &supervisor);
        let journal = create(host, directory, bootstrap, capture(), supervisor).unwrap();
        let identity = journal.store_identity().clone();
        let attached = journal.record_binding(expected_binding.clone()).unwrap();
        let guest = tempfile::tempdir().unwrap();
        let guest_directory = lillux::PinnedDirectory::open(guest.path())
            .unwrap()
            .unwrap();
        guest_directory.tighten_owner_private_directory().unwrap();
        let (_state_root, authority) = state_authority();
        let launcher_digest = "7".repeat(64);
        let reservation = PreparedGuestJournal::reserve(
            guest_directory,
            &authority,
            &launcher_digest,
            expected_binding,
            host.factory(),
            host.destination(),
        )
        .unwrap();
        let launch = attached
            .begin_launch(
                reservation.store_identity().clone(),
                &launcher_digest,
                &launcher_digest,
                &"8".repeat(64),
            )
            .unwrap();
        retire(host, launch.close());
        drop(reservation);
        (outer, identity)
    }

    #[test]
    fn recovery_preserves_attach_key_and_launch_intent_is_one_way() {
        let host = shared_test_host();
        let outer = tempfile::tempdir().unwrap();
        let outer_directory = lillux::PinnedDirectory::open(outer.path())
            .unwrap()
            .unwrap();
        outer_directory.tighten_owner_private_directory().unwrap();
        let bootstrap = bootstrap();
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[13; 32]);
        let expected_binding = binding(&bootstrap, &supervisor);
        let journal = create(&host, outer_directory, bootstrap, capture(), supervisor).unwrap();
        let store_identity = journal.store_identity().clone();
        let exact_request = journal.attachment_request().canonical_bytes().unwrap();
        let exact_public_key =
            encode_channel_public_key(&journal.supervisor_signing_key().verifying_key()).unwrap();
        refusal(&host, recover(&host, &outer, &store_identity));
        retire(&host, journal.close());

        let ExternalSupervisorJournalRecovery::Prepared(recovered) =
            recover(&host, &outer, &store_identity).unwrap()
        else {
            panic!("fresh supervisor journal recovered at wrong stage")
        };
        assert_eq!(
            recovered.attachment_request().canonical_bytes().unwrap(),
            exact_request
        );
        assert_eq!(
            encode_channel_public_key(&recovered.supervisor_signing_key().verifying_key()).unwrap(),
            exact_public_key
        );
        let attached = recovered.record_binding(expected_binding.clone()).unwrap();
        retire(&host, attached.close());

        let ExternalSupervisorJournalRecovery::Attached(attached) =
            recover(&host, &outer, &store_identity).unwrap()
        else {
            panic!("attached supervisor journal recovered at wrong stage")
        };
        assert_eq!(attached.binding(), &expected_binding);

        let guest = tempfile::tempdir().unwrap();
        let guest_directory = lillux::PinnedDirectory::open(guest.path())
            .unwrap()
            .unwrap();
        guest_directory.tighten_owner_private_directory().unwrap();
        let (_state_root, authority) = state_authority();
        let launcher_digest = "7".repeat(64);
        let reservation = PreparedGuestJournal::reserve(
            guest_directory,
            &authority,
            &launcher_digest,
            expected_binding.clone(),
            host.factory(),
            host.destination(),
        )
        .unwrap();
        let anchored_guest = reservation.store_identity().clone();
        let launch = attached
            .begin_launch(
                anchored_guest.clone(),
                &launcher_digest,
                &launcher_digest,
                &"8".repeat(64),
            )
            .unwrap();
        assert_eq!(launch.binding(), &expected_binding);
        assert_eq!(launch.launch_intent().guest_store_identity, anchored_guest);
        retire(&host, launch.close());

        let ExternalSupervisorJournalRecovery::LaunchIntent(recovered) =
            recover(&host, &outer, &store_identity).unwrap()
        else {
            panic!("launch intent recovered with live transition authority")
        };
        assert_eq!(recovered.binding(), &expected_binding);
        assert_eq!(
            recovered.launch_intent().guest_store_identity,
            anchored_guest
        );
        recovered.validate().unwrap();
        retire(&host, recovered.close());
        let prepared_guest = reservation.initialize(&census(&host)).unwrap();
        assert_eq!(prepared_guest.store_identity(), &anchored_guest);
        retire(&host, prepared_guest.close());
    }

    #[test]
    fn substituted_outer_anchor_and_binding_are_refused() {
        let host = shared_test_host();
        let outer = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(outer.path())
            .unwrap()
            .unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let first_bootstrap = bootstrap();
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[13; 32]);
        let mut wrong_binding = binding(&first_bootstrap, &supervisor);
        let journal = create(&host, directory, first_bootstrap, capture(), supervisor).unwrap();
        wrong_binding.candidate_program_digest = "0".repeat(64);
        refusal(&host, journal.record_binding(wrong_binding));

        let other = tempfile::tempdir().unwrap();
        let other_directory = lillux::PinnedDirectory::open(other.path())
            .unwrap()
            .unwrap();
        other_directory.tighten_owner_private_directory().unwrap();
        let other_journal = create(
            &host,
            other_directory,
            bootstrap(),
            capture(),
            lillux::crypto::SigningKey::from_bytes(&[14; 32]),
        )
        .unwrap();
        let other_identity = other_journal.store_identity().clone();
        retire(&host, other_journal.close());
        refusal(&host, recover(&host, &outer, &other_identity));
    }

    #[test]
    fn relay_bootstrap_versions_do_not_change_the_journal_epoch() {
        assert_eq!(RELAY_SCHEMA_EPOCH, 7);
        assert_eq!(
            super::super::transport::EXTERNAL_SUPERVISOR_RELAY_BOOTSTRAP_SCHEMA,
            9
        );
        assert_eq!(
            super::super::transport::EXTERNAL_SUPERVISOR_ORDINARY_RELAY_BOOTSTRAP_SCHEMA,
            10
        );
        let relay_schema = schema_for(RELAY_SCHEMA_EPOCH).unwrap();
        assert!(relay_schema.contains("CHECK(schema_epoch=7)"));
        assert!(relay_schema.contains("attach_response_digest TEXT NOT NULL"));
        assert!(schema_for(8).is_err());
        assert!(schema_for(9).is_err());
    }

    #[test]
    fn exact_schema_inventory_is_required_on_reopen() {
        enum Mutation {
            MissingTrigger,
            ReplacedTrigger,
            ExtraTable,
        }

        let host = shared_test_host();
        for mutation in [
            Mutation::MissingTrigger,
            Mutation::ReplacedTrigger,
            Mutation::ExtraTable,
        ] {
            let (root, identity) = fresh_journal(&host);
            with_journal(&host, &root, |conn| {
                match mutation {
                    Mutation::MissingTrigger => {
                        conn.execute_batch("DROP TRIGGER external_supervisor_meta_no_delete")?
                    }
                    Mutation::ReplacedTrigger => conn.execute_batch(
                        "DROP TRIGGER external_supervisor_meta_no_delete;
                         CREATE TRIGGER external_supervisor_meta_no_delete
                         BEFORE DELETE ON external_supervisor_meta
                         BEGIN SELECT RAISE(ABORT, 'changed guard'); END;",
                    )?,
                    Mutation::ExtraTable => {
                        conn.execute_batch("CREATE TABLE unauthorized_extra(value TEXT)")?
                    }
                }
                Ok(())
            });
            assert!(
                recovery_error(&host, &root, &identity).contains("complete schema SQL mismatch"),
                "mutated exact schema was not diagnosed"
            );
        }
    }

    #[test]
    fn predecessor_schema_epoch_is_not_reopened_as_current_authority() {
        let host = shared_test_host();
        let (root, identity) = fresh_journal(&host);
        with_journal(&host, &root, |conn| {
            conn.pragma_update(None, "user_version", HTTP_SCHEMA_EPOCH - 1)?;
            Ok(())
        });
        assert!(
            recovery_error(&host, &root, &identity).contains("not the exact current schema"),
            "predecessor outer-supervisor authority was reinterpreted"
        );
    }

    #[test]
    fn same_directory_database_replacement_is_refused() {
        let host = shared_test_host();
        let (root, identity) = fresh_journal(&host);
        let database = root.path().join(DATABASE_NAME);
        let replacement = root.path().join("replacement");
        std::fs::copy(&database, &replacement).unwrap();
        std::fs::rename(&replacement, &database).unwrap();
        assert!(
            recovery_error(&host, &root, &identity).contains("outer inode identity"),
            "same-directory database replacement was not diagnosed"
        );
    }

    #[test]
    fn leftover_rollback_journal_is_refused() {
        let host = shared_test_host();
        let (root, identity) = fresh_journal(&host);
        std::fs::File::create(root.path().join(format!("{DATABASE_NAME}-journal")))
            .unwrap()
            .sync_all()
            .unwrap();
        assert!(
            recovery_error(&host, &root, &identity).contains("rollback journal"),
            "a foreign rollback journal was admitted"
        );
    }

    /// A commit acknowledged before a crash survives in the WAL alone: the
    /// crash image is the main file as it stood before the checkpoint plus
    /// the WAL holding the committed frames and a torn uncommitted tail, all
    /// written back into the same inodes. Native recovery replays the commit
    /// under the unchanged main identity and discards the tail.
    #[test]
    fn committed_wal_frames_survive_a_crash_before_checkpoint() {
        use std::io::Write as _;

        let main_identity = |path: &std::path::Path| {
            lillux::pinned_regular_file_identity(&std::fs::File::open(path).unwrap()).unwrap()
        };
        let host = shared_test_host();
        let outer = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(outer.path())
            .unwrap()
            .unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let bootstrap = bootstrap();
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[13; 32]);
        let expected_binding = binding(&bootstrap, &supervisor);
        let journal = create(&host, directory, bootstrap, capture(), supervisor).unwrap();
        let identity = journal.store_identity().clone();
        let database = outer.path().join(DATABASE_NAME);
        let wal = outer.path().join(format!("{DATABASE_NAME}-wal"));
        let main = main_identity(&database);
        assert_eq!(main, identity.database_identity);
        let attached = journal.record_binding(expected_binding.clone()).unwrap();
        // The crash image, taken while the attachment commit is acknowledged
        // but not yet checkpointed.
        let crashed_main = std::fs::read(&database).unwrap();
        let mut crashed_wal = std::fs::read(&wal).unwrap();
        retire(&host, attached.close());
        // Move every committed frame into the main file so the crash image
        // provably differs from the checkpointed state.
        with_journal(&host, &outer, |conn| {
            let (busy, _, _): (i64, i64, i64) =
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
            ensure!(busy == 0, "test checkpoint was busy");
            Ok(())
        });
        assert!(
            crashed_wal.len() > 32,
            "the acknowledged commit left no WAL frames"
        );
        assert_ne!(
            crashed_main,
            std::fs::read(&database).unwrap(),
            "the attachment commit was already checkpointed in the crash image"
        );
        // A torn frame past the last commit: never a valid commit frame.
        crashed_wal.extend_from_slice(&[0x5a; 100]);
        for (path, bytes) in [(&database, &crashed_main), (&wal, &crashed_wal)] {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .unwrap();
            file.write_all(bytes).unwrap();
            file.sync_all().unwrap();
        }
        assert!(
            lillux::matches_pinned_regular_file_identity(
                &std::fs::File::open(&database).unwrap(),
                main,
            )
            .unwrap()
        );
        let ExternalSupervisorJournalRecovery::Attached(recovered) =
            recover(&host, &outer, &identity).unwrap()
        else {
            panic!("the acknowledged attachment was lost by the crash")
        };
        assert_eq!(recovered.binding(), &expected_binding);
        assert!(
            lillux::matches_pinned_regular_file_identity(
                &std::fs::File::open(&database).unwrap(),
                main,
            )
            .unwrap()
        );
        retire(&host, recovered.close());
    }

    #[test]
    fn retained_bootstrap_request_and_key_are_revalidated_on_reopen() {
        enum Mutation {
            Bootstrap,
            Request,
            SigningKey,
        }

        let host = shared_test_host();
        for mutation in [Mutation::Bootstrap, Mutation::Request, Mutation::SigningKey] {
            let (root, identity) = fresh_journal(&host);
            let expected = match mutation {
                Mutation::Bootstrap => "bootstrap changed its canonical identity",
                Mutation::Request => "attachment request changed its exact bytes",
                Mutation::SigningKey => "signing key changed its public identity",
            };
            mutate_meta_without_guard(&host, &root, |conn| match mutation {
                Mutation::Bootstrap => {
                    let json: String = conn
                        .query_row(
                            "SELECT bootstrap_json FROM external_supervisor_meta WHERE singleton=1",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
                    value["occurrence_id"] = serde_json::Value::String("substituted".into());
                    conn.execute(
                        "UPDATE external_supervisor_meta SET bootstrap_json=?1 WHERE singleton=1",
                        [lillux::canonical_json(&value).unwrap()],
                    )
                    .unwrap();
                }
                Mutation::Request => {
                    let json: String = conn
                        .query_row(
                            "SELECT attachment_request_json FROM external_supervisor_meta WHERE singleton=1",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
                    value["bootstrap_capability"] =
                        serde_json::Value::String(STANDARD.encode([99_u8; 32]));
                    conn.execute(
                        "UPDATE external_supervisor_meta SET attachment_request_json=?1 WHERE singleton=1",
                        [lillux::canonical_json(&value).unwrap()],
                    )
                    .unwrap();
                }
                Mutation::SigningKey => {
                    conn.execute(
                        "UPDATE external_supervisor_meta
                         SET supervisor_signing_seed_base64=?1 WHERE singleton=1",
                        [STANDARD.encode([99_u8; 32])],
                    )
                    .unwrap();
                }
            });
            assert!(
                recovery_error(&host, &root, &identity).contains(expected),
                "mutated retained authority was not diagnosed as {expected}"
            );
        }
    }

    #[test]
    fn launch_guest_binding_and_bootstrap_substitution_are_refused() {
        let host = shared_test_host();
        for mutate_binding in [true, false] {
            let (root, identity) = launched_journal(&host);
            with_journal(&host, &root, |conn| {
                let trigger_sql: String = conn.query_row(
                    "SELECT sql FROM sqlite_schema
                     WHERE type='trigger' AND name='external_supervisor_launch_no_update'",
                    [],
                    |row| row.get(0),
                )?;
                let guest_json: String = conn.query_row(
                    "SELECT guest_store_identity_json
                     FROM external_supervisor_launch_intent WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?;
                let mut guest_value: serde_json::Value = serde_json::from_str(&guest_json)?;
                if mutate_binding {
                    guest_value["binding_digest"] = serde_json::Value::String("0".repeat(64));
                } else {
                    guest_value["bootstrap_digest"] = serde_json::Value::String("1".repeat(64));
                }
                let guest: GuestJournalStoreIdentity = serde_json::from_value(guest_value)?;
                let digest = guest.digest()?;
                let json = lillux::canonical_json(&serde_json::to_value(&guest)?)?;
                conn.execute_batch("DROP TRIGGER external_supervisor_launch_no_update")?;
                conn.execute(
                    "UPDATE external_supervisor_launch_intent
                     SET guest_store_identity_digest=?1,guest_store_identity_json=?2
                     WHERE singleton=1",
                    params![digest, json],
                )?;
                conn.execute_batch(&trigger_sql)?;
                Ok(())
            });
            assert!(
                recovery_error(&host, &root, &identity)
                    .contains("launch intent changed its authority"),
                "substituted guest launch authority was not diagnosed"
            );
        }
    }
}
