//! Operator-side vault commands — `ryeos vault {put,list,remove,rewrap}`.
//!
//! These commands operate directly on the daemon's on-disk vault key
//! (`<state>/.ai/node/vault/private_key.pem`) and the sealed
//! secret-store (`<state>/.ai/state/secrets/store.enc`). They run
//! locally without the daemon so rotation works even when the daemon
//! is down — that's why this module sits in `ryeos-core-tools` and is
//! invoked through `crates/bin/cli/src/lifecycle_commands.rs`.
//!
//! The library functions (policy, paths, sealed I/O, dotenv) are now
//! owned by the `ryeos-vault` crate. This module re-exports them and
//! adds the CLI command structs + `run_*()` implementations.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub use ryeos_vault::dotenv::read_dotenv_overlay;
pub use ryeos_vault::paths::{
    default_sealed_store_path, default_vault_public_key_path, default_vault_secret_key_path,
};
use ryeos_vault::policy::MAX_VAULT_ENVELOPE_BYTES;
pub use ryeos_vault::policy::{
    validate_decrypted_keys, validate_key_name, validate_secret_value, BLOCKED_NAMES,
};
pub use ryeos_vault::sealed::{
    cleanup_staged_rewrap_files, prepare_rewrap, read_bounded_file, read_sealed_secrets,
    recover_rewrap, with_store_lock, write_sealed_secrets, MAX_VAULT_KEY_FILE_BYTES,
};

// ── Command options + reports ────────────────────────────────────────

#[derive(Debug)]
pub struct PutOptions {
    pub app_root: PathBuf,
    pub entries: Vec<(String, String)>,
}

#[derive(Debug, serde::Serialize)]
pub struct PutReport {
    pub store_path: PathBuf,
    pub keys_written: Vec<String>,
    pub total_keys_after_put: usize,
}

#[derive(Debug)]
pub struct ListOptions {
    pub app_root: PathBuf,
}

#[derive(Debug, serde::Serialize)]
pub struct ListReport {
    pub store_path: PathBuf,
    pub keys: Vec<String>,
}

#[derive(Debug)]
pub struct RemoveOptions {
    pub app_root: PathBuf,
    pub keys: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct RemoveReport {
    pub store_path: PathBuf,
    pub removed: Vec<String>,
    pub not_present: Vec<String>,
    pub total_keys_after_remove: usize,
}

#[derive(Debug)]
pub struct RewrapOptions {
    pub app_root: PathBuf,
}

#[derive(Debug, serde::Serialize)]
pub struct RewrapReport {
    pub store_path: PathBuf,
    pub old_fingerprint: String,
    pub new_fingerprint: String,
    pub keys_rewrapped: usize,
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RewrapOutcome {
    CommittedDurable {
        report: RewrapReport,
        #[serde(skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    RestoredPrevious {
        report: RewrapReport,
        reason: String,
    },
    CommitDurabilityUncertain {
        report: RewrapReport,
        reason: String,
    },
}

// ── Command implementations ──────────────────────────────────────────

pub fn run_put(opts: &PutOptions) -> Result<PutReport> {
    if opts.entries.is_empty() {
        bail!("ryeos vault put: at least one entry required");
    }
    for (k, v) in &opts.entries {
        validate_key_name(k)?;
        validate_secret_value(v)?;
        if v.is_empty() {
            bail!("refusing to store empty value for vault key '{k}'");
        }
    }
    let key_path = default_vault_secret_key_path(&opts.app_root);
    let store_path = default_sealed_store_path(&opts.app_root);
    let public_path = default_vault_public_key_path(&opts.app_root);

    let (keys_written, total) = with_store_lock(&store_path, || {
        recover_rewrap(&key_path, &public_path, &store_path)?;
        let sk = lillux::vault::read_secret_key(&key_path).with_context(|| {
            format!(
                "read vault secret key {} — has `ryeos init` (or daemon) ever run \
                 on this state dir?",
                key_path.display()
            )
        })?;
        let pk = sk.public_key();
        let mut current = read_sealed_secrets(&store_path, &sk)?;
        let mut keys_written = Vec::with_capacity(opts.entries.len());
        for (k, v) in &opts.entries {
            keys_written.push(k.clone());
            current.insert(k.clone(), v.clone());
        }
        write_sealed_secrets(&store_path, &pk, &current)?;
        Ok((keys_written, current.len()))
    })?;
    Ok(PutReport {
        store_path,
        keys_written,
        total_keys_after_put: total,
    })
}

pub fn run_list(opts: &ListOptions) -> Result<ListReport> {
    let key_path = default_vault_secret_key_path(&opts.app_root);
    let store_path = default_sealed_store_path(&opts.app_root);
    let public_path = default_vault_public_key_path(&opts.app_root);
    let current = with_store_lock(&store_path, || {
        recover_rewrap(&key_path, &public_path, &store_path)?;
        let sk = lillux::vault::read_secret_key(&key_path)
            .with_context(|| format!("read vault secret key {}", key_path.display()))?;
        read_sealed_secrets(&store_path, &sk)
    })?;
    let mut keys: Vec<String> = current.keys().cloned().collect();
    keys.sort();
    Ok(ListReport { store_path, keys })
}

pub fn run_remove(opts: &RemoveOptions) -> Result<RemoveReport> {
    if opts.keys.is_empty() {
        bail!("ryeos vault remove: at least one KEY required");
    }
    let key_path = default_vault_secret_key_path(&opts.app_root);
    let store_path = default_sealed_store_path(&opts.app_root);
    let public_path = default_vault_public_key_path(&opts.app_root);

    let (removed, not_present, total) = with_store_lock(&store_path, || {
        recover_rewrap(&key_path, &public_path, &store_path)?;
        let sk = lillux::vault::read_secret_key(&key_path)
            .with_context(|| format!("read vault secret key {}", key_path.display()))?;
        let pk = sk.public_key();
        let mut current = read_sealed_secrets(&store_path, &sk)?;
        let mut removed = Vec::new();
        let mut not_present = Vec::new();
        for k in &opts.keys {
            if current.remove(k).is_some() {
                removed.push(k.clone());
            } else {
                not_present.push(k.clone());
            }
        }
        write_sealed_secrets(&store_path, &pk, &current)?;
        Ok((removed, not_present, current.len()))
    })?;
    Ok(RemoveReport {
        store_path,
        removed,
        not_present,
        total_keys_after_remove: total,
    })
}

pub fn run_rewrap(opts: &RewrapOptions) -> Result<RewrapOutcome> {
    let key_path = default_vault_secret_key_path(&opts.app_root);
    let pub_path = default_vault_public_key_path(&opts.app_root);
    let store_path = default_sealed_store_path(&opts.app_root);

    with_store_lock(&store_path, || {
        lillux::with_exclusive_file_lock(&key_path, || {
            run_rewrap_locked(key_path.clone(), pub_path, store_path.clone())
        })
    })
}

fn run_rewrap_locked(
    key_path: PathBuf,
    pub_path: PathBuf,
    store_path: PathBuf,
) -> Result<RewrapOutcome> {
    recover_rewrap(&key_path, &pub_path, &store_path)?;

    let old_sk = lillux::vault::read_secret_key(&key_path)
        .with_context(|| format!("read vault secret key {}", key_path.display()))?;
    let old_fingerprint = old_sk.public_key().fingerprint();

    let plaintext = read_sealed_secrets(&store_path, &old_sk)?;

    let new_sk = lillux::vault::VaultSecretKey::generate();
    let new_pk = new_sk.public_key();
    let new_fingerprint = new_pk.fingerprint();

    let new_key_path = key_path.with_extension("pem.new");
    let new_pub_path = pub_path.with_extension("pem.new");
    let new_store_path = store_path.with_extension("enc.new");

    let rewrap_store = store_path.exists();

    let write_result = (|| -> Result<()> {
        lillux::vault::write_secret_key(&new_key_path, &new_sk)
            .with_context(|| format!("write {}", new_key_path.display()))?;
        lillux::vault::write_public_key(&new_pub_path, &new_pk)
            .with_context(|| format!("write {}", new_pub_path.display()))?;
        if rewrap_store {
            write_sealed_secrets(&new_store_path, &new_pk, &plaintext)?;
        }
        Ok(())
    })();

    if let Err(e) = write_result {
        cleanup_staged_rewrap_files(&key_path, &pub_path, &store_path)
            .context("clean incomplete vault rewrap staging")?;
        return Err(e);
    }

    prepare_rewrap(&key_path, &pub_path, &store_path)?;

    let report = RewrapReport {
        store_path: store_path.clone(),
        old_fingerprint: old_fingerprint.clone(),
        new_fingerprint: new_fingerprint.clone(),
        keys_rewrapped: plaintext.len(),
    };

    let activation = (|| -> std::result::Result<(), lillux::AtomicMutationError> {
        if rewrap_store {
            let bytes = read_bounded_file(
                &new_store_path,
                MAX_VAULT_ENVELOPE_BYTES,
                "staged sealed envelope",
            )
            .map_err(|error| lillux::AtomicMutationError::BeforeCommit(error.into()))?;
            lillux::atomic_write_private(&store_path, &bytes)?;
        }
        let public_bytes = read_bounded_file(
            &new_pub_path,
            MAX_VAULT_KEY_FILE_BYTES,
            "staged vault public key",
        )
        .map_err(|error| lillux::AtomicMutationError::BeforeCommit(error.into()))?;
        lillux::atomic_write(&pub_path, &public_bytes)?;
        // Secret key last: once this changes, the current generation is complete.
        let secret_bytes = read_bounded_file(
            &new_key_path,
            MAX_VAULT_KEY_FILE_BYTES,
            "staged vault secret key",
        )
        .map_err(|error| lillux::AtomicMutationError::BeforeCommit(error.into()))?;
        lillux::atomic_write_private(&key_path, &secret_bytes)?;
        Ok(())
    })();
    if let Err(error) = activation {
        return Ok(handle_activation_failure(
            &key_path,
            &pub_path,
            &store_path,
            report,
            error,
        ));
    }
    let finalization_warning = recover_rewrap(&key_path, &pub_path, &store_path)
        .err()
        .map(|error| format!("rotation committed; deferred cleanup/recovery: {error:#}"));

    tracing::info!(
        old_fingerprint = %old_fingerprint,
        new_fingerprint = %new_fingerprint,
        keys_rewrapped = plaintext.len(),
        store_path = %store_path.display(),
        "vault: rewrap complete — keypair rotated and store re-sealed"
    );

    Ok(RewrapOutcome::CommittedDurable {
        report,
        warning: finalization_warning,
    })
}

fn handle_activation_failure(
    key_path: &Path,
    public_path: &Path,
    store_path: &Path,
    report: RewrapReport,
    error: lillux::AtomicMutationError,
) -> RewrapOutcome {
    match error {
        lillux::AtomicMutationError::BeforeCommit(error) => {
            match recover_rewrap(key_path, public_path, store_path) {
                Ok(()) => RewrapOutcome::RestoredPrevious {
                    report,
                    reason: format!("{error:#}"),
                },
                Err(recovery_error) => RewrapOutcome::CommitDurabilityUncertain {
                    report,
                    reason: format!(
                        "activation failed before its current commit point: {error:#}; durable recovery also failed: {recovery_error:#}"
                    ),
                },
            }
        }
        lillux::AtomicMutationError::DurabilityUncertain(error) => {
            // Do not reconcile here: the durable journal and backups are the
            // evidence the next locked operation needs to select a generation.
            RewrapOutcome::CommitDurabilityUncertain {
                report,
                reason: format!("{error:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_committed(outcome: RewrapOutcome) -> RewrapReport {
        match outcome {
            RewrapOutcome::CommittedDurable { report, .. } => report,
            other => panic!("expected durable rewrap commit, got {other:?}"),
        }
    }

    fn test_rewrap_report(state: &Path, old_fingerprint: String) -> RewrapReport {
        RewrapReport {
            store_path: default_sealed_store_path(state),
            old_fingerprint,
            new_fingerprint: "new-fingerprint".to_string(),
            keys_rewrapped: 0,
        }
    }

    #[test]
    fn uncertain_activation_preserves_recovery_journal() {
        let (state, old_key) = fresh_state_with_keypair();
        let key_path = default_vault_secret_key_path(state.path());
        let public_path = default_vault_public_key_path(state.path());
        let store_path = default_sealed_store_path(state.path());
        prepare_rewrap(&key_path, &public_path, &store_path).unwrap();
        let journal = store_path.with_extension("rewrap-journal.toml");

        let outcome = handle_activation_failure(
            &key_path,
            &public_path,
            &store_path,
            test_rewrap_report(state.path(), old_key.public_key().fingerprint()),
            lillux::AtomicMutationError::DurabilityUncertain(anyhow::anyhow!(
                "injected sync failure"
            )),
        );

        assert!(matches!(
            outcome,
            RewrapOutcome::CommitDurabilityUncertain { .. }
        ));
        assert!(journal.exists());
    }

    #[test]
    fn before_commit_failure_durably_restores_previous_generation() {
        let (state, old_key) = fresh_state_with_keypair();
        let key_path = default_vault_secret_key_path(state.path());
        let public_path = default_vault_public_key_path(state.path());
        let store_path = default_sealed_store_path(state.path());
        prepare_rewrap(&key_path, &public_path, &store_path).unwrap();
        let journal = store_path.with_extension("rewrap-journal.toml");

        let outcome = handle_activation_failure(
            &key_path,
            &public_path,
            &store_path,
            test_rewrap_report(state.path(), old_key.public_key().fingerprint()),
            lillux::AtomicMutationError::BeforeCommit(anyhow::anyhow!(
                "injected pre-rename failure"
            )),
        );

        assert!(matches!(outcome, RewrapOutcome::RestoredPrevious { .. }));
        assert!(!journal.exists());
        let restored = lillux::vault::read_secret_key(&key_path).unwrap();
        assert_eq!(
            restored.public_key().fingerprint(),
            old_key.public_key().fingerprint()
        );
    }

    fn fresh_state_with_keypair() -> (tempfile::TempDir, lillux::vault::VaultSecretKey) {
        let tmp = tempfile::tempdir().unwrap();
        let sk = lillux::vault::VaultSecretKey::generate();
        let key_path = default_vault_secret_key_path(tmp.path());
        let pub_path = default_vault_public_key_path(tmp.path());
        lillux::vault::write_secret_key(&key_path, &sk).unwrap();
        lillux::vault::write_public_key(&pub_path, &sk.public_key()).unwrap();
        (tmp, sk)
    }

    #[test]
    fn put_creates_store_and_writes_keys() {
        let (state, _sk) = fresh_state_with_keypair();
        let report = run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![
                ("OPENAI_API_KEY".into(), "sk-1".into()),
                ("DATABASE_URL".into(), "postgres://h/db".into()),
            ],
        })
        .unwrap();
        assert_eq!(report.total_keys_after_put, 2);
        assert!(report.store_path.exists());

        let listed = run_list(&ListOptions {
            app_root: state.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(listed.keys, vec!["DATABASE_URL", "OPENAI_API_KEY"]);
    }

    #[test]
    fn put_merges_with_existing() {
        let (state, _sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("A".into(), "1".into())],
        })
        .unwrap();
        let report = run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("B".into(), "2".into())],
        })
        .unwrap();
        assert_eq!(report.total_keys_after_put, 2);
    }

    #[test]
    fn put_overwrites_existing_key() {
        let (state, _sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("FOO".into(), "old".into())],
        })
        .unwrap();
        run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("FOO".into(), "new".into())],
        })
        .unwrap();

        let key_path = default_vault_secret_key_path(state.path());
        let sk = lillux::vault::read_secret_key(&key_path).unwrap();
        let store_path = default_sealed_store_path(state.path());
        let map = read_sealed_secrets(&store_path, &sk).unwrap();
        assert_eq!(map.get("FOO").unwrap(), "new");
    }

    #[test]
    fn put_rejects_blocked_key() {
        let (state, _sk) = fresh_state_with_keypair();
        let err = run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("PATH".into(), "/evil".into())],
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("PATH"));
    }

    #[test]
    fn put_rejects_invalid_key_chars() {
        let (state, _sk) = fresh_state_with_keypair();
        let err = run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("FOO-BAR".into(), "baz".into())],
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("invalid key name"));
    }

    #[test]
    fn put_requires_at_least_one_entry() {
        let (state, _sk) = fresh_state_with_keypair();
        let err = run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![],
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("at least one"));
    }

    #[test]
    fn list_on_missing_store_returns_empty() {
        let (state, _sk) = fresh_state_with_keypair();
        let report = run_list(&ListOptions {
            app_root: state.path().to_path_buf(),
        })
        .unwrap();
        assert!(report.keys.is_empty());
    }

    #[test]
    fn remove_drops_keys_idempotently() {
        let (state, _sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
        })
        .unwrap();

        let report = run_remove(&RemoveOptions {
            app_root: state.path().to_path_buf(),
            keys: vec!["A".into(), "C".into()],
        })
        .unwrap();
        assert_eq!(report.removed, vec!["A".to_string()]);
        assert_eq!(report.not_present, vec!["C".to_string()]);
        assert_eq!(report.total_keys_after_remove, 1);

        let listed = run_list(&ListOptions {
            app_root: state.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(listed.keys, vec!["B"]);
    }

    #[test]
    fn rewrap_rotates_keypair_and_re_seals_store() {
        let (state, old_sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("FOO".into(), "bar".into()), ("BAZ".into(), "qux".into())],
        })
        .unwrap();
        let old_fingerprint = old_sk.public_key().fingerprint();

        let report = expect_committed(
            run_rewrap(&RewrapOptions {
                app_root: state.path().to_path_buf(),
            })
            .unwrap(),
        );
        assert_eq!(report.old_fingerprint, old_fingerprint);
        assert_ne!(report.old_fingerprint, report.new_fingerprint);
        assert_eq!(report.keys_rewrapped, 2);

        let store_path = default_sealed_store_path(state.path());
        let err = read_sealed_secrets(&store_path, &old_sk).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fingerprint") || msg.contains("AEAD"),
            "expected fingerprint/AEAD failure with old key, got: {msg}"
        );

        let key_path = default_vault_secret_key_path(state.path());
        let pub_path = default_vault_public_key_path(state.path());
        let new_sk = lillux::vault::read_secret_key(&key_path).unwrap();
        let map = read_sealed_secrets(&store_path, &new_sk).unwrap();
        assert_eq!(map.get("FOO").unwrap(), "bar");
        assert_eq!(map.get("BAZ").unwrap(), "qux");

        assert!(key_path.exists(), "private_key.pem missing");
        assert!(pub_path.exists(), "public_key.pem missing");
        assert!(store_path.exists(), "store.enc missing");
        assert!(
            !key_path.with_extension("pem.new").exists(),
            "stale private_key.pem.new"
        );
        assert!(
            !pub_path.with_extension("pem.new").exists(),
            "stale public_key.pem.new"
        );
        assert!(
            !store_path.with_extension("enc.new").exists(),
            "stale store.enc.new"
        );

        assert_ne!(new_sk.public_key().fingerprint(), old_fingerprint);
    }

    #[test]
    fn rewrap_with_empty_store_only_rotates_keys() {
        let (state, old_sk) = fresh_state_with_keypair();
        let report = expect_committed(
            run_rewrap(&RewrapOptions {
                app_root: state.path().to_path_buf(),
            })
            .unwrap(),
        );
        assert_eq!(report.keys_rewrapped, 0);
        assert_ne!(report.new_fingerprint, old_sk.public_key().fingerprint());
    }

    #[test]
    fn rewrap_after_removing_all_keys_re_seals_empty_store() {
        let (state, _old_sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            app_root: state.path().to_path_buf(),
            entries: vec![("ONLY".into(), "value".into())],
        })
        .unwrap();
        run_remove(&RemoveOptions {
            app_root: state.path().to_path_buf(),
            keys: vec![("ONLY".into())],
        })
        .unwrap();
        let store_path = default_sealed_store_path(state.path());
        assert!(
            store_path.exists(),
            "precondition: store.enc must exist after remove-all"
        );

        run_rewrap(&RewrapOptions {
            app_root: state.path().to_path_buf(),
        })
        .unwrap();

        let key_path = default_vault_secret_key_path(state.path());
        let new_sk = lillux::vault::read_secret_key(&key_path).unwrap();
        let map = read_sealed_secrets(&store_path, &new_sk).unwrap();
        assert!(map.is_empty());
    }
}
