//! Protected outer lifecycle journal for one external candidate supervisor.
//!
//! This store exists outside the candidate and its guest journal. It persists
//! the exact secret bootstrap and supervisor signing key before attachment,
//! the returned channel binding before any candidate preparation, and the
//! independently anchored guest-store/launcher intent before native spawn.
//! Reopen may continue only the exact pre-launch transition already authorized
//! by a retained `prepared` or `attached` record. Once launch intent exists,
//! reopen is recovery-only and cannot mint another attachment or launch.

use std::ffi::{OsStr, OsString};
use std::fs::File;

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use zeroize::Zeroizing;

use super::guest_journal::GuestJournalStoreIdentity;
use super::transport::{ExternalChannelAttachRequest, ExternalSupervisorBootstrap};
use super::{ExecutionChannelBinding, encode_channel_public_key};

const DATABASE_NAME: &str = "external-supervisor.sqlite3";
const APPLICATION_ID: i32 = 0x5259_4553; // RYES
const SCHEMA_EPOCH: i64 = 3;

const SCHEMA: &str = r#"
CREATE TABLE external_supervisor_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    schema_epoch INTEGER NOT NULL CHECK(schema_epoch=3),
    journal_nonce TEXT NOT NULL UNIQUE,
    directory_identity_json TEXT NOT NULL,
    database_identity_json TEXT NOT NULL,
    bootstrap_digest TEXT NOT NULL UNIQUE,
    bootstrap_json TEXT NOT NULL,
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
    conn: Connection,
    directory: lillux::PinnedDirectory,
    _lifetime_lock: lillux::PinnedDirectoryLock,
    database_file: File,
    bootstrap: ExternalSupervisorBootstrap,
    supervisor_signing_key: lillux::crypto::SigningKey,
    attachment_request: ExternalChannelAttachRequest,
    binding: Option<ExecutionChannelBinding>,
    launch_intent: Option<ExternalSupervisorLaunchIntent>,
    store_identity: ExternalSupervisorJournalStoreIdentity,
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
            self.schema == 1,
            "unsupported supervisor journal identity schema"
        );
        hash(&self.journal_nonce, "supervisor journal nonce")?;
        hash(&self.bootstrap_digest, "supervisor bootstrap digest")?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }
}

impl PreparedExternalSupervisorJournal {
    pub fn create(
        directory: lillux::PinnedDirectory,
        bootstrap: ExternalSupervisorBootstrap,
        supervisor_signing_key: lillux::crypto::SigningKey,
    ) -> Result<Self> {
        bootstrap.validate()?;
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
        let database_file =
            directory.open_regular_create(OsStr::new(DATABASE_NAME), true, true, 0o600)?;
        directory.sync()?;
        let database_identity = lillux::pinned_regular_file_identity(&database_file)?;
        let store_identity = ExternalSupervisorJournalStoreIdentity {
            schema: 1,
            journal_nonce: lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>()),
            directory_identity: directory.identity()?,
            database_identity,
            bootstrap_digest: bootstrap_digest.clone(),
        };
        store_identity.digest()?;
        let conn = open_exact(&directory, &database_file)?;
        configure(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
        conn.pragma_update(None, "user_version", SCHEMA_EPOCH)?;
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO external_supervisor_meta VALUES(1,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'prepared')",
            params![
                SCHEMA_EPOCH,
                &store_identity.journal_nonce,
                lillux::canonical_json(&serde_json::to_value(store_identity.directory_identity)?)?,
                lillux::canonical_json(&serde_json::to_value(store_identity.database_identity)?)?,
                &bootstrap_digest,
                &bootstrap_json,
                signing_seed.as_str(),
                &supervisor_public_key,
                &attachment_request_digest,
                &attachment_request_json,
            ],
        )?;
        tx.commit()?;
        database_file.sync_all()?;
        directory.sync()?;
        let store = SupervisorStore {
            conn,
            directory,
            _lifetime_lock: lifetime_lock,
            database_file,
            bootstrap,
            supervisor_signing_key,
            attachment_request,
            binding: None,
            launch_intent: None,
            store_identity,
        };
        store.validate()?;
        Ok(Self(store))
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
        mut self,
        binding: ExecutionChannelBinding,
    ) -> Result<AttachedExternalSupervisorJournal> {
        self.0.bootstrap.validate_attached_binding(
            &binding,
            &self.0.attachment_request.supervisor_public_key,
        )?;
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let binding_digest = binding.digest()?;
        let binding_json = lillux::canonical_json(&serde_json::to_value(&binding)?)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO external_supervisor_binding VALUES(1,?1,?2)",
            params![&binding_digest, &binding_json],
        )?;
        let changed = tx.execute(
            "UPDATE external_supervisor_meta SET lifecycle='attached'
             WHERE singleton=1 AND lifecycle='prepared'",
            [],
        )?;
        ensure!(changed == 1, "external supervisor attachment is not fresh");
        tx.commit()?;
        self.0.database_file.sync_all()?;
        self.0.binding = Some(binding);
        self.0.validate()?;
        Ok(AttachedExternalSupervisorJournal(self.0))
    }
}

impl AttachedExternalSupervisorJournal {
    pub fn store_identity(&self) -> &ExternalSupervisorJournalStoreIdentity {
        &self.0.store_identity
    }

    pub fn bootstrap(&self) -> &ExternalSupervisorBootstrap {
        &self.0.bootstrap
    }

    pub fn binding(&self) -> &ExecutionChannelBinding {
        self.0.binding.as_ref().expect("attached stage has binding")
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
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
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
        self.0.database_file.sync_all()?;
        self.0.launch_intent = Some(intent);
        self.0.validate()?;
        Ok(LaunchIntentExternalSupervisorJournal(self.0))
    }
}

impl LaunchIntentExternalSupervisorJournal {
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
}

impl ExternalSupervisorJournalRecovery {
    pub fn open(
        directory: lillux::PinnedDirectory,
        store_identity: &ExternalSupervisorJournalStoreIdentity,
    ) -> Result<Self> {
        directory.require_owner_private_directory()?;
        ensure!(
            directory.identity()? == store_identity.directory_identity,
            "external supervisor directory changed its outer identity"
        );
        let lifetime_lock = directory
            .try_lock_exclusive()?
            .context("external supervisor directory still has a live owner")?;
        lifetime_lock.ensure_protects(&directory)?;
        for name in directory.entry_names()? {
            ensure!(
                name == OsStr::new(DATABASE_NAME) || name == journal_name(),
                "external supervisor store has an ambient entry: {}",
                directory.path().join(name).display()
            );
        }
        for name in [wal_name(), shm_name()] {
            ensure!(
                directory.open_entry(&name, false)?.is_none(),
                "external supervisor store has an unexpected sidecar"
            );
        }
        let _rollback = directory.open_regular(&journal_name(), false)?;
        let database_file = directory
            .open_regular(OsStr::new(DATABASE_NAME), true)?
            .context("external supervisor database is absent")?;
        ensure!(
            lillux::matches_pinned_regular_file_identity(
                &database_file,
                store_identity.database_identity,
            )?,
            "external supervisor database changed its outer inode identity"
        );
        let conn = open_exact(&directory, &database_file)?;
        configure(&conn)?;
        let (bootstrap, supervisor_signing_key, attachment_request, binding, launch_intent) =
            load_content(&conn)?;
        let store = SupervisorStore {
            conn,
            directory,
            _lifetime_lock: lifetime_lock,
            database_file,
            bootstrap,
            supervisor_signing_key,
            attachment_request,
            binding,
            launch_intent,
            store_identity: store_identity.clone(),
        };
        store.validate()?;
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
}

impl SupervisorStore {
    fn validate(&self) -> Result<()> {
        ensure_same_file(&self.directory, &self.database_file)?;
        self.store_identity.digest()?;
        validate_store_identity_row(&self.conn, &self.store_identity)?;
        ensure!(
            self.store_identity.directory_identity == self.directory.identity()?
                && lillux::matches_pinned_regular_file_identity(
                    &self.database_file,
                    self.store_identity.database_identity,
                )?
                && self.store_identity.bootstrap_digest == self.bootstrap.digest()?,
            "external supervisor store changed its retained identity"
        );
        let application_id: i64 = self
            .conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let user_version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let quick_check: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        ensure!(
            application_id == i64::from(APPLICATION_ID)
                && user_version == SCHEMA_EPOCH
                && quick_check == "ok",
            "external supervisor database is not the exact current schema"
        );
        crate::sqlite_schema::assert_complete_schema_sql(
            &self.conn,
            SCHEMA,
            &self.directory.path().join(DATABASE_NAME),
        )?;
        let (loaded_bootstrap, loaded_key, loaded_request, loaded_binding, loaded_launch) =
            load_content(&self.conn)?;
        ensure!(
            loaded_bootstrap.canonical_bytes()? == self.bootstrap.canonical_bytes()?
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

fn validate_store_identity_row(
    conn: &Connection,
    expected: &ExternalSupervisorJournalStoreIdentity,
) -> Result<()> {
    let (journal_nonce, directory_json, database_json, bootstrap_digest): (
        String,
        String,
        String,
        String,
    ) = conn.query_row(
        "SELECT journal_nonce,directory_identity_json,database_identity_json,bootstrap_digest
         FROM external_supervisor_meta WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
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
            && bootstrap_digest == expected.bootstrap_digest,
        "external supervisor database changed its outer anchor"
    );
    Ok(())
}

fn load_content(
    conn: &Connection,
) -> Result<(
    ExternalSupervisorBootstrap,
    lillux::crypto::SigningKey,
    ExternalChannelAttachRequest,
    Option<ExecutionChannelBinding>,
    Option<ExternalSupervisorLaunchIntent>,
)> {
    let (
        journal_nonce,
        bootstrap_digest,
        bootstrap_json,
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
    ) = conn.query_row(
        "SELECT journal_nonce,bootstrap_digest,bootstrap_json,
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
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
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
    let binding = conn
        .query_row(
            "SELECT binding_digest,binding_json FROM external_supervisor_binding WHERE singleton=1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(digest, json)| -> Result<_> {
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
        supervisor_signing_key,
        attachment_request,
        binding,
        launch_intent,
    ))
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

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let mode: String = conn.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
    ensure!(
        mode == "delete",
        "external supervisor journal mode is not rollback-delete"
    );
    Ok(())
}

fn open_exact(directory: &lillux::PinnedDirectory, expected: &File) -> Result<Connection> {
    let path = directory.descriptor_child_path(OsStr::new(DATABASE_NAME))?;
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "open protected external supervisor database {}",
            path.display()
        )
    })?;
    ensure_same_file(directory, expected)?;
    Ok(conn)
}

fn ensure_same_file(directory: &lillux::PinnedDirectory, expected: &File) -> Result<()> {
    let current = directory
        .open_regular(OsStr::new(DATABASE_NAME), false)?
        .context("external supervisor database disappeared")?;
    ensure!(
        lillux::same_open_file_identity(expected, &current)?,
        "external supervisor database identity changed"
    );
    Ok(())
}

fn wal_name() -> OsString {
    OsString::from(format!("{DATABASE_NAME}-wal"))
}

fn shm_name() -> OsString {
    OsString::from(format!("{DATABASE_NAME}-shm"))
}

fn journal_name() -> OsString {
    OsString::from(format!("{DATABASE_NAME}-journal"))
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
    use crate::external_execution::transport::{
        EXTERNAL_CHANNEL_ROUTE_CONTRACT, ExternalControllerTransportContract,
        external_tls_root_bundle_digest,
    };

    fn bootstrap() -> ExternalSupervisorBootstrap {
        let recipe = ExternalCandidateRuntimeRecipe {
            schema: 1,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: true,
            nested_sandbox: true,
        };
        let recipe_digest = recipe.digest().unwrap();
        let roots = vec![STANDARD.encode(b"fixture root")];
        ExternalSupervisorBootstrap {
            schema: 4,
            controller: ExternalControllerTransportContract {
                schema: 1,
                https_origin: "https://controller.example".into(),
                route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
                tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
                connect_timeout_ms: 1_000,
                request_timeout_ms: 2_000,
                maximum_response_bytes: 64 * 1024,
            },
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
                requirement: ExternalCandidateRequirement {
                    schema: 2,
                    protocol: PROTOCOL.into(),
                    runtime_product_declaration_id: "runtime".into(),
                    runtime_recipe: recipe,
                },
                runtime_manifest_hash: "e".repeat(64),
                runtime_witness_hash: "1".repeat(64),
                qualification_attestation_hash: "2".repeat(64),
                selection_identity_digest: "3".repeat(64),
                runtime_recipe_digest: recipe_digest,
            },
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
        }
    }

    fn binding(
        bootstrap: &ExternalSupervisorBootstrap,
        supervisor: &lillux::crypto::SigningKey,
    ) -> ExecutionChannelBinding {
        let issued_at_ms = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        ExecutionChannelBinding {
            schema: 3,
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
        let db = crate::StateDb::open(root.path(), std::sync::Arc::new(crate::TrustStore::new()))
            .unwrap();
        let authority = db.pinned_authority().unwrap();
        drop(db);
        (root, authority)
    }

    fn recovery_error(
        root: &tempfile::TempDir,
        identity: &ExternalSupervisorJournalStoreIdentity,
    ) -> String {
        match ExternalSupervisorJournalRecovery::open(
            lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
            identity,
        ) {
            Ok(_) => panic!("mutated supervisor journal reopened"),
            Err(error) => format!("{error:#}"),
        }
    }

    fn mutate_meta_without_guard(root: &tempfile::TempDir, mutation: impl FnOnce(&Connection)) {
        let conn = Connection::open(root.path().join(DATABASE_NAME)).unwrap();
        let trigger_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_schema
                 WHERE type='trigger' AND name='external_supervisor_meta_transition'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute_batch("DROP TRIGGER external_supervisor_meta_transition")
            .unwrap();
        mutation(&conn);
        conn.execute_batch(&trigger_sql).unwrap();
        drop(conn);
    }

    fn fresh_journal() -> (tempfile::TempDir, ExternalSupervisorJournalStoreIdentity) {
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let journal = PreparedExternalSupervisorJournal::create(
            directory,
            bootstrap(),
            lillux::crypto::SigningKey::from_bytes(&[13; 32]),
        )
        .unwrap();
        let identity = journal.store_identity().clone();
        drop(journal);
        (root, identity)
    }

    fn launched_journal() -> (tempfile::TempDir, ExternalSupervisorJournalStoreIdentity) {
        let outer = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(outer.path())
            .unwrap()
            .unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let bootstrap = bootstrap();
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[13; 32]);
        let expected_binding = binding(&bootstrap, &supervisor);
        let journal =
            PreparedExternalSupervisorJournal::create(directory, bootstrap, supervisor).unwrap();
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
        drop(launch);
        drop(reservation);
        (outer, identity)
    }

    #[test]
    fn recovery_preserves_attach_key_and_launch_intent_is_one_way() {
        let outer = tempfile::tempdir().unwrap();
        let outer_directory = lillux::PinnedDirectory::open(outer.path())
            .unwrap()
            .unwrap();
        outer_directory.tighten_owner_private_directory().unwrap();
        let bootstrap = bootstrap();
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[13; 32]);
        let expected_binding = binding(&bootstrap, &supervisor);
        let journal =
            PreparedExternalSupervisorJournal::create(outer_directory, bootstrap, supervisor)
                .unwrap();
        let store_identity = journal.store_identity().clone();
        let exact_request = journal.attachment_request().canonical_bytes().unwrap();
        let exact_public_key =
            encode_channel_public_key(&journal.supervisor_signing_key().verifying_key()).unwrap();
        assert!(
            ExternalSupervisorJournalRecovery::open(
                lillux::PinnedDirectory::open(outer.path())
                    .unwrap()
                    .unwrap(),
                &store_identity,
            )
            .is_err()
        );
        drop(journal);

        let ExternalSupervisorJournalRecovery::Prepared(recovered) =
            ExternalSupervisorJournalRecovery::open(
                lillux::PinnedDirectory::open(outer.path())
                    .unwrap()
                    .unwrap(),
                &store_identity,
            )
            .unwrap()
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
        drop(attached);

        let ExternalSupervisorJournalRecovery::Attached(attached) =
            ExternalSupervisorJournalRecovery::open(
                lillux::PinnedDirectory::open(outer.path())
                    .unwrap()
                    .unwrap(),
                &store_identity,
            )
            .unwrap()
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
        drop(launch);

        let ExternalSupervisorJournalRecovery::LaunchIntent(recovered) =
            ExternalSupervisorJournalRecovery::open(
                lillux::PinnedDirectory::open(outer.path())
                    .unwrap()
                    .unwrap(),
                &store_identity,
            )
            .unwrap()
        else {
            panic!("launch intent recovered with live transition authority")
        };
        assert_eq!(recovered.binding(), &expected_binding);
        assert_eq!(
            recovered.launch_intent().guest_store_identity,
            anchored_guest
        );
        recovered.validate().unwrap();
        let prepared_guest = reservation.initialize().unwrap();
        assert_eq!(prepared_guest.store_identity(), &anchored_guest);
    }

    #[test]
    fn substituted_outer_anchor_and_binding_are_refused() {
        let outer = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(outer.path())
            .unwrap()
            .unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let first_bootstrap = bootstrap();
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[13; 32]);
        let mut wrong_binding = binding(&first_bootstrap, &supervisor);
        let journal =
            PreparedExternalSupervisorJournal::create(directory, first_bootstrap, supervisor)
                .unwrap();
        wrong_binding.candidate_program_digest = "0".repeat(64);
        assert!(journal.record_binding(wrong_binding).is_err());

        let other = tempfile::tempdir().unwrap();
        let other_directory = lillux::PinnedDirectory::open(other.path())
            .unwrap()
            .unwrap();
        other_directory.tighten_owner_private_directory().unwrap();
        let other_journal = PreparedExternalSupervisorJournal::create(
            other_directory,
            bootstrap(),
            lillux::crypto::SigningKey::from_bytes(&[14; 32]),
        )
        .unwrap();
        let other_identity = other_journal.store_identity().clone();
        drop(other_journal);
        assert!(
            ExternalSupervisorJournalRecovery::open(
                lillux::PinnedDirectory::open(outer.path())
                    .unwrap()
                    .unwrap(),
                &other_identity,
            )
            .is_err()
        );
    }

    #[test]
    fn exact_schema_inventory_is_required_on_reopen() {
        enum Mutation {
            MissingTrigger,
            ReplacedTrigger,
            ExtraTable,
        }

        for mutation in [
            Mutation::MissingTrigger,
            Mutation::ReplacedTrigger,
            Mutation::ExtraTable,
        ] {
            let (root, identity) = fresh_journal();
            let conn = Connection::open(root.path().join(DATABASE_NAME)).unwrap();
            match mutation {
                Mutation::MissingTrigger => conn
                    .execute_batch("DROP TRIGGER external_supervisor_meta_no_delete")
                    .unwrap(),
                Mutation::ReplacedTrigger => conn
                    .execute_batch(
                        "DROP TRIGGER external_supervisor_meta_no_delete;
                         CREATE TRIGGER external_supervisor_meta_no_delete
                         BEFORE DELETE ON external_supervisor_meta
                         BEGIN SELECT RAISE(ABORT, 'changed guard'); END;",
                    )
                    .unwrap(),
                Mutation::ExtraTable => conn
                    .execute_batch("CREATE TABLE unauthorized_extra(value TEXT)")
                    .unwrap(),
            }
            drop(conn);
            assert!(
                recovery_error(&root, &identity).contains("complete schema SQL mismatch"),
                "mutated exact schema was not diagnosed"
            );
        }
    }

    #[test]
    fn same_directory_database_replacement_is_refused() {
        let (root, identity) = fresh_journal();
        let database = root.path().join(DATABASE_NAME);
        let replacement = root.path().join("replacement");
        std::fs::copy(&database, &replacement).unwrap();
        std::fs::rename(&replacement, &database).unwrap();
        assert!(
            recovery_error(&root, &identity).contains("outer inode identity"),
            "same-directory database replacement was not diagnosed"
        );
    }

    #[test]
    fn retained_bootstrap_request_and_key_are_revalidated_on_reopen() {
        enum Mutation {
            Bootstrap,
            Request,
            SigningKey,
        }

        for mutation in [Mutation::Bootstrap, Mutation::Request, Mutation::SigningKey] {
            let (root, identity) = fresh_journal();
            let expected = match mutation {
                Mutation::Bootstrap => "bootstrap changed its canonical identity",
                Mutation::Request => "attachment request changed its exact bytes",
                Mutation::SigningKey => "signing key changed its public identity",
            };
            mutate_meta_without_guard(&root, |conn| match mutation {
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
                recovery_error(&root, &identity).contains(expected),
                "mutated retained authority was not diagnosed as {expected}"
            );
        }
    }

    #[test]
    fn launch_guest_binding_and_bootstrap_substitution_are_refused() {
        for mutate_binding in [true, false] {
            let (root, identity) = launched_journal();
            let conn = Connection::open(root.path().join(DATABASE_NAME)).unwrap();
            let trigger_sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_schema
                     WHERE type='trigger' AND name='external_supervisor_launch_no_update'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let guest_json: String = conn
                .query_row(
                    "SELECT guest_store_identity_json
                     FROM external_supervisor_launch_intent WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let mut guest_value: serde_json::Value = serde_json::from_str(&guest_json).unwrap();
            if mutate_binding {
                guest_value["binding_digest"] = serde_json::Value::String("0".repeat(64));
            } else {
                guest_value["bootstrap_digest"] = serde_json::Value::String("1".repeat(64));
            }
            let guest: GuestJournalStoreIdentity = serde_json::from_value(guest_value).unwrap();
            let digest = guest.digest().unwrap();
            let json = lillux::canonical_json(&serde_json::to_value(&guest).unwrap()).unwrap();
            conn.execute_batch("DROP TRIGGER external_supervisor_launch_no_update")
                .unwrap();
            conn.execute(
                "UPDATE external_supervisor_launch_intent
                 SET guest_store_identity_digest=?1,guest_store_identity_json=?2
                 WHERE singleton=1",
                params![digest, json],
            )
            .unwrap();
            conn.execute_batch(&trigger_sql).unwrap();
            drop(conn);
            assert!(
                recovery_error(&root, &identity).contains("launch intent changed its authority"),
                "substituted guest launch authority was not diagnosed"
            );
        }
    }
}
