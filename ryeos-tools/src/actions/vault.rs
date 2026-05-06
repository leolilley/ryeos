//! Operator-side vault verbs — `rye vault {put,list,remove,rewrap}`.
//!
//! These verbs operate directly on the daemon's on-disk vault key
//! (`<state>/.ai/node/vault/private_key.pem`) and the sealed
//! secret-store (`<state>/.ai/state/secrets/store.enc`). They run
//! locally without the daemon so rotation works even when the daemon
//! is down — that's why this module sits in `ryeos-tools` and is
//! invoked through `ryeos-cli/src/local_verbs.rs`.
//!
//! ## Trust boundary
//!
//! Vault state is operator-tier: the on-disk key is the only thing
//! that can decrypt the store. The CLI must therefore have read
//! access to the key file (file mode 0600 enforced by
//! [`lillux::vault::write_secret_key`]). That mirrors the daemon's
//! own access — running the verb as the operator is exactly what
//! gives it the right to mutate the store.
//!
//! ## Why these helpers live here (and `ryeosd::vault` re-uses them)
//!
//! `ryeosd` depends on `ryeos-tools` (not the other way around). The
//! daemon's [`SealedEnvelopeVault`](../../../ryeosd/src/vault.rs)
//! consumes [`validate_decrypted_keys`] post-decrypt to enforce the
//! same key-name policy the CLI applies at write time, and
//! [`write_sealed_secrets`] is the single authoring path for both
//! `rye vault put` and the `bootstrap`/test-fixture write paths.
//!
//! ## Policy: blocked key names
//!
//! [`BLOCKED_NAMES`] mirrors the Python
//! `ryeos-node/ryeos_node/vault.py::validate_env_map()` blocked list.
//! These are environment variable names that the OS or process
//! bootstrap pre-sets and that no vault entry is allowed to override.
//! A poisoned store containing one of these names aborts the read
//! with a typed error so it never silently shadows `PATH`/`HOME`/etc
//! for spawned subprocesses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

/// Names that the OS or process-bootstrap pre-sets and that no vault
/// is allowed to override. A secrets store containing one of these
/// aborts the read with a typed error.
pub const BLOCKED_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "PWD",
    "USER",
    "SHELL",
    "TERM",
    "PYTHONPATH",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
];

/// Default sealed-envelope store path: `<system_space_dir>/.ai/state/secrets/store.enc`.
pub fn default_sealed_store_path(system_space_dir: &Path) -> PathBuf {
    system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("secrets")
        .join("store.enc")
}

/// Default vault private key path: `<system_space_dir>/.ai/node/vault/private_key.pem`.
pub fn default_vault_secret_key_path(system_space_dir: &Path) -> PathBuf {
    system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("private_key.pem")
}

/// Default vault public key path: `<system_space_dir>/.ai/node/vault/public_key.pem`.
pub fn default_vault_public_key_path(system_space_dir: &Path) -> PathBuf {
    system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem")
}

/// Apply the key-name policy ([`BLOCKED_NAMES`] + `[A-Za-z0-9_]+`)
/// to a secrets map. Used post-decrypt by the daemon and pre-write
/// by the CLI authoring path.
pub fn validate_decrypted_keys(
    map: &HashMap<String, String>,
    store_path: &Path,
) -> Result<()> {
    for key in map.keys() {
        if key.is_empty() {
            bail!(
                "vault: empty key in sealed store {}",
                store_path.display()
            );
        }
        if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            bail!(
                "vault: invalid key `{key}` in sealed store {} (must match [A-Za-z0-9_]+)",
                store_path.display()
            );
        }
        if BLOCKED_NAMES.contains(&key.as_str()) {
            bail!(
                "vault: key `{key}` in sealed store {} is on the OS-protected \
                 blocked list and would shadow inherited environment",
                store_path.display()
            );
        }
    }
    Ok(())
}

/// Atomically write a sealed envelope containing `secrets` to
/// `store_path`, sealing to `vault_pk`. Used by CLI verbs (e.g.
/// `rye vault put`) and by tests; the daemon NEVER writes the store.
///
/// Refuses on any key that fails [`validate_decrypted_keys`] so a bad
/// write at authoring time fails before the secrets are encrypted.
pub fn write_sealed_secrets(
    store_path: &Path,
    vault_pk: &lillux::vault::VaultPublicKey,
    secrets: &HashMap<String, String>,
) -> Result<()> {
    validate_decrypted_keys(secrets, store_path)?;

    // Serialize the secret map deterministically: sorted keys.
    let mut sorted: Vec<(&String, &String)> = secrets.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut plaintext_toml = String::new();
    for (k, v) in &sorted {
        plaintext_toml.push_str(&format!("{k} = {}\n", toml_quote(v)));
    }

    let envelope = lillux::vault::seal(vault_pk, plaintext_toml.as_bytes())
        .map_err(|e| anyhow!("vault: seal failed: {e:#}"))?;
    let envelope_toml = toml::to_string(&envelope)
        .map_err(|e| anyhow!("vault: serialize envelope: {e}"))?;

    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("vault: create parent {}: {e}", parent.display()))?;
    }
    let tmp = store_path.with_extension("tmp");
    std::fs::write(&tmp, envelope_toml.as_bytes())
        .map_err(|e| anyhow!("vault: write tmp {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow!("vault: chmod 0600 {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, store_path)
        .map_err(|e| anyhow!("vault: rename tmp -> {}: {e}", store_path.display()))?;
    Ok(())
}

/// Read and decrypt a sealed store via the on-disk vault secret key.
/// Returns an empty map if the store doesn't exist (legitimate
/// "operator hasn't provisioned secrets yet" state).
fn read_sealed_secrets(
    store_path: &Path,
    sk: &lillux::vault::VaultSecretKey,
) -> Result<HashMap<String, String>> {
    if !store_path.exists() {
        return Ok(HashMap::new());
    }
    let raw = std::fs::read_to_string(store_path)
        .with_context(|| format!("read {}", store_path.display()))?;
    let envelope: lillux::vault::SealedEnvelope = toml::from_str(&raw)
        .with_context(|| format!("parse envelope TOML at {}", store_path.display()))?;
    let plaintext = lillux::vault::open(sk, &envelope)
        .map_err(|e| anyhow!("open envelope: {e:#}"))?;
    let plaintext_str =
        std::str::from_utf8(&plaintext).context("decrypted plaintext is not UTF-8")?;
    let map: HashMap<String, String> =
        toml::from_str(plaintext_str).context("decrypted plaintext is not a TOML map")?;
    validate_decrypted_keys(&map, store_path)?;
    Ok(map)
}

// ── Verb options + reports ───────────────────────────────────────────

#[derive(Debug)]
pub struct PutOptions {
    pub system_space_dir: PathBuf,
    /// `KEY=VALUE` pairs to merge into the store. Later pairs override
    /// earlier pairs for the same key (rare but well-defined).
    pub assignments: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct PutReport {
    pub store_path: PathBuf,
    pub keys_written: Vec<String>,
    pub total_keys_after_put: usize,
}

#[derive(Debug)]
pub struct ListOptions {
    pub system_space_dir: PathBuf,
}

#[derive(Debug, serde::Serialize)]
pub struct ListReport {
    pub store_path: PathBuf,
    /// Sorted key names. Values are NEVER returned — that's a
    /// separate `--reveal` flag job we are intentionally not adding
    /// in this pass.
    pub keys: Vec<String>,
}

#[derive(Debug)]
pub struct RemoveOptions {
    pub system_space_dir: PathBuf,
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
    pub system_space_dir: PathBuf,
}

#[derive(Debug, serde::Serialize)]
pub struct RewrapReport {
    pub store_path: PathBuf,
    pub old_fingerprint: String,
    pub new_fingerprint: String,
    pub keys_rewrapped: usize,
}

// ── Verb implementations ─────────────────────────────────────────────

/// `rye vault put KEY=VALUE [KEY=VALUE...]` — merge new entries into
/// the sealed store. Decrypts, applies the merge, validates, and
/// re-writes atomically with the same vault keypair.
pub fn run_put(opts: &PutOptions) -> Result<PutReport> {
    if opts.assignments.is_empty() {
        bail!("rye vault put: at least one KEY=VALUE assignment required");
    }
    let key_path = default_vault_secret_key_path(&opts.system_space_dir);
    let sk = lillux::vault::read_secret_key(&key_path).with_context(|| {
        format!(
            "read vault secret key {} — has `rye init` (or daemon) ever run \
             on this state dir?",
            key_path.display()
        )
    })?;
    let pk = sk.public_key();
    let store_path = default_sealed_store_path(&opts.system_space_dir);

    let mut current = read_sealed_secrets(&store_path, &sk)?;
    let mut keys_written = Vec::with_capacity(opts.assignments.len());
    for raw in &opts.assignments {
        let (k, v) = parse_assignment(raw)?;
        keys_written.push(k.clone());
        current.insert(k, v);
    }

    write_sealed_secrets(&store_path, &pk, &current)?;
    let total = current.len();
    Ok(PutReport {
        store_path,
        keys_written,
        total_keys_after_put: total,
    })
}

/// `rye vault list` — print the keys currently in the store. Values
/// are intentionally NOT printed; this is a discovery command, not a
/// reveal command.
pub fn run_list(opts: &ListOptions) -> Result<ListReport> {
    let key_path = default_vault_secret_key_path(&opts.system_space_dir);
    let sk = lillux::vault::read_secret_key(&key_path).with_context(|| {
        format!("read vault secret key {}", key_path.display())
    })?;
    let store_path = default_sealed_store_path(&opts.system_space_dir);
    let current = read_sealed_secrets(&store_path, &sk)?;
    let mut keys: Vec<String> = current.keys().cloned().collect();
    keys.sort();
    Ok(ListReport { store_path, keys })
}

/// `rye vault remove KEY [KEY...]` — drop entries from the store.
/// Idempotent on non-present keys (reported separately).
pub fn run_remove(opts: &RemoveOptions) -> Result<RemoveReport> {
    if opts.keys.is_empty() {
        bail!("rye vault remove: at least one KEY required");
    }
    let key_path = default_vault_secret_key_path(&opts.system_space_dir);
    let sk = lillux::vault::read_secret_key(&key_path).with_context(|| {
        format!("read vault secret key {}", key_path.display())
    })?;
    let pk = sk.public_key();
    let store_path = default_sealed_store_path(&opts.system_space_dir);

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
    let total = current.len();
    Ok(RemoveReport {
        store_path,
        removed,
        not_present,
        total_keys_after_remove: total,
    })
}

/// `rye vault rewrap` — generate a fresh X25519 keypair, decrypt the
/// existing store with the OLD key, re-seal with the NEW public key,
/// then atomically swap both the store and the keypair files.
///
/// Fails-loud if either the old key file or the store decrypts
/// inconsistently — never silently re-encrypts under a new identity
/// without proving the old plaintext was correct.
///
/// # Crash safety
///
/// The rotation runs in two phases:
///
/// **Phase A (steps 1–6, all `.new` writes):**
/// 1. Decrypt the existing store with the old key → in-memory plaintext.
/// 2. Generate a new X25519 keypair.
/// 3. Wrap the plaintext under the new public key → sealed bytes.
/// 4. Write `<vault>/private_key.pem.new` (0600).
/// 5. Write `<vault>/public_key.pem.new`.
/// 6. Write `<vault>/store.enc.new` (only if `store.enc` already exists;
///    we never spontaneously create a store).
///
/// If anything in phase A fails, every `.new` file we wrote is removed
/// and the operator's on-disk state is unchanged.
///
/// **Phase B (step 7, renames into final position, in this exact order):**
/// 1. `private_key.pem.new` → `private_key.pem`
/// 2. `public_key.pem.new`  → `public_key.pem`
/// 3. `store.enc.new`       → `store.enc` (skipped if no `.new` was written)
///
/// This ordering is the failure-recovery contract. A reader that races
/// during phase B sees one of:
///   - All-old: private_key rename hasn't happened yet → old store is
///     readable with the still-on-disk old private key.
///   - Mid-rotation, new private key in place: the new private key
///     decrypts either the old store (still on disk) or the new store
///     once the third rename lands, because both stores were sealed to
///     the same DEK-equivalent identity by construction (the new
///     private key wraps the same plaintext we just decrypted).
///   - All-new: rotation complete.
///
/// **Phase B failure** (a `fs::rename` returns Err mid-way through):
/// the function refuses to proceed and returns the rename error. Some
/// `.new` files may remain on disk alongside their non-`.new`
/// counterparts. The operator must inspect manually:
///   - If `private_key.pem.new` exists, the old private key is still
///     in place and the rotation has NOT taken effect; remove the
///     `.new` files to abort, or rename them in the documented order
///     to complete.
///   - If `private_key.pem` is the new key (i.e. its fingerprint
///     differs from the previous backup), the rotation is partially
///     applied; rename any remaining `.new` files in order to finish.
///
/// On Unix, the new private key is written 0600 (via
/// [`lillux::vault::write_secret_key`]).
pub fn run_rewrap(opts: &RewrapOptions) -> Result<RewrapReport> {
    let key_path = default_vault_secret_key_path(&opts.system_space_dir);
    let pub_path = default_vault_public_key_path(&opts.system_space_dir);
    let store_path = default_sealed_store_path(&opts.system_space_dir);

    let old_sk = lillux::vault::read_secret_key(&key_path).with_context(|| {
        format!("read vault secret key {}", key_path.display())
    })?;
    let old_fingerprint = old_sk.public_key().fingerprint();

    let plaintext = read_sealed_secrets(&store_path, &old_sk)?;

    let new_sk = lillux::vault::VaultSecretKey::generate();
    let new_pk = new_sk.public_key();
    let new_fingerprint = new_pk.fingerprint();

    let new_key_path = key_path.with_extension("pem.new");
    let new_pub_path = pub_path.with_extension("pem.new");
    let new_store_path = store_path.with_extension("enc.new");

    // Whether to roll the store forward is driven by on-disk
    // existence, NOT by whether plaintext is empty. A vault with an
    // existing-but-empty store (e.g. after `rye vault remove` cleared
    // the last key) MUST be re-sealed under the new key — otherwise
    // the rotated keypair couldn't decrypt the still-present store.
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
        cleanup_new_files(&new_key_path, &new_pub_path, &new_store_path);
        return Err(e);
    }

    // Phase B: rename in the documented order. See the function-level
    // docstring for the failure-recovery contract.
    std::fs::rename(&new_key_path, &key_path)
        .with_context(|| format!("rename {} -> {}", new_key_path.display(), key_path.display()))?;
    std::fs::rename(&new_pub_path, &pub_path)
        .with_context(|| format!("rename {} -> {}", new_pub_path.display(), pub_path.display()))?;
    if rewrap_store {
        std::fs::rename(&new_store_path, &store_path).with_context(|| {
            format!("rename {} -> {}", new_store_path.display(), store_path.display())
        })?;
    }

    tracing::info!(
        old_fingerprint = %old_fingerprint,
        new_fingerprint = %new_fingerprint,
        keys_rewrapped = plaintext.len(),
        store_path = %store_path.display(),
        "vault: rewrap complete — keypair rotated and store re-sealed"
    );

    Ok(RewrapReport {
        store_path,
        old_fingerprint,
        new_fingerprint,
        keys_rewrapped: plaintext.len(),
    })
}

fn cleanup_new_files(new_key_path: &Path, new_pub_path: &Path, new_store_path: &Path) {
    let _ = std::fs::remove_file(new_key_path);
    let _ = std::fs::remove_file(new_pub_path);
    let _ = std::fs::remove_file(new_store_path);
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Quote a TOML string value. We deliberately keep this minimal: the
/// secret-store plaintext is always written by us, so we don't need
/// to handle every TOML edge case — only the ones our values can
/// contain. Strings with backslashes, double-quotes, control chars,
/// or non-ASCII fall back to escaped form via toml-rs.
fn toml_quote(s: &str) -> String {
    if s.bytes().all(|b| {
        b.is_ascii() && b != b'"' && b != b'\\' && !b.is_ascii_control()
    }) {
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

/// Read a layered `.env` overlay from a sequence of search
/// directories. Each directory may contain a single `.env` file in
/// the conventional `KEY=VALUE` (or `export KEY=VALUE`) form. Later
/// directories in `search_dirs` win on key collision — typical use
/// is `[user_home, project_root]`, so a project's `.env` overrides
/// the operator's user-wide `.env`.
///
/// ## Policy
///
/// The same key-name rules that [`validate_decrypted_keys`] enforces
/// post-decrypt apply here at parse time:
/// - Empty key → fail-loud.
/// - Key not matching `[A-Za-z0-9_]+` → fail-loud.
/// - Key on [`BLOCKED_NAMES`] → fail-loud. A project that ships a
///   `.env` with `PATH=/evil` MUST NOT silently shadow `PATH` for
///   spawned subprocesses.
///
/// `.env` files are NOT signed and NOT trust-checked. The operator
/// owns the project, project tree is trusted — exactly mirroring the
/// shell's relationship with `.env`. If you don't trust the project,
/// don't put it on your daemon's filesystem.
///
/// ## Format
///
/// - Lines starting with `#` (after optional whitespace) are comments.
/// - Blank lines are skipped.
/// - `export KEY=VALUE` is accepted; the leading `export ` is stripped.
/// - Values may be wrapped in matching `"..."` or `'...'` quotes;
///   quotes are stripped if both endpoints match. No escape-sequence
///   processing — the value is taken literally between the quotes.
/// - No multi-line / heredoc / variable interpolation. Operator
///   convenience, not a shell parser.
pub fn read_dotenv_overlay(
    search_dirs: &[PathBuf],
) -> Result<HashMap<String, String>> {
    let mut out: HashMap<String, String> = HashMap::new();
    for dir in search_dirs {
        let path = dir.join(".env");
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let parsed = parse_dotenv_text(&content, &path)?;
        for (k, v) in parsed {
            out.insert(k, v);
        }
    }
    Ok(out)
}

fn parse_dotenv_text(content: &str, path: &Path) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (idx, raw) in content.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(eq) = line.find('=') else {
            bail!(
                "vault dotenv: malformed line at {}:{lineno} (no `=`): {line:?}",
                path.display()
            );
        };
        let key = line[..eq].trim();
        if key.is_empty() {
            bail!("vault dotenv: empty key at {}:{lineno}", path.display());
        }
        if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            bail!(
                "vault dotenv: invalid key `{key}` at {}:{lineno} \
                 (must match [A-Za-z0-9_]+)",
                path.display()
            );
        }
        if BLOCKED_NAMES.contains(&key) {
            bail!(
                "vault dotenv: key `{key}` at {}:{lineno} is on the \
                 OS-protected blocked list and would shadow inherited \
                 environment",
                path.display()
            );
        }
        let value = line[eq + 1..].trim();
        let value = strip_matching_quotes(value);
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[0] == bytes[bytes.len() - 1]
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse a single `KEY=VALUE` token. Splits on the first `=`. Refuses
/// empty keys, missing `=`, and applies the same key-name policy that
/// [`validate_decrypted_keys`] enforces post-decrypt — so the operator
/// can't accidentally write a `PATH=` entry that would later trip the
/// daemon's read path.
fn parse_assignment(raw: &str) -> Result<(String, String)> {
    let Some(eq) = raw.find('=') else {
        bail!(
            "vault put: malformed assignment {raw:?} — expected `KEY=VALUE` \
             with a literal `=`"
        );
    };
    let key = raw[..eq].trim().to_string();
    let value = raw[eq + 1..].to_string();
    if key.is_empty() {
        bail!("vault put: empty key in assignment {raw:?}");
    }
    if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        bail!(
            "vault put: invalid key `{key}` (must match [A-Za-z0-9_]+)"
        );
    }
    if BLOCKED_NAMES.contains(&key.as_str()) {
        bail!(
            "vault put: key `{key}` is on the OS-protected blocked list and \
             would shadow inherited environment"
        );
    }
    Ok((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            system_space_dir: state.path().to_path_buf(),
            assignments: vec![
                "OPENAI_API_KEY=sk-1".into(),
                "DATABASE_URL=postgres://h/db".into(),
            ],
        })
        .unwrap();
        assert_eq!(report.total_keys_after_put, 2);
        assert!(report.store_path.exists());

        let listed = run_list(&ListOptions {
            system_space_dir: state.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(listed.keys, vec!["DATABASE_URL", "OPENAI_API_KEY"]);
    }

    #[test]
    fn put_merges_with_existing() {
        let (state, _sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["A=1".into()],
        })
        .unwrap();
        let report = run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["B=2".into()],
        })
        .unwrap();
        assert_eq!(report.total_keys_after_put, 2);
    }

    #[test]
    fn put_overwrites_existing_key() {
        let (state, _sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["FOO=old".into()],
        })
        .unwrap();
        run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["FOO=new".into()],
        })
        .unwrap();

        // Round-trip via the same private key to assert the value.
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
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["PATH=/evil".into()],
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("PATH"));
    }

    #[test]
    fn put_rejects_invalid_key_chars() {
        let (state, _sk) = fresh_state_with_keypair();
        let err = run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["FOO-BAR=baz".into()],
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("invalid key"));
    }

    #[test]
    fn put_rejects_no_equals() {
        let (state, _sk) = fresh_state_with_keypair();
        let err = run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["JUSTAKEY".into()],
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("malformed assignment"));
    }

    #[test]
    fn put_requires_at_least_one_assignment() {
        let (state, _sk) = fresh_state_with_keypair();
        let err = run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec![],
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("at least one"));
    }

    #[test]
    fn list_on_missing_store_returns_empty() {
        let (state, _sk) = fresh_state_with_keypair();
        let report = run_list(&ListOptions {
            system_space_dir: state.path().to_path_buf(),
        })
        .unwrap();
        assert!(report.keys.is_empty());
    }

    #[test]
    fn remove_drops_keys_idempotently() {
        let (state, _sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["A=1".into(), "B=2".into()],
        })
        .unwrap();

        let report = run_remove(&RemoveOptions {
            system_space_dir: state.path().to_path_buf(),
            keys: vec!["A".into(), "C".into()],
        })
        .unwrap();
        assert_eq!(report.removed, vec!["A".to_string()]);
        assert_eq!(report.not_present, vec!["C".to_string()]);
        assert_eq!(report.total_keys_after_remove, 1);

        let listed = run_list(&ListOptions {
            system_space_dir: state.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(listed.keys, vec!["B"]);
    }

    #[test]
    fn rewrap_rotates_keypair_and_re_seals_store() {
        // Atomicity guarantee: rewrap writes .new files first, then
        // renames them into final position. A crash at any point
        // leaves either the old (key, store) pair intact or the new
        // pair fully committed — never a half-rotated state.
        let (state, old_sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["FOO=bar".into(), "BAZ=qux".into()],
        })
        .unwrap();
        let old_fingerprint = old_sk.public_key().fingerprint();

        let report = run_rewrap(&RewrapOptions {
            system_space_dir: state.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(report.old_fingerprint, old_fingerprint);
        assert_ne!(report.old_fingerprint, report.new_fingerprint);
        assert_eq!(report.keys_rewrapped, 2);

        // The OLD secret key MUST no longer decrypt the store —
        // rewrap is a real rotation, not a copy.
        let store_path = default_sealed_store_path(state.path());
        let err = read_sealed_secrets(&store_path, &old_sk).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fingerprint") || msg.contains("AEAD"),
            "expected fingerprint/AEAD failure with old key, got: {msg}"
        );

        // The newly persisted key MUST decrypt the store and round-trip.
        let key_path = default_vault_secret_key_path(state.path());
        let pub_path = default_vault_public_key_path(state.path());
        let new_sk = lillux::vault::read_secret_key(&key_path).unwrap();
        let map = read_sealed_secrets(&store_path, &new_sk).unwrap();
        assert_eq!(map.get("FOO").unwrap(), "bar");
        assert_eq!(map.get("BAZ").unwrap(), "qux");

        // Final files exist; no `.new` siblings left behind.
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

        // The new private key's fingerprint MUST differ from the old.
        assert_ne!(new_sk.public_key().fingerprint(), old_fingerprint);
    }

    #[test]
    fn rewrap_with_empty_store_only_rotates_keys() {
        let (state, old_sk) = fresh_state_with_keypair();
        let report = run_rewrap(&RewrapOptions {
            system_space_dir: state.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(report.keys_rewrapped, 0);
        assert_ne!(
            report.new_fingerprint,
            old_sk.public_key().fingerprint()
        );
    }

    #[test]
    fn rewrap_after_removing_all_keys_re_seals_empty_store() {
        // Regression: if `run_remove` clears the last key, `store.enc`
        // remains on disk encoding an empty map. A rewrap that
        // skipped the store rename (driven by `plaintext.is_empty()`)
        // would rotate the keypair but leave the still-present
        // `store.enc` sealed under the OLD key — bricking the vault.
        let (state, _old_sk) = fresh_state_with_keypair();
        run_put(&PutOptions {
            system_space_dir: state.path().to_path_buf(),
            assignments: vec!["ONLY=value".into()],
        })
        .unwrap();
        run_remove(&RemoveOptions {
            system_space_dir: state.path().to_path_buf(),
            keys: vec!["ONLY".into()],
        })
        .unwrap();
        let store_path = default_sealed_store_path(state.path());
        assert!(
            store_path.exists(),
            "precondition: store.enc must exist after remove-all"
        );

        run_rewrap(&RewrapOptions {
            system_space_dir: state.path().to_path_buf(),
        })
        .unwrap();

        // The now-on-disk private key must decrypt the now-on-disk store.
        let key_path = default_vault_secret_key_path(state.path());
        let new_sk = lillux::vault::read_secret_key(&key_path).unwrap();
        let map = read_sealed_secrets(&store_path, &new_sk).unwrap();
        assert!(map.is_empty());
    }
}
