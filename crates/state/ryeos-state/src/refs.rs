//! Signed ref read/write/verify.
//!
//! One authoritative signed mutable pointer per chain:
//! `.ai/state/objects/refs/generic/chains/<chain_root_id>/head`
//!
//! Signed ref format:
//! ```json
//! {
//!   "schema": 1,
//!   "kind": "signed_ref",
//!   "ref_path": "chains/T-root/head",
//!   "target_hash": "<chain_state_hash>",
//!   "updated_at": "...",
//!   "signer": "<node-fingerprint>",
//!   "signature": "<ed25519-sig-over-canonical-json-without-signature-field>"
//! }
//! ```

use anyhow::{anyhow, Context};
use base64::Engine as _;
use lillux::crypto::Verifier;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};

use crate::signer::Signer;

const SIGNED_REF_SCHEMA: u32 = 1;
const SIGNED_REF_KIND: &str = "signed_ref";

/// A signed reference — an authoritative mutable pointer to a CAS object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRef {
    pub schema: u32,
    pub kind: String,
    pub ref_path: String,
    pub target_hash: String,
    pub updated_at: String,
    pub signer: String,
    /// Signature is computed over the object WITHOUT this field.
    pub signature: String,
}

impl SignedRef {
    /// Create a new signed ref (without signature).
    pub fn new(ref_path: String, target_hash: String, updated_at: String, signer: String) -> Self {
        Self {
            schema: SIGNED_REF_SCHEMA,
            kind: SIGNED_REF_KIND.to_string(),
            ref_path,
            target_hash,
            updated_at,
            signer,
            signature: String::new(),
        }
    }

    /// Validate the ref object structure (not signature).
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != SIGNED_REF_SCHEMA {
            anyhow::bail!(
                "invalid schema: expected {}, got {}",
                SIGNED_REF_SCHEMA,
                self.schema
            );
        }
        if self.kind != SIGNED_REF_KIND {
            anyhow::bail!(
                "invalid kind: expected {}, got {}",
                SIGNED_REF_KIND,
                self.kind
            );
        }
        if self.ref_path.is_empty() {
            anyhow::bail!("ref_path must not be empty");
        }
        if !is_canonical_hash(&self.target_hash) {
            anyhow::bail!("invalid target_hash: {}", self.target_hash);
        }
        if self.updated_at.is_empty() {
            anyhow::bail!("updated_at must not be empty");
        }
        if self.signer.is_empty() {
            anyhow::bail!("signer must not be empty");
        }
        if self.signature.is_empty() {
            anyhow::bail!("signature must not be empty");
        }
        Ok(())
    }

    /// Return a copy of this ref without the signature field (for signing/verifying).
    fn without_signature(&self) -> Value {
        json!({
            "schema": self.schema,
            "kind": self.kind,
            "ref_path": self.ref_path,
            "target_hash": self.target_hash,
            "updated_at": self.updated_at,
            "signer": self.signer,
        })
    }

    /// Convert to serde_json::Value.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Write a signed ref atomically to a file.
///
/// The signature is computed over the canonical JSON representation
/// of the ref WITHOUT the signature field.
pub fn write_signed_ref(
    path: &Path,
    mut signed_ref: SignedRef,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    // Compute signature over the ref without the signature field
    let unsigned = signed_ref.without_signature();
    let canonical = lillux::canonical_json(&unsigned)
        .context("failed to canonicalize unsigned ref")?;
    let sig_bytes = signer.sign(canonical.as_bytes());
    signed_ref.signature = base64::engine::general_purpose::STANDARD.encode(sig_bytes);

    // Validate the ref
    signed_ref.validate()?;

    // Serialize to canonical JSON
    let value = signed_ref.to_value();
    let canonical = lillux::canonical_json(&value)
        .context("failed to canonicalize signed ref")?;

    // Create parent directories
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create parent directories")?;
    }

    // Atomic write
    lillux::atomic_write(path, canonical.as_bytes()).context("failed to write signed ref")?;

    Ok(())
}

/// Read a signed ref and verify its signature against a trust store.
///
/// This is the safe variant of [`read_signed_ref`] that also validates
/// the cryptographic signature. Use this on all root-discovery paths
/// where untrusted data could be tampered with.
pub fn read_verified_ref(path: &Path, trust_store: &TrustStore) -> anyhow::Result<SignedRef> {
    let signed_ref = read_signed_ref(path)?;
    verify_signed_ref(&signed_ref, trust_store)?;
    Ok(signed_ref)
}

/// Read a signed ref from a file.
pub fn read_signed_ref(path: &Path) -> anyhow::Result<SignedRef> {
    let content = fs::read_to_string(path).context("failed to read signed ref")?;
    let value: Value = serde_json::from_str(&content).context("failed to parse signed ref JSON")?;
    let signed_ref: SignedRef =
        serde_json::from_value(value).context("failed to deserialize signed ref")?;
    signed_ref.validate()?;
    Ok(signed_ref)
}

/// Verify a signed ref's signature against a trust store.
///
/// The signature must be valid over the canonical JSON representation
/// of the ref WITHOUT the signature field, signed by the signer's key.
pub fn verify_signed_ref(
    signed_ref: &SignedRef,
    verifying_keys: &TrustStore,
) -> anyhow::Result<()> {
    signed_ref.validate()?;

    // Look up the signer's public key in the trust store
    let pubkey = verifying_keys
        .get(&signed_ref.signer)
        .ok_or_else(|| anyhow!("signer {} not in trust store", signed_ref.signer))?;

    // Reconstruct the canonical JSON without the signature
    let unsigned = signed_ref.without_signature();
    let canonical = lillux::canonical_json(&unsigned)
        .context("failed to canonicalize unsigned ref")?;

    // Decode the signature from base64
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&signed_ref.signature)
        .context("failed to decode signature")?;

    // Convert to Signature
    let signature = lillux::crypto::Signature::from_slice(&sig_bytes)
        .map_err(|e| anyhow!("failed to parse signature: {}", e))?;

    // Verify
    pubkey
        .verify(canonical.as_bytes(), &signature)
        .map_err(|e| anyhow!("signature verification failed: {}", e))?;

    Ok(())
}

/// Trust store — map of fingerprint → public key.
pub type TrustStore = std::collections::HashMap<String, lillux::crypto::VerifyingKey>;

fn is_canonical_hash(hash: &str) -> bool {
    lillux::valid_hash(hash) && !hash.bytes().any(|b| b.is_ascii_uppercase())
}

/// Canonical principal storage key — raw fingerprint hex, no `fp:` prefix.
///
/// Used for HEAD ref paths and any other per-principal filesystem keys.
/// Panics if `principal_id` doesn't start with `fp:` — every caller in
/// the daemon receives identity from `RoutePrincipal.id` which always
/// uses the `fp:` prefix.
pub fn principal_storage_key(principal_id: &str) -> &str {
    principal_id
        .strip_prefix("fp:")
        .expect("principal_id must be in fp:<hex> format")
}

/// Write a project head ref scoped to a principal.
///
/// The ref path is `projects/<principal_key>/<project_hash>/head`, so
/// different principals can push the same project path without colliding.
///
/// The `principal_key` should be the raw fingerprint hex (output of
/// [`principal_storage_key`]). The `project_hash` is derived from the
/// project path. The `project_snapshot_hash` is the CAS hash of the
/// `ProjectSnapshot` this HEAD points to.
pub fn write_project_head_ref(
    refs_root: &Path,
    principal_key: &str,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let ref_path = format!("projects/{}/{}", principal_key, project_hash);
    let signed_ref = SignedRef::new(
        ref_path.clone(),
        project_snapshot_hash.to_string(),
        lillux::time::iso8601_now(),
        signer.fingerprint().to_string(),
    );
    let path = refs_root.join(&ref_path).join("head");
    write_signed_ref(&path, signed_ref, signer)
}

/// Read a principal-scoped project head ref. Returns the target hash
/// (project snapshot hash), or `None` if no HEAD exists for this
/// principal + project combination.
pub fn read_project_head_ref(
    refs_root: &Path,
    principal_key: &str,
    project_hash: &str,
) -> anyhow::Result<Option<String>> {
    let head_path = refs_root
        .join(format!("projects/{}", principal_key))
        .join(project_hash)
        .join("head");
    if !head_path.exists() {
        return Ok(None);
    }
    let signed_ref = read_signed_ref(&head_path)?;
    Ok(Some(signed_ref.target_hash))
}

/// Advisory inter-process lock for a principal-scoped project HEAD.
///
/// The signed project HEAD helpers provide compare-and-swap semantics,
/// but without a shared lock two processes can both read the same HEAD
/// and then both write a valid advancement. Hold this guard around any
/// read/advance/write sequence for `projects/<principal>/<project>`.
pub struct ProjectHeadLock {
    _lock_file: File,
}

impl ProjectHeadLock {
    /// Acquire an exclusive lock for a principal/project HEAD.
    ///
    /// The lock path is `refs/projects/<principal_key>/<project_hash>/lock`.
    pub fn acquire(
        refs_root: &Path,
        principal_key: &str,
        project_hash: &str,
    ) -> anyhow::Result<Self> {
        let lock_path = refs_root
            .join("projects")
            .join(principal_key)
            .join(project_hash)
            .join("lock");

        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).context("failed to create project HEAD lock directory")?;
        }

        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| {
                format!("failed to open project HEAD lock: {}", lock_path.display())
            })?;

        #[cfg(unix)]
        {
            let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
            if ret != 0 {
                anyhow::bail!(
                    "project HEAD flock failed at {}: {}",
                    lock_path.display(),
                    std::io::Error::last_os_error()
                );
            }
        }

        Ok(Self {
            _lock_file: lock_file,
        })
    }
}

impl Drop for ProjectHeadLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::flock(self._lock_file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Advance a project head ref with compare-and-swap.
///
/// `expected_current_hash` must match the current HEAD target, or the
/// operation fails with a conflict error. On success, writes a new signed
/// ref pointing at `new_snapshot_hash`.
///
/// This is the project equivalent of advancing a chain head. Use it in
/// the fold-back path to prevent lost updates when multiple executions
/// race on the same project.
pub fn advance_project_head_ref(
    refs_root: &Path,
    principal_key: &str,
    project_hash: &str,
    new_snapshot_hash: &str,
    expected_current_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let current =
        read_project_head_ref(refs_root, principal_key, project_hash)?.ok_or_else(|| {
            anyhow!(
                "no project head ref for principal/project {}/{}",
                principal_key,
                project_hash
            )
        })?;

    if current != expected_current_hash {
        anyhow::bail!(
            "project head conflict for principal/project {}/{}: expected {}, got {}",
            principal_key,
            project_hash,
            expected_current_hash,
            current
        );
    }

    write_project_head_ref(
        refs_root,
        principal_key,
        project_hash,
        new_snapshot_hash,
        signer,
    )
}

/// A namespace-neutral signed head discovered under `refs/generic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericHeadRef {
    pub namespace: String,
    pub name: String,
    pub ref_path: String,
    pub target_hash: String,
    pub signer: String,
    pub updated_at: String,
    pub signed_ref: SignedRef,
}

fn validate_relative_ref_component_path(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    let path = Path::new(value);
    if path.is_absolute() {
        anyhow::bail!("{label} must be relative");
    }
    for component in path.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => {}
            _ => anyhow::bail!("{label} contains unsafe path component: {value}"),
        }
    }
    Ok(())
}

fn generic_head_ref_path(namespace: &str, name: &str) -> anyhow::Result<String> {
    validate_relative_ref_component_path("head namespace", namespace)?;
    validate_relative_ref_component_path("head name", name)?;
    Ok(format!("{namespace}/{name}/head"))
}

fn generic_head_file_path(
    refs_root: &Path,
    namespace: &str,
    name: &str,
) -> anyhow::Result<PathBuf> {
    let ref_path = generic_head_ref_path(namespace, name)?;
    Ok(refs_root.join("generic").join(ref_path))
}

/// Write a namespace-neutral signed head under `refs/generic/<namespace>/<name>/head`.
pub fn write_generic_head_ref(
    refs_root: &Path,
    namespace: &str,
    name: &str,
    target_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    if !is_canonical_hash(target_hash) {
        anyhow::bail!("invalid generic head target hash: {target_hash}");
    }
    let ref_path = generic_head_ref_path(namespace, name)?;
    let signed_ref = SignedRef::new(
        ref_path.clone(),
        target_hash.to_string(),
        lillux::time::iso8601_now(),
        signer.fingerprint().to_string(),
    );
    let path = refs_root.join("generic").join(&ref_path);
    write_signed_ref(&path, signed_ref, signer)
}

/// Read a namespace-neutral signed head.
pub fn read_generic_head_ref(
    refs_root: &Path,
    namespace: &str,
    name: &str,
) -> anyhow::Result<Option<SignedRef>> {
    let head_path = generic_head_file_path(refs_root, namespace, name)?;
    if !head_path.exists() {
        return Ok(None);
    }
    let signed_ref = read_signed_ref(&head_path)?;
    let expected_ref_path = generic_head_ref_path(namespace, name)?;
    if signed_ref.ref_path != expected_ref_path {
        anyhow::bail!(
            "generic head ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            signed_ref.ref_path
        );
    }
    Ok(Some(signed_ref))
}

/// Remove a namespace-neutral head, making its CAS closure unreachable from
/// this ref family. Missing refs are an idempotent success.
pub fn remove_generic_head_ref(
    refs_root: &Path,
    namespace: &str,
    name: &str,
) -> anyhow::Result<bool> {
    let path = generic_head_file_path(refs_root, namespace, name)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Advance a namespace-neutral signed head with compare-and-swap semantics.
///
/// `expected_current_hash = None` means the head must not exist yet.
pub fn advance_generic_head_ref(
    refs_root: &Path,
    namespace: &str,
    name: &str,
    new_target_hash: &str,
    expected_current_hash: Option<&str>,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let current = read_generic_head_ref(refs_root, namespace, name)?;
    match (current.as_ref(), expected_current_hash) {
        (None, None) => {}
        (Some(_), None) => anyhow::bail!(
            "generic head conflict for {}/{}: expected no current head",
            namespace,
            name
        ),
        (None, Some(expected)) => anyhow::bail!(
            "generic head conflict for {}/{}: expected {}, got no current head",
            namespace,
            name,
            expected
        ),
        (Some(current), Some(expected)) if current.target_hash == expected => {}
        (Some(current), Some(expected)) => anyhow::bail!(
            "generic head conflict for {}/{}: expected {}, got {}",
            namespace,
            name,
            expected,
            current.target_hash
        ),
    }

    write_generic_head_ref(refs_root, namespace, name, new_target_hash, signer)
}

/// List namespace-neutral signed heads beneath `refs/generic/<prefix>`.
pub fn list_generic_head_refs(
    refs_root: &Path,
    prefix: &str,
) -> anyhow::Result<Vec<GenericHeadRef>> {
    if !prefix.is_empty() {
        validate_relative_ref_component_path("head prefix", prefix)?;
    }
    let root = if prefix.is_empty() {
        refs_root.join("generic")
    } else {
        refs_root.join("generic").join(prefix)
    };
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut found = Vec::new();
    collect_generic_head_refs(refs_root, &root, &mut found)?;
    found.sort_by(|a, b| a.ref_path.cmp(&b.ref_path));
    Ok(found)
}

fn collect_generic_head_refs(
    refs_root: &Path,
    dir: &Path,
    found: &mut Vec<GenericHeadRef>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry.context("failed to read generic ref directory entry")?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .context("failed to inspect generic ref directory entry")?;
        if file_type.is_dir() {
            collect_generic_head_refs(refs_root, &path, found)?;
            continue;
        }
        if !file_type.is_file() || entry.file_name() != "head" {
            continue;
        }
        let signed_ref = read_signed_ref(&path)
            .with_context(|| format!("failed to read generic head {}", path.display()))?;
        let rel = path
            .strip_prefix(refs_root.join("generic"))
            .context("generic head path escaped refs root")?;
        let expected_ref_path = rel.to_string_lossy().replace('\\', "/");
        if signed_ref.ref_path != expected_ref_path {
            anyhow::bail!(
                "generic head ref_path mismatch: expected {}, got {}",
                expected_ref_path,
                signed_ref.ref_path
            );
        }
        let without_head = expected_ref_path
            .strip_suffix("/head")
            .ok_or_else(|| anyhow!("generic head path missing /head suffix"))?;
        let (namespace, name) = without_head.split_once('/').ok_or_else(|| {
            anyhow!("generic head path must contain namespace and name: {expected_ref_path}")
        })?;
        found.push(GenericHeadRef {
            namespace: namespace.to_string(),
            name: name.to_string(),
            ref_path: signed_ref.ref_path.clone(),
            target_hash: signed_ref.target_hash.clone(),
            signer: signed_ref.signer.clone(),
            updated_at: signed_ref.updated_at.clone(),
            signed_ref,
        });
    }
    Ok(())
}

/// Canonical deployed-project storage key derived from the remote live
/// project path after remote-side canonicalization.
pub fn deployed_project_key(canonical_project_path: &str) -> String {
    lillux::cas::sha256_hex(canonical_project_path.as_bytes())
}

/// Write the node-level deployed ref for a live project path.
pub fn write_deployed_project_ref(
    refs_root: &Path,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let ref_path = format!("deployed/projects/{}", project_hash);
    let signed_ref = SignedRef::new(
        ref_path.clone(),
        project_snapshot_hash.to_string(),
        lillux::time::iso8601_now(),
        signer.fingerprint().to_string(),
    );
    let path = refs_root.join(&ref_path).join("head");
    write_signed_ref(&path, signed_ref, signer)
}

/// Read the node-level deployed ref for a live project path.
pub fn read_deployed_project_ref(
    refs_root: &Path,
    project_hash: &str,
) -> anyhow::Result<Option<SignedRef>> {
    let head_path = refs_root
        .join("deployed/projects")
        .join(project_hash)
        .join("head");
    if !head_path.exists() {
        return Ok(None);
    }
    Ok(Some(read_signed_ref(&head_path)?))
}

/// Advance the node-level deployed ref with compare-and-swap.
pub fn advance_deployed_project_ref(
    refs_root: &Path,
    project_hash: &str,
    new_snapshot_hash: &str,
    expected_current_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let current = read_deployed_project_ref(refs_root, project_hash)?
        .ok_or_else(|| anyhow!("no deployed project ref for project {}", project_hash))?;

    if current.target_hash != expected_current_hash {
        anyhow::bail!(
            "deployed project conflict for project {}: expected {}, got {}",
            project_hash,
            expected_current_hash,
            current.target_hash
        );
    }

    write_deployed_project_ref(refs_root, project_hash, new_snapshot_hash, signer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;

    fn make_signed_ref() -> SignedRef {
        SignedRef::new(
            "chains/T-root/head".to_string(),
            "01".repeat(32),
            "2026-04-21T12:00:00Z".to_string(),
            "abcd1234".to_string(),
        )
    }

    #[test]
    fn signed_ref_validation_passes() {
        let mut r = make_signed_ref();
        r.signature = "valid_sig".to_string();
        assert!(r.validate().is_ok());
    }

    #[test]
    fn signed_ref_validation_rejects_bad_schema() {
        let mut r = make_signed_ref();
        r.schema = 999;
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_bad_kind() {
        let mut r = make_signed_ref();
        r.kind = "wrong_kind".to_string();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_empty_ref_path() {
        let mut r = make_signed_ref();
        r.ref_path = String::new();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_invalid_target_hash() {
        let mut r = make_signed_ref();
        r.target_hash = "not_a_hash".to_string();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_empty_updated_at() {
        let mut r = make_signed_ref();
        r.updated_at = String::new();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_empty_signer() {
        let mut r = make_signed_ref();
        r.signer = String::new();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_empty_signature() {
        let r = make_signed_ref();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_serialization_roundtrip() {
        let mut r = make_signed_ref();
        r.signature = "test_sig".to_string();
        let json = serde_json::to_string(&r).unwrap();
        let r2: SignedRef = serde_json::from_str(&json).unwrap();
        assert_eq!(r.ref_path, r2.ref_path);
        assert_eq!(r.target_hash, r2.target_hash);
        assert_eq!(r.signature, r2.signature);
    }

    #[test]
    fn signed_ref_to_value_is_valid_json() {
        let mut r = make_signed_ref();
        r.signature = "sig".to_string();
        let value = r.to_value();
        assert!(value.is_object());
        assert_eq!(value["schema"], 1);
        assert_eq!(value["kind"], "signed_ref");
    }

    #[test]
    fn signed_ref_without_signature_excludes_signature() {
        let mut r = make_signed_ref();
        r.signature = "should_be_excluded".to_string();
        let unsigned = r.without_signature();
        assert!(!unsigned.as_object().unwrap().contains_key("signature"));
    }

    #[test]
    #[should_panic(expected = "principal_id must be in fp:<hex> format")]
    fn principal_storage_key_rejects_bare_hex() {
        super::principal_storage_key("abc123");
    }

    #[test]
    fn principal_storage_key_strips_prefix() {
        assert_eq!(super::principal_storage_key("fp:abc123"), "abc123");
    }

    #[test]
    fn generic_head_write_read_advance_and_list() {
        let tempdir = tempfile::tempdir().unwrap();
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let first = "11".repeat(32);
        let second = "22".repeat(32);

        advance_generic_head_ref(
            &refs_root,
            "admissions/policy-a",
            "subject-a",
            &first,
            None,
            &signer,
        )
        .unwrap();

        let read = read_generic_head_ref(&refs_root, "admissions/policy-a", "subject-a")
            .unwrap()
            .unwrap();
        assert_eq!(read.ref_path, "admissions/policy-a/subject-a/head");
        assert_eq!(read.target_hash, first);

        let conflict = advance_generic_head_ref(
            &refs_root,
            "admissions/policy-a",
            "subject-a",
            &second,
            Some(&"33".repeat(32)),
            &signer,
        )
        .unwrap_err();
        assert!(conflict.to_string().contains("generic head conflict"));

        advance_generic_head_ref(
            &refs_root,
            "admissions/policy-a",
            "subject-a",
            &second,
            Some(&first),
            &signer,
        )
        .unwrap();

        write_generic_head_ref(
            &refs_root,
            "collections",
            "accepted/root-b",
            &"44".repeat(32),
            &signer,
        )
        .unwrap();

        let admissions = list_generic_head_refs(&refs_root, "admissions").unwrap();
        assert_eq!(admissions.len(), 1);
        assert_eq!(admissions[0].namespace, "admissions");
        assert_eq!(admissions[0].name, "policy-a/subject-a");
        assert_eq!(admissions[0].target_hash, second);

        let all = list_generic_head_refs(&refs_root, "").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].ref_path, "admissions/policy-a/subject-a/head");
        assert_eq!(all[1].ref_path, "collections/accepted/root-b/head");
    }

    #[test]
    fn generic_head_rejects_unsafe_paths() {
        let tempdir = tempfile::tempdir().unwrap();
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let target = "11".repeat(32);

        assert!(write_generic_head_ref(&refs_root, "../escape", "name", &target, &signer).is_err());
        assert!(
            write_generic_head_ref(&refs_root, "namespace", "../escape", &target, &signer).is_err()
        );
        assert!(list_generic_head_refs(&refs_root, "../escape").is_err());
    }

    #[test]
    fn write_and_read_signed_ref() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("test_ref");
        let signer = TestSigner::default();

        let signed_ref = SignedRef::new(
            "chains/T-test/head".to_string(),
            "02".repeat(32),
            "2026-04-21T13:00:00Z".to_string(),
            signer.fingerprint().to_string(),
        );

        write_signed_ref(&path, signed_ref.clone(), &signer).unwrap();
        assert!(path.exists());

        let read_ref = read_signed_ref(&path).unwrap();
        assert_eq!(read_ref.ref_path, signed_ref.ref_path);
        assert_eq!(read_ref.target_hash, signed_ref.target_hash);
        assert!(!read_ref.signature.is_empty());
    }

    #[test]
    fn write_signed_ref_creates_parent_dirs() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("deep/nested/dir/ref");
        let signer = TestSigner::default();

        let signed_ref = SignedRef::new(
            "chains/T-test/head".to_string(),
            "03".repeat(32),
            "2026-04-21T14:00:00Z".to_string(),
            signer.fingerprint().to_string(),
        );

        write_signed_ref(&path, signed_ref, &signer).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn signed_ref_canonical_json_determinism() {
        let mut r1 = make_signed_ref();
        r1.signature = "same_sig".to_string();

        let mut r2 = make_signed_ref();
        r2.signature = "same_sig".to_string();

        let json1 = lillux::canonical_json(&r1.to_value()).unwrap();
        let json2 = lillux::canonical_json(&r2.to_value()).unwrap();
        assert_eq!(json1, json2);
    }
}
