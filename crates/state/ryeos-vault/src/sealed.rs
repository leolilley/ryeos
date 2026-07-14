use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::policy::{validate_decrypted_keys, MAX_VAULT_ENVELOPE_BYTES, MAX_VAULT_PLAINTEXT_BYTES};
pub use lillux::vault::MAX_VAULT_KEY_FILE_BYTES;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RewrapJournal {
    version: u32,
    store_present: bool,
}

const MAX_REWRAP_JOURNAL_BYTES: u64 = 4 * 1024;

fn rewrap_journal_path(store_path: &Path) -> std::path::PathBuf {
    store_path.with_extension("rewrap-journal.toml")
}

fn rewrap_backup_path(path: &Path) -> std::path::PathBuf {
    path.with_extension(format!(
        "{}.rewrap-backup",
        path.extension()
            .map(|extension| extension.to_string_lossy())
            .unwrap_or_default()
    ))
}

fn rewrap_staged_path(path: &Path) -> std::path::PathBuf {
    path.with_extension(format!(
        "{}.new",
        path.extension()
            .map(|extension| extension.to_string_lossy())
            .unwrap_or_default()
    ))
}

/// Durably remove the deterministic staging files used by vault rewrap.
///
/// Staged files are never authoritative: the journal and live key/store paths
/// decide whether recovery commits or restores a generation. Recovery may
/// therefore remove these files even when a crash happened before the journal
/// itself was published.
pub fn cleanup_staged_rewrap_files(
    key_path: &Path,
    public_key_path: &Path,
    store_path: &Path,
) -> Result<()> {
    for path in [
        rewrap_staged_path(key_path),
        rewrap_staged_path(public_key_path),
        rewrap_staged_path(store_path),
    ] {
        lillux::remove_file_durable(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

/// Hold an interprocess lock for a complete sealed-store operation.
///
/// Mutators must lock across read, modify, and atomic replacement; locking only
/// the final write still permits two processes to overwrite each other's
/// changes after reading the same prior generation.
pub fn with_store_lock<T>(store_path: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    lillux::with_exclusive_file_lock(store_path, operation)
        .with_context(|| format!("lock sealed store {}", store_path.display()))
}

/// Recover an interrupted key/store rotation while the caller holds the store
/// lock. A readable current key/store pair is committed; otherwise the complete
/// previous generation is restored from durable backups.
pub fn recover_rewrap(key_path: &Path, public_key_path: &Path, store_path: &Path) -> Result<()> {
    let journal_path = rewrap_journal_path(store_path);
    if !journal_path.exists() {
        return cleanup_staged_rewrap_files(key_path, public_key_path, store_path);
    }
    let journal_raw = String::from_utf8(read_bounded_file(
        &journal_path,
        MAX_REWRAP_JOURNAL_BYTES,
        "rewrap journal",
    )?)
    .with_context(|| format!("rewrap journal {} is not UTF-8", journal_path.display()))?;
    let journal: RewrapJournal = toml::from_str(&journal_raw)
        .with_context(|| format!("parse rewrap journal {}", journal_path.display()))?;
    if journal.version != 1 {
        return Err(anyhow!(
            "unsupported vault rewrap journal version {}",
            journal.version
        ));
    }

    let current_valid = lillux::vault::read_secret_key(key_path)
        .and_then(|key| {
            if journal.store_present {
                read_sealed_secrets(store_path, &key).map(|_| key)
            } else {
                Ok(key)
            }
        })
        .ok();

    if let Some(key) = current_valid {
        lillux::vault::write_public_key(public_key_path, &key.public_key())
            .context("repair public key after completed rewrap")?;
        cleanup_rewrap_files(key_path, public_key_path, store_path)?;
        return Ok(());
    }

    let key_backup = rewrap_backup_path(key_path);
    let store_backup = rewrap_backup_path(store_path);
    let old_key = lillux::vault::read_secret_key(&key_backup)
        .with_context(|| format!("read rewrap key backup {}", key_backup.display()))?;
    if journal.store_present {
        read_sealed_secrets(&store_backup, &old_key)
            .context("rewrap backup generation is not readable")?;
        let store_bytes = read_bounded_file(
            &store_backup,
            MAX_VAULT_ENVELOPE_BYTES,
            "sealed envelope backup",
        )?;
        lillux::atomic_write_private(store_path, &store_bytes)
            .context("restore sealed store after interrupted rewrap")?;
    }
    let key_bytes = read_bounded_file(
        &key_backup,
        MAX_VAULT_KEY_FILE_BYTES,
        "vault secret-key backup",
    )?;
    lillux::atomic_write_private(key_path, &key_bytes)
        .context("restore secret key after interrupted rewrap")?;
    lillux::vault::write_public_key(public_key_path, &old_key.public_key())
        .context("restore public key after interrupted rewrap")?;
    cleanup_rewrap_files(key_path, public_key_path, store_path)
}

/// Persist rollback material and a journal before rotating live vault files.
pub fn prepare_rewrap(key_path: &Path, public_key_path: &Path, store_path: &Path) -> Result<()> {
    let key_backup = rewrap_backup_path(key_path);
    let public_backup = rewrap_backup_path(public_key_path);
    let store_backup = rewrap_backup_path(store_path);

    lillux::atomic_write_private(
        &key_backup,
        &read_bounded_file(key_path, MAX_VAULT_KEY_FILE_BYTES, "vault secret key")?,
    )?;
    if public_key_path.exists() {
        lillux::atomic_write(
            &public_backup,
            &read_bounded_file(
                public_key_path,
                MAX_VAULT_KEY_FILE_BYTES,
                "vault public key",
            )?,
        )?;
    }
    let store_present = store_path.exists();
    if store_present {
        lillux::atomic_write_private(
            &store_backup,
            &read_bounded_file(store_path, MAX_VAULT_ENVELOPE_BYTES, "sealed envelope")?,
        )?;
    }
    let journal = toml::to_string(&RewrapJournal {
        version: 1,
        store_present,
    })?;
    lillux::atomic_write_private(&rewrap_journal_path(store_path), journal.as_bytes())
        .context("write durable vault rewrap journal")
}

fn cleanup_rewrap_files(key_path: &Path, public_key_path: &Path, store_path: &Path) -> Result<()> {
    // Journal removal is the commit marker. Flush it before removing rollback
    // material so a crash cannot expose a journal whose backups are gone.
    lillux::remove_file_durable(&rewrap_journal_path(store_path))?;
    for path in [
        rewrap_backup_path(key_path),
        rewrap_backup_path(public_key_path),
        rewrap_backup_path(store_path),
    ] {
        lillux::remove_file_durable(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    cleanup_staged_rewrap_files(key_path, public_key_path, store_path)
}

pub fn write_sealed_secrets(
    store_path: &Path,
    vault_pk: &lillux::vault::VaultPublicKey,
    secrets: &HashMap<String, String>,
) -> Result<()> {
    validate_decrypted_keys(secrets, store_path)?;

    let mut sorted: Vec<(&String, &String)> = secrets.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut plaintext_toml = String::new();
    for (k, v) in &sorted {
        let line = format!("{k} = {}\n", toml_quote(v));
        let next_len = plaintext_toml
            .len()
            .checked_add(line.len())
            .ok_or_else(|| anyhow!("vault: plaintext size overflow"))?;
        if next_len > MAX_VAULT_PLAINTEXT_BYTES {
            anyhow::bail!(
                "vault: serialized plaintext is {next_len} bytes; maximum is {MAX_VAULT_PLAINTEXT_BYTES}"
            );
        }
        plaintext_toml.push_str(&line);
    }

    let envelope = lillux::vault::seal(vault_pk, plaintext_toml.as_bytes())
        .map_err(|e| anyhow!("vault: seal failed: {e:#}"))?;
    let envelope_toml =
        toml::to_string(&envelope).map_err(|e| anyhow!("vault: serialize envelope: {e}"))?;
    if envelope_toml.len() as u64 > MAX_VAULT_ENVELOPE_BYTES {
        anyhow::bail!(
            "vault: serialized envelope is {} bytes; maximum is {MAX_VAULT_ENVELOPE_BYTES}",
            envelope_toml.len()
        );
    }

    lillux::atomic_write_private(store_path, envelope_toml.as_bytes())
        .map_err(|e| anyhow!("vault: write sealed store {}: {e:#}", store_path.display()))
}

pub fn read_sealed_secrets(
    store_path: &Path,
    sk: &lillux::vault::VaultSecretKey,
) -> Result<HashMap<String, String>> {
    if !store_path.exists() {
        return Ok(HashMap::new());
    }
    let raw = read_bounded_envelope(store_path)?;
    let envelope: lillux::vault::SealedEnvelope = toml::from_str(&raw)
        .with_context(|| format!("parse envelope TOML at {}", store_path.display()))?;
    let plaintext =
        lillux::vault::open(sk, &envelope).map_err(|e| anyhow!("open envelope: {e:#}"))?;
    if plaintext.len() > MAX_VAULT_PLAINTEXT_BYTES {
        anyhow::bail!(
            "vault: decrypted plaintext is {} bytes; maximum is {MAX_VAULT_PLAINTEXT_BYTES}",
            plaintext.len()
        );
    }
    let plaintext_str =
        std::str::from_utf8(&plaintext).context("decrypted plaintext is not UTF-8")?;
    let map: HashMap<String, String> =
        toml::from_str(plaintext_str).context("decrypted plaintext is not a TOML map")?;
    validate_decrypted_keys(&map, store_path)?;
    Ok(map)
}

fn read_bounded_envelope(store_path: &Path) -> Result<String> {
    String::from_utf8(read_bounded_file(
        store_path,
        MAX_VAULT_ENVELOPE_BYTES,
        "sealed envelope",
    )?)
    .with_context(|| format!("sealed envelope {} is not UTF-8", store_path.display()))
}

/// Read a vault-related file without trusting a potentially stale metadata
/// length. Rewrap activation uses this for its generated staging files too, so
/// a same-UID replacement cannot turn the final copy into an unbounded read.
pub fn read_bounded_file(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let envelope_len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if envelope_len > maximum {
        anyhow::bail!(
            "vault: {label} {} is {envelope_len} bytes; maximum is {maximum}",
            path.display()
        );
    }
    let initial_capacity = usize::try_from(envelope_len)
        .unwrap_or(0)
        .min(maximum as usize);
    let mut bytes = Vec::with_capacity(initial_capacity);
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() as u64 > maximum {
        anyhow::bail!(
            "vault: {label} {} exceeds the {maximum}-byte maximum",
            path.display()
        );
    }
    Ok(bytes)
}

fn toml_quote(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii() && b != b'"' && b != b'\\' && !b.is_ascii_control())
    {
        format!("\"{s}\"")
    } else {
        let mut tmp = std::collections::BTreeMap::new();
        tmp.insert("v", s);
        let serialized = toml::to_string(&tmp).unwrap_or_else(|_| format!("\"{s}\""));
        serialized
            .trim()
            .strip_prefix("v = ")
            .unwrap_or(&serialized)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::vault::VaultSecretKey;

    fn sample_secrets() -> HashMap<String, String> {
        let mut secrets = HashMap::new();
        secrets.insert("API_TOKEN".to_string(), "hunter2".to_string());
        secrets.insert("DB_PASSWORD".to_string(), "p@ss word".to_string());
        secrets
    }

    /// An envelope sealed to vault key A must not open with vault key
    /// B's secret key. lillux's open() refuses on the
    /// vault_pubkey_fingerprint mismatch with a rewrap hint.
    #[test]
    fn open_rejects_wrong_vault_key() {
        let key_a = VaultSecretKey::generate();
        let key_b = VaultSecretKey::generate();
        let tmp = tempfile::TempDir::new().unwrap();
        let store = tmp.path().join("vault").join("secrets.toml");

        write_sealed_secrets(&store, &key_a.public_key(), &sample_secrets()).unwrap();

        // Positive control: the matching secret key opens the store.
        let opened = read_sealed_secrets(&store, &key_a).expect("matching key must open");
        assert_eq!(opened, sample_secrets());

        // Wrong key: refused, with fingerprint mismatch + rewrap hint.
        let err = read_sealed_secrets(&store, &key_b)
            .expect_err("wrong vault key must not open the store");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fingerprint"),
            "error should mention the fingerprint mismatch, got: {msg}"
        );
        assert!(
            msg.contains("rewrap"),
            "error should hint at `ryeos vault rewrap`, got: {msg}"
        );
    }

    /// A corrupted persisted envelope (truncated nonce or ciphertext
    /// base64) must fail with an error, never panic or yield garbage.
    #[test]
    fn open_rejects_corrupted_envelope() {
        let key = VaultSecretKey::generate();
        let tmp = tempfile::TempDir::new().unwrap();
        let store = tmp.path().join("secrets.toml");

        write_sealed_secrets(&store, &key.public_key(), &sample_secrets()).unwrap();
        let raw = std::fs::read_to_string(&store).unwrap();

        // Truncate the ciphertext base64 (still valid base64, shorter
        // ciphertext: the AEAD tag check must refuse it).
        let mut envelope: lillux::vault::SealedEnvelope = toml::from_str(&raw).unwrap();
        assert!(
            envelope.ciphertext.len() > 8,
            "ciphertext should be long enough to truncate"
        );
        let truncated_len = envelope.ciphertext.len() - 8;
        envelope.ciphertext.truncate(truncated_len);
        std::fs::write(&store, toml::to_string(&envelope).unwrap()).unwrap();
        let err =
            read_sealed_secrets(&store, &key).expect_err("truncated ciphertext must fail to open");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("open envelope"),
            "corruption should surface as an open failure, got: {msg}"
        );

        // Truncate the nonce base64 the same way.
        let mut envelope: lillux::vault::SealedEnvelope = toml::from_str(&raw).unwrap();
        let truncated_len = envelope.nonce.len() - 8;
        envelope.nonce.truncate(truncated_len);
        std::fs::write(&store, toml::to_string(&envelope).unwrap()).unwrap();
        let err = read_sealed_secrets(&store, &key).expect_err("truncated nonce must fail to open");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("open envelope"),
            "corruption should surface as an open failure, got: {msg}"
        );
    }

    #[test]
    fn read_rejects_oversized_envelope_before_allocation() {
        let key = VaultSecretKey::generate();
        let tmp = tempfile::TempDir::new().unwrap();
        let store = tmp.path().join("oversized-store.enc");
        std::fs::File::create(&store)
            .unwrap()
            .set_len(MAX_VAULT_ENVELOPE_BYTES + 1)
            .unwrap();

        let error = read_sealed_secrets(&store, &key).unwrap_err();
        assert!(
            format!("{error:#}").contains("maximum"),
            "oversized envelope should fail at the file bound: {error:#}"
        );
    }

    #[test]
    fn recover_rewrap_restores_previous_complete_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("vault/private_key.pem");
        let public_path = tmp.path().join("vault/public_key.pem");
        let store_path = tmp.path().join("secrets/store.enc");
        let old_key = VaultSecretKey::generate();
        lillux::vault::write_secret_key(&key_path, &old_key).unwrap();
        lillux::vault::write_public_key(&public_path, &old_key.public_key()).unwrap();
        write_sealed_secrets(&store_path, &old_key.public_key(), &sample_secrets()).unwrap();
        prepare_rewrap(&key_path, &public_path, &store_path).unwrap();

        let new_key = VaultSecretKey::generate();
        write_sealed_secrets(&store_path, &new_key.public_key(), &sample_secrets()).unwrap();
        recover_rewrap(&key_path, &public_path, &store_path).unwrap();

        let restored = lillux::vault::read_secret_key(&key_path).unwrap();
        assert_eq!(
            restored.public_key().fingerprint(),
            old_key.public_key().fingerprint()
        );
        assert_eq!(
            read_sealed_secrets(&store_path, &restored).unwrap(),
            sample_secrets()
        );
    }

    #[test]
    fn recover_rewrap_commits_complete_new_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("vault/private_key.pem");
        let public_path = tmp.path().join("vault/public_key.pem");
        let store_path = tmp.path().join("secrets/store.enc");
        let old_key = VaultSecretKey::generate();
        lillux::vault::write_secret_key(&key_path, &old_key).unwrap();
        lillux::vault::write_public_key(&public_path, &old_key.public_key()).unwrap();
        write_sealed_secrets(&store_path, &old_key.public_key(), &sample_secrets()).unwrap();
        prepare_rewrap(&key_path, &public_path, &store_path).unwrap();

        let new_key = VaultSecretKey::generate();
        write_sealed_secrets(&store_path, &new_key.public_key(), &sample_secrets()).unwrap();
        lillux::vault::write_secret_key(&key_path, &new_key).unwrap();
        recover_rewrap(&key_path, &public_path, &store_path).unwrap();

        let committed = lillux::vault::read_secret_key(&key_path).unwrap();
        assert_eq!(
            committed.public_key().fingerprint(),
            new_key.public_key().fingerprint()
        );
        assert_eq!(
            read_sealed_secrets(&store_path, &committed).unwrap(),
            sample_secrets()
        );
        let public = lillux::vault::read_public_key(&public_path).unwrap();
        assert_eq!(public.fingerprint(), new_key.public_key().fingerprint());
    }

    #[test]
    fn recover_rewrap_removes_staging_left_before_journal_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("vault/private_key.pem");
        let public_path = tmp.path().join("vault/public_key.pem");
        let store_path = tmp.path().join("secrets/store.enc");
        let key = VaultSecretKey::generate();
        lillux::vault::write_secret_key(&key_path, &key).unwrap();
        lillux::vault::write_public_key(&public_path, &key.public_key()).unwrap();

        let staged_key = rewrap_staged_path(&key_path);
        let staged_public = rewrap_staged_path(&public_path);
        let staged_store = rewrap_staged_path(&store_path);
        lillux::atomic_write_private(&staged_key, b"unused secret").unwrap();
        lillux::atomic_write(&staged_public, b"unused public").unwrap();
        lillux::atomic_write_private(&staged_store, b"unused store").unwrap();

        recover_rewrap(&key_path, &public_path, &store_path).unwrap();

        assert!(!staged_key.exists());
        assert!(!staged_public.exists());
        assert!(!staged_store.exists());
        assert!(key_path.exists());
        assert!(public_path.exists());
    }
}
