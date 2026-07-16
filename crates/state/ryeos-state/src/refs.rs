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
use std::fs::{self, File};
use std::io::Read;
#[cfg(test)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::objects::thread_snapshot::{parse_canonical_timestamp, validate_canonical_hash};
use crate::signer::Signer;

const SIGNED_REF_SCHEMA: u32 = 1;
const SIGNED_REF_KIND: &str = "signed_ref";
#[cfg(test)]
static REF_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn open_directory_path_no_follow(path: &Path, create: bool) -> anyhow::Result<Option<File>> {
    let start = if path.is_absolute() { "/" } else { "." };
    let start = std::ffi::CString::new(start).expect("static path contains no NUL");
    let descriptor = unsafe {
        libc::open(
            start.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("open ref path traversal root");
    }
    let mut directory = unsafe { File::from_raw_fd(descriptor) };
    for component in path.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!("ref path contains an unsafe component: {}", path.display())
            }
        };
        let component = std::ffi::CString::new(component.as_bytes())
            .context("ref path component contains NUL")?;
        let mut descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound && !create {
                return Ok(None);
            }
            if error.kind() != std::io::ErrorKind::NotFound || !create {
                return Err(error).with_context(|| {
                    format!(
                        "open ref directory without following links {}",
                        path.display()
                    )
                });
            }
            if unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o777) } != 0 {
                let mkdir_error = std::io::Error::last_os_error();
                if mkdir_error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(mkdir_error).with_context(|| {
                        format!(
                            "create ref directory without following links {}",
                            path.display()
                        )
                    });
                }
            }
            descriptor = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY
                        | libc::O_DIRECTORY
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | libc::O_NONBLOCK,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "open newly-created ref directory without following links {}",
                        path.display()
                    )
                });
            }
            directory
                .sync_all()
                .context("sync parent after creating ref directory")?;
        }
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok(Some(directory))
}

#[cfg(unix)]
fn open_ref_parent_no_follow(
    path: &Path,
    create_parents: bool,
) -> anyhow::Result<Option<(File, std::ffi::CString)>> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("ref path has no filename: {}", path.display()))?;
    let file_name =
        std::ffi::CString::new(file_name.as_bytes()).context("ref filename contains NUL")?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    Ok(open_directory_path_no_follow(parent, create_parents)?.map(|parent| (parent, file_name)))
}

#[cfg(unix)]
fn open_regular_ref_at(
    parent: &File,
    file_name: &std::ffi::CStr,
    path: &Path,
    writable: bool,
    create: bool,
) -> anyhow::Result<Option<File>> {
    let flags = if writable {
        libc::O_RDWR
    } else {
        libc::O_RDONLY
    } | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if create { libc::O_CREAT } else { 0 };
    let descriptor = if create {
        unsafe { libc::openat(parent.as_raw_fd(), file_name.as_ptr(), flags, 0o666) }
    } else {
        unsafe { libc::openat(parent.as_raw_fd(), file_name.as_ptr(), flags) }
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if !create && error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).with_context(|| {
            format!(
                "open regular ref without following links {}",
                path.display()
            )
        });
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!("ref path is not a regular file: {}", path.display());
    }
    Ok(Some(file))
}

#[cfg(unix)]
fn open_regular_ref_path_no_follow(
    path: &Path,
    writable: bool,
    create: bool,
    create_parents: bool,
) -> anyhow::Result<Option<File>> {
    let Some((parent, file_name)) = open_ref_parent_no_follow(path, create_parents)? else {
        return Ok(None);
    };
    open_regular_ref_at(&parent, &file_name, path, writable, create)
}

#[cfg(unix)]
fn same_file(left: &File, right: &File) -> anyhow::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(unix)]
fn open_child_ref_directory_no_follow(
    parent: &File,
    name: &std::ffi::CStr,
    display_path: &Path,
) -> anyhow::Result<Option<File>> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) {
            return Ok(None);
        }
        return Err(error).with_context(|| {
            format!(
                "open ref child directory without following links {}",
                display_path.display()
            )
        });
    }
    Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
}

#[cfg(target_os = "linux")]
fn read_ref_directory_names(directory: &File) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let mut names = Vec::new();
    for entry in fs::read_dir(&descriptor_path).with_context(|| {
        format!(
            "enumerate pinned ref directory {}",
            descriptor_path.display()
        )
    })? {
        names.push(
            entry
                .context("read pinned ref directory entry")?
                .file_name(),
        );
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn read_ref_directory_names(_directory: &File) -> anyhow::Result<Vec<std::ffi::OsString>> {
    anyhow::bail!("descriptor-relative ref enumeration is unavailable on this platform")
}

#[cfg(unix)]
#[cfg(test)]
fn atomic_write_ref_no_follow(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let (parent, file_name) = open_ref_parent_no_follow(path, true)?
        .ok_or_else(|| anyhow!("failed to create ref parent for {}", path.display()))?;
    // Reject an existing symlink or special entry before publication. A
    // regular current ref may be replaced atomically while its protocol lock
    // excludes every cooperating writer.
    let _existing = open_regular_ref_at(&parent, &file_name, path, false, false)?;

    let mut temp_name = None;
    let mut temp_file = None;
    for _ in 0..128 {
        let sequence = REF_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = std::ffi::CString::new(format!(
            ".ryeos-ref.tmp.{}.{}",
            std::process::id(),
            sequence
        ))
        .expect("generated ref temp name contains no NUL");
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                candidate.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666,
            )
        };
        if descriptor >= 0 {
            temp_name = Some(candidate);
            temp_file = Some(unsafe { File::from_raw_fd(descriptor) });
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error).with_context(|| format!("create ref temp for {}", path.display()));
        }
    }
    let temp_name = temp_name.ok_or_else(|| anyhow!("exhausted ref temp names"))?;
    let mut temp_file = temp_file.expect("temp name and file are created together");
    let result = (|| -> anyhow::Result<()> {
        temp_file.write_all(bytes)?;
        temp_file.sync_all()?;
        if unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                temp_name.as_ptr(),
                parent.as_raw_fd(),
                file_name.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("publish signed ref {}", path.display()));
        }
        parent
            .sync_all()
            .with_context(|| format!("sync signed ref parent for {}", path.display()))
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0);
        }
    }
    result
}

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
        parse_canonical_timestamp(&self.updated_at)
            .map_err(|error| anyhow!("invalid updated_at: {error}"))?;
        validate_canonical_hash("signer", &self.signer)?;
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
#[cfg(test)]
pub(crate) fn write_signed_ref(
    path: &Path,
    signed_ref: SignedRef,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let canonical = encode_signed_ref(signed_ref, signer)?;

    #[cfg(unix)]
    return atomic_write_ref_no_follow(path, &canonical).context("failed to write signed ref");
    #[cfg(not(unix))]
    anyhow::bail!("secure signed-ref publication is unavailable on this platform")
}

fn encode_signed_ref(mut signed_ref: SignedRef, signer: &dyn Signer) -> anyhow::Result<Vec<u8>> {
    // Compute signature over the ref without the signature field
    let unsigned = signed_ref.without_signature();
    let canonical =
        lillux::canonical_json(&unsigned).context("failed to canonicalize unsigned ref")?;
    let sig_bytes = signer.sign(canonical.as_bytes());
    signed_ref.signature = base64::engine::general_purpose::STANDARD.encode(sig_bytes);

    // Validate the ref
    signed_ref.validate()?;

    // Serialize to canonical JSON
    let value = signed_ref.to_value();
    Ok(lillux::canonical_json(&value)
        .context("failed to canonicalize signed ref")?
        .into_bytes())
}

/// Publish a signed ref relative to an already-pinned directory inode.
pub(crate) fn write_signed_ref_in_directory(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    signed_ref: SignedRef,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let canonical = encode_signed_ref(signed_ref, signer)?;
    let expected = directory.open_regular(name, false)?;
    directory
        .atomic_write_if_same(name, expected.as_ref(), &canonical, 0o666)
        .context("failed to write descriptor-bound signed ref")
}

fn write_signed_ref_in_directory_if_same(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    expected: Option<&File>,
    signed_ref: SignedRef,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let canonical = encode_signed_ref(signed_ref, signer)?;
    directory
        .atomic_write_if_same(name, expected, &canonical, 0o666)
        .context("failed to compare-and-write descriptor-bound signed ref")
}

fn read_verified_ref_with_file_in_directory(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<(SignedRef, File)>> {
    let Some(file) = directory.open_regular(name, false)? else {
        return Ok(None);
    };
    let signed_ref = read_signed_ref_from_file(file.try_clone()?)?;
    verify_signed_ref(&signed_ref, trust_store)?;
    Ok(Some((signed_ref, file)))
}

/// Read and trust-verify a signed ref relative to an already-pinned directory.
pub(crate) fn read_verified_ref_in_directory(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    let Some(file) = directory.open_regular(name, false)? else {
        return Ok(None);
    };
    let signed_ref = read_signed_ref_from_file(file)?;
    verify_signed_ref(&signed_ref, trust_store)?;
    Ok(Some(signed_ref))
}

/// Remove a regular ref relative to an already-pinned directory inode.
pub(crate) fn remove_ref_in_directory(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
) -> anyhow::Result<bool> {
    let Some(file) = directory.open_regular(name, false)? else {
        return Ok(false);
    };
    directory.remove_if_same(name, &file)?;
    Ok(true)
}

/// Read a signed ref and verify its signature against a trust store.
pub(crate) fn read_verified_ref(
    path: &Path,
    trust_store: &TrustStore,
) -> anyhow::Result<SignedRef> {
    let signed_ref = read_signed_ref_envelope_structural(path)?;
    verify_signed_ref(&signed_ref, trust_store)?;
    Ok(signed_ref)
}

/// Parse and structurally validate a signed-ref envelope without establishing
/// cryptographic authority. This is crate-private so it cannot be mistaken for
/// an authoritative ref read.
pub(crate) fn read_signed_ref_envelope_structural(path: &Path) -> anyhow::Result<SignedRef> {
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("secure signed-ref reading is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        let file = open_regular_ref_path_no_follow(path, false, false, false)?
            .ok_or_else(|| anyhow!("signed ref does not exist: {}", path.display()))?;
        read_signed_ref_from_file(file)
    }
}

fn read_signed_ref_from_file(mut file: File) -> anyhow::Result<SignedRef> {
    let mut content = String::new();
    file.read_to_string(&mut content)
        .context("failed to read signed ref")?;
    let value: Value = serde_json::from_str(&content).context("failed to parse signed ref JSON")?;
    let signed_ref: SignedRef =
        serde_json::from_value(value).context("failed to deserialize signed ref")?;
    signed_ref.validate()?;
    Ok(signed_ref)
}

fn ensure_ref_path_no_symlinks(
    refs_root: &Path,
    path: &Path,
    final_directory: bool,
) -> anyhow::Result<bool> {
    path_within_refs_root(refs_root, path)?;
    #[cfg(unix)]
    if final_directory {
        Ok(open_directory_path_no_follow(path, false)?.is_some())
    } else {
        Ok(open_regular_ref_path_no_follow(path, false, false, false)?.is_some())
    }
    #[cfg(not(unix))]
    anyhow::bail!("secure ref path validation is unavailable on this platform")
}

fn path_within_refs_root(refs_root: &Path, path: &Path) -> anyhow::Result<()> {
    path.strip_prefix(refs_root)
        .context("ref path escaped refs root")?;
    Ok(())
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
    let canonical =
        lillux::canonical_json(&unsigned).context("failed to canonicalize unsigned ref")?;

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

/// Prove that every top-level entry in the authoritative refs root belongs to
/// one of the closed, traversed namespaces. GC/reachability must fail closed on
/// an unknown namespace instead of silently treating its signed roots as
/// unreachable garbage.
pub(crate) fn validate_authoritative_ref_namespaces(refs_root: &Path) -> anyhow::Result<()> {
    let Some(root) = lillux::PinnedDirectory::open(refs_root)? else {
        return Ok(());
    };
    validate_authoritative_ref_namespaces_in_directory(&root)
}

pub(crate) fn validate_authoritative_ref_namespaces_in_directory(
    root: &lillux::PinnedDirectory,
) -> anyhow::Result<()> {
    for name in root.entry_names()? {
        let name_text = name
            .to_str()
            .ok_or_else(|| anyhow!("refs root contains a non-UTF8 namespace"))?;
        if !matches!(name_text, "generic" | "projects" | "deployed") {
            anyhow::bail!(
                "unknown authoritative refs namespace: {}",
                root.path().join(&name).display()
            );
        }
        if pinned_ref_entry_kind(root, &name)? != PinnedRefEntryKind::Directory {
            anyhow::bail!(
                "authoritative refs namespace is not a directory: {}",
                root.path().join(&name).display()
            );
        }
        root.open_child_directory(&name)?
            .ok_or_else(|| anyhow!("authoritative refs namespace disappeared"))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PinnedRefEntryKind {
    Directory,
    Regular,
}

fn pinned_ref_entry_kind(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
) -> anyhow::Result<PinnedRefEntryKind> {
    let descriptor_path = directory.descriptor_child_path(name)?;
    let file_type = fs::symlink_metadata(&descriptor_path)
        .with_context(|| {
            format!(
                "inspect pinned ref entry {}",
                directory.path().join(name).display()
            )
        })?
        .file_type();
    if file_type.is_symlink() {
        anyhow::bail!(
            "authoritative ref entry is a symbolic link: {}",
            directory.path().join(name).display()
        );
    }
    if file_type.is_dir() {
        return Ok(PinnedRefEntryKind::Directory);
    }
    if file_type.is_file() {
        return Ok(PinnedRefEntryKind::Regular);
    }
    anyhow::bail!(
        "authoritative ref entry is not a regular file or directory: {}",
        directory.path().join(name).display()
    )
}

fn open_ref_subdirectory<'a>(
    root: &lillux::PinnedDirectory,
    components: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<Option<lillux::PinnedDirectory>> {
    let mut directory = root.try_clone()?;
    for component in components {
        let Some(child) = directory.open_child_directory(std::ffi::OsStr::new(component))? else {
            return Ok(None);
        };
        directory = child;
    }
    Ok(Some(directory))
}

fn open_or_create_ref_subdirectory<'a>(
    root: &lillux::PinnedDirectory,
    components: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<lillux::PinnedDirectory> {
    let mut directory = root.try_clone()?;
    for component in components {
        directory = directory.open_or_create_child(std::ffi::OsStr::new(component), 0o755)?;
    }
    Ok(directory)
}

fn acquire_pinned_ref_lock(
    directory: &lillux::PinnedDirectory,
    create: bool,
    label: &str,
) -> anyhow::Result<File> {
    let name = std::ffi::OsStr::new("lock");
    let file = if create {
        let file = directory.open_regular_create(name, true, false, 0o666)?;
        directory.sync()?;
        file
    } else {
        directory.open_regular(name, true)?.ok_or_else(|| {
            anyhow!(
                "{label} lock anchor is missing: {}",
                directory.path().join(name).display()
            )
        })?
    };
    #[cfg(unix)]
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("acquire {label} lock {}", directory.path().display()));
    }
    #[cfg(not(unix))]
    anyhow::bail!("{label} locking is unavailable on this platform");
    Ok(file)
}

fn ensure_pinned_ref_lock(
    held: &File,
    directory: &lillux::PinnedDirectory,
    label: &str,
) -> anyhow::Result<()> {
    let expected = directory
        .open_regular(std::ffi::OsStr::new("lock"), false)?
        .ok_or_else(|| anyhow!("{label} lock anchor disappeared"))?;
    #[cfg(unix)]
    if !same_file(held, &expected)? {
        anyhow::bail!("{label} lock belongs to a different pinned refs namespace");
    }
    #[cfg(not(unix))]
    anyhow::bail!("{label} locking is unavailable on this platform");
    Ok(())
}

fn is_canonical_hash(hash: &str) -> bool {
    validate_canonical_hash("hash", hash).is_ok()
}

/// Canonical principal storage key — raw fingerprint hex, no `fp:` prefix.
///
/// Used for HEAD ref paths and any other per-principal filesystem keys.
/// The external identity must be exactly `fp:<64 lowercase hex>`; no
/// normalization is performed because this value selects authority-bearing
/// per-principal state.
pub fn principal_storage_key(principal_id: &str) -> anyhow::Result<&str> {
    let raw = principal_id
        .strip_prefix("fp:")
        .ok_or_else(|| anyhow!("principal id must be in fp:<64 lowercase hex> format"))?;
    if raw.len() != 64
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("principal id must be in fp:<64 lowercase hex> format");
    }
    Ok(raw)
}

fn validate_single_ref_component(label: &str, value: &str) -> anyhow::Result<()> {
    validate_relative_ref_component_path(label, value)?;
    if Path::new(value).components().count() != 1 {
        anyhow::bail!("{label} must be a single path component: {value}");
    }
    Ok(())
}

fn project_head_ref_path(principal_key: &str, project_hash: &str) -> anyhow::Result<String> {
    validate_single_ref_component("project principal key", principal_key)?;
    validate_single_ref_component("project hash", project_hash)?;
    Ok(format!("projects/{principal_key}/{project_hash}/head"))
}

fn project_head_file_path(
    refs_root: &Path,
    principal_key: &str,
    project_hash: &str,
) -> anyhow::Result<PathBuf> {
    Ok(refs_root.join(project_head_ref_path(principal_key, project_hash)?))
}

#[cfg(test)]
fn ensure_same_lock_inode(
    held_file: &File,
    expected_path: &Path,
    label: &str,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let expected = open_regular_ref_path_no_follow(expected_path, false, false, false)?
            .ok_or_else(|| {
                anyhow!(
                    "expected {label} lock does not exist: {}",
                    expected_path.display()
                )
            })?;
        if !same_file(&expected, held_file)? {
            anyhow::bail!("{label} lock belongs to a different refs root");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    anyhow::bail!("{label} locking is unavailable on this platform");
}

/// Create a project head ref scoped to a principal.
///
/// The ref path is `projects/<principal_key>/<project_hash>/head`, so
/// different principals can push the same project path without colliding.
///
/// The `principal_key` should be the raw fingerprint hex (output of
/// [`principal_storage_key`]). The `project_hash` is derived from the
/// project path. The `project_snapshot_hash` is the CAS hash of the
/// `ProjectSnapshot` this HEAD points to.
#[cfg(test)]
pub(crate) fn write_verified_project_head_ref(
    refs_root: &Path,
    principal_key: &str,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    project_lock: &ProjectHeadLock,
) -> anyhow::Result<()> {
    project_lock.ensure_protects(refs_root, principal_key, project_hash)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(project_snapshot_hash) {
        anyhow::bail!("invalid project snapshot hash: {project_snapshot_hash}");
    }
    if let Some(current) =
        read_verified_project_head_ref(refs_root, principal_key, project_hash, trust_store)?
    {
        anyhow::bail!(
            "project head conflict for principal/project {}/{}: expected no current head, got {}",
            principal_key,
            project_hash,
            current.target_hash
        );
    }
    project_lock.ensure_protects(refs_root, principal_key, project_hash)?;
    write_project_head_ref_unchecked(
        refs_root,
        principal_key,
        project_hash,
        project_snapshot_hash,
        signer,
    )
}

#[cfg(test)]
fn write_project_head_ref_unchecked(
    refs_root: &Path,
    principal_key: &str,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let ref_path = project_head_ref_path(principal_key, project_hash)?;
    let signed_ref = SignedRef::new(
        ref_path.clone(),
        project_snapshot_hash.to_string(),
        lillux::time::iso8601_now(),
        signer.fingerprint().to_string(),
    );
    let path = refs_root.join(&ref_path);
    write_signed_ref(&path, signed_ref, signer)
}

fn write_project_head_ref_unchecked_in_head_directory(
    head_directory: &lillux::PinnedDirectory,
    principal_key: &str,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
    expected: Option<&File>,
) -> anyhow::Result<()> {
    let ref_path = project_head_ref_path(principal_key, project_hash)?;
    write_signed_ref_in_directory_if_same(
        head_directory,
        std::ffi::OsStr::new("head"),
        expected,
        SignedRef::new(
            ref_path,
            project_snapshot_hash.to_string(),
            lillux::time::iso8601_now(),
            signer.fingerprint().to_string(),
        ),
        signer,
    )
}

pub(crate) fn write_verified_project_head_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    principal_key: &str,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    project_lock: &ProjectHeadLock,
) -> anyhow::Result<()> {
    let head_directory =
        project_lock.protected_directory_in_refs(refs_directory, principal_key, project_hash)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(project_snapshot_hash) {
        anyhow::bail!("invalid project snapshot hash: {project_snapshot_hash}");
    }
    let expected_ref_path = project_head_ref_path(principal_key, project_hash)?;
    if let Some((current, _current_file)) = read_verified_ref_with_file_in_directory(
        &head_directory,
        std::ffi::OsStr::new("head"),
        trust_store,
    )? {
        if current.ref_path != expected_ref_path {
            anyhow::bail!(
                "project head ref_path mismatch: expected {}, got {}",
                expected_ref_path,
                current.ref_path
            );
        }
        anyhow::bail!(
            "project head conflict for principal/project {}/{}: expected no current head, got {}",
            principal_key,
            project_hash,
            current.target_hash
        );
    }
    ensure_pinned_ref_lock(&project_lock._lock_file, &head_directory, "project HEAD")?;
    write_project_head_ref_unchecked_in_head_directory(
        &head_directory,
        principal_key,
        project_hash,
        project_snapshot_hash,
        signer,
        None,
    )
}

/// Read a trust-verified, exact-path-bound principal-scoped project head.
pub fn read_verified_project_head_ref(
    refs_root: &Path,
    principal_key: &str,
    project_hash: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    let head_path = project_head_file_path(refs_root, principal_key, project_hash)?;
    if !ensure_ref_path_no_symlinks(refs_root, &head_path, false)? {
        return Ok(None);
    }
    let signed_ref = read_verified_ref(&head_path, trust_store)?;
    let expected_ref_path = project_head_ref_path(principal_key, project_hash)?;
    if signed_ref.ref_path != expected_ref_path {
        anyhow::bail!(
            "project head ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            signed_ref.ref_path
        );
    }
    Ok(Some(signed_ref))
}

pub(crate) fn read_verified_project_head_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    principal_key: &str,
    project_hash: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    let expected_ref_path = project_head_ref_path(principal_key, project_hash)?;
    let Some(directory) =
        open_ref_subdirectory(refs_directory, ["projects", principal_key, project_hash])?
    else {
        return Ok(None);
    };
    let Some(signed_ref) =
        read_verified_ref_in_directory(&directory, std::ffi::OsStr::new("head"), trust_store)
            .with_context(|| format!("failed to verify project head {expected_ref_path}"))?
    else {
        return Ok(None);
    };
    if signed_ref.ref_path != expected_ref_path {
        anyhow::bail!(
            "project head ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            signed_ref.ref_path
        );
    }
    Ok(Some(signed_ref))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedProjectHead {
    pub principal_key: String,
    pub project_hash: String,
    pub signed_ref: SignedRef,
}

fn read_verified_closed_head_directory(
    directory: &lillux::PinnedDirectory,
    expected_ref_path: &str,
    label: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    let mut head = None;
    for name in directory.entry_names()? {
        let name_text = name
            .to_str()
            .ok_or_else(|| anyhow!("{label} entry is not valid UTF-8"))?;
        if pinned_ref_entry_kind(directory, &name)? != PinnedRefEntryKind::Regular {
            anyhow::bail!(
                "unexpected directory in {label} namespace: {}",
                directory.path().join(&name).display()
            );
        }
        match name_text {
            "lock" => {
                directory
                    .open_regular(&name, false)?
                    .ok_or_else(|| anyhow!("{label} lock disappeared"))?;
            }
            "head" => {
                head = read_verified_ref_in_directory(directory, &name, trust_store)
                    .with_context(|| format!("failed to verify {label} {expected_ref_path}"))?;
            }
            _ => anyhow::bail!(
                "unexpected regular file in {label} namespace: {}",
                directory.path().join(&name).display()
            ),
        }
    }
    let Some(head) = head else {
        return Ok(None);
    };
    if head.ref_path != expected_ref_path {
        anyhow::bail!(
            "{label} ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            head.ref_path
        );
    }
    Ok(Some(head))
}

/// Deterministically enumerate every principal-scoped project head without
/// following any symlink in the authoritative namespace.
pub fn list_verified_project_head_refs(
    refs_root: &Path,
    trust_store: &TrustStore,
) -> anyhow::Result<Vec<VerifiedProjectHead>> {
    let projects_root = refs_root.join("projects");
    path_within_refs_root(refs_root, &projects_root)?;
    #[cfg(not(unix))]
    anyhow::bail!("secure project-ref enumeration is unavailable on this platform");
    #[cfg(unix)]
    let Some(projects_directory) = open_directory_path_no_follow(&projects_root, false)?
    else {
        return Ok(Vec::new());
    };
    let mut heads = Vec::new();
    for principal_name in read_ref_directory_names(&projects_directory)? {
        let principal_key = principal_name.into_string().map_err(|name| {
            anyhow!(
                "project principal ref entry is not valid UTF-8: {}",
                Path::new(&name).display()
            )
        })?;
        validate_single_ref_component("project principal key", &principal_key)?;
        let principal_path = projects_root.join(&principal_key);
        let principal_name = std::ffi::CString::new(principal_key.as_bytes())?;
        let Some(principal_directory) = open_child_ref_directory_no_follow(
            &projects_directory,
            &principal_name,
            &principal_path,
        )?
        else {
            // Opening as a regular file distinguishes an ignorable file from
            // a symlink or special entry, which fails closed.
            open_regular_ref_at(
                &projects_directory,
                &principal_name,
                &principal_path,
                false,
                false,
            )?;
            continue;
        };
        for project_name in read_ref_directory_names(&principal_directory)? {
            let project_hash = project_name.into_string().map_err(|name| {
                anyhow!(
                    "project ref entry is not valid UTF-8: {}",
                    Path::new(&name).display()
                )
            })?;
            project_head_ref_path(&principal_key, &project_hash)?;
            let project_path = principal_path.join(&project_hash);
            let project_name = std::ffi::CString::new(project_hash.as_bytes())?;
            let Some(project_directory) = open_child_ref_directory_no_follow(
                &principal_directory,
                &project_name,
                &project_path,
            )?
            else {
                open_regular_ref_at(
                    &principal_directory,
                    &project_name,
                    &project_path,
                    false,
                    false,
                )?;
                continue;
            };
            let mut signed_ref = None;
            for child_name in read_ref_directory_names(&project_directory)? {
                let child_path = project_path.join(&child_name);
                let child_name_c = std::ffi::CString::new(child_name.as_bytes())?;
                if open_child_ref_directory_no_follow(
                    &project_directory,
                    &child_name_c,
                    &child_path,
                )?
                .is_some()
                {
                    anyhow::bail!(
                        "unexpected directory in project head namespace: {}",
                        child_path.display()
                    );
                }
                let file = open_regular_ref_at(
                    &project_directory,
                    &child_name_c,
                    &child_path,
                    false,
                    false,
                )?
                .ok_or_else(|| anyhow!("project head entry disappeared"))?;
                if child_name == "head" {
                    signed_ref = Some(read_signed_ref_from_file(file)?);
                }
            }
            if let Some(signed_ref) = signed_ref {
                verify_signed_ref(&signed_ref, trust_store)?;
                let expected_ref_path = project_head_ref_path(&principal_key, &project_hash)?;
                if signed_ref.ref_path != expected_ref_path {
                    anyhow::bail!(
                        "project head ref_path mismatch: expected {}, got {}",
                        expected_ref_path,
                        signed_ref.ref_path
                    );
                }
                heads.push(VerifiedProjectHead {
                    principal_key: principal_key.clone(),
                    project_hash,
                    signed_ref,
                });
            }
        }
    }
    heads.sort_by(|left, right| {
        (&left.principal_key, &left.project_hash).cmp(&(&right.principal_key, &right.project_hash))
    });
    Ok(heads)
}

pub(crate) fn list_verified_project_head_refs_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    trust_store: &TrustStore,
) -> anyhow::Result<Vec<VerifiedProjectHead>> {
    let Some(projects) = open_ref_subdirectory(refs_directory, ["projects"])? else {
        return Ok(Vec::new());
    };
    let mut heads = Vec::new();
    for principal_name in projects.entry_names()? {
        let principal_key = principal_name
            .to_str()
            .ok_or_else(|| anyhow!("project principal ref entry is not valid UTF-8"))?;
        validate_single_ref_component("project principal key", principal_key)?;
        if pinned_ref_entry_kind(&projects, &principal_name)? != PinnedRefEntryKind::Directory {
            anyhow::bail!(
                "project principal ref entry is not a directory: {}",
                projects.path().join(&principal_name).display()
            );
        }
        let principal = projects
            .open_child_directory(&principal_name)?
            .ok_or_else(|| anyhow!("project principal directory disappeared"))?;
        for project_name in principal.entry_names()? {
            let project_hash = project_name
                .to_str()
                .ok_or_else(|| anyhow!("project ref entry is not valid UTF-8"))?;
            let expected_ref_path = project_head_ref_path(principal_key, project_hash)?;
            if pinned_ref_entry_kind(&principal, &project_name)? != PinnedRefEntryKind::Directory {
                anyhow::bail!(
                    "project ref entry is not a directory: {}",
                    principal.path().join(&project_name).display()
                );
            }
            let project = principal
                .open_child_directory(&project_name)?
                .ok_or_else(|| anyhow!("project head directory disappeared"))?;
            if let Some(signed_ref) = read_verified_closed_head_directory(
                &project,
                &expected_ref_path,
                "project head",
                trust_store,
            )? {
                heads.push(VerifiedProjectHead {
                    principal_key: principal_key.to_string(),
                    project_hash: project_hash.to_string(),
                    signed_ref,
                });
            }
        }
    }
    heads.sort_by(|left, right| {
        (&left.principal_key, &left.project_hash).cmp(&(&right.principal_key, &right.project_hash))
    });
    Ok(heads)
}

/// Mandatory inter-process mutation lock for a principal-scoped project HEAD.
///
/// Mutation helpers require this exact guard and prove its inode belongs to the
/// supplied refs root before comparing or publishing.
pub struct ProjectHeadLock {
    _lock_file: File,
    principal_key: String,
    project_hash: String,
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
        Self::acquire_inner(refs_root, principal_key, project_hash, true)
    }

    /// Acquire an existing project-head lock without creating its directory or
    /// anchor. Mutation-free compaction dry-runs use the anchor established by
    /// the current project-head publication path.
    pub fn acquire_existing(
        refs_root: &Path,
        principal_key: &str,
        project_hash: &str,
    ) -> anyhow::Result<Self> {
        Self::acquire_inner(refs_root, principal_key, project_hash, false)
    }

    pub(crate) fn acquire_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        principal_key: &str,
        project_hash: &str,
    ) -> anyhow::Result<Self> {
        Self::acquire_inner_in_refs_directory(refs_directory, principal_key, project_hash, true)
    }

    pub(crate) fn acquire_existing_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        principal_key: &str,
        project_hash: &str,
    ) -> anyhow::Result<Self> {
        Self::acquire_inner_in_refs_directory(refs_directory, principal_key, project_hash, false)
    }

    fn acquire_inner_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        principal_key: &str,
        project_hash: &str,
        create: bool,
    ) -> anyhow::Result<Self> {
        project_head_ref_path(principal_key, project_hash)?;
        let directory = if create {
            open_or_create_ref_subdirectory(
                refs_directory,
                ["projects", principal_key, project_hash],
            )?
        } else {
            open_ref_subdirectory(refs_directory, ["projects", principal_key, project_hash])?
                .ok_or_else(|| anyhow!("project HEAD lock directory is missing"))?
        };
        let lock_file = acquire_pinned_ref_lock(&directory, create, "project HEAD")?;
        Ok(Self {
            _lock_file: lock_file,
            principal_key: principal_key.to_string(),
            project_hash: project_hash.to_string(),
        })
    }

    fn acquire_inner(
        refs_root: &Path,
        principal_key: &str,
        project_hash: &str,
        create: bool,
    ) -> anyhow::Result<Self> {
        project_head_ref_path(principal_key, project_hash)?;
        let lock_path = refs_root
            .join("projects")
            .join(principal_key)
            .join(project_hash)
            .join("lock");

        #[cfg(unix)]
        let lock_file = open_regular_ref_path_no_follow(&lock_path, true, create, create)?
            .ok_or_else(|| {
                anyhow!(
                    "project HEAD lock anchor is missing: {}",
                    lock_path.display()
                )
            })?;
        #[cfg(not(unix))]
        let lock_file: File = anyhow::bail!("project HEAD locking is unavailable on this platform");

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
            principal_key: principal_key.to_string(),
            project_hash: project_hash.to_string(),
        })
    }

    #[cfg(test)]
    fn ensure_protects(
        &self,
        refs_root: &Path,
        principal_key: &str,
        project_hash: &str,
    ) -> anyhow::Result<()> {
        if self.principal_key != principal_key || self.project_hash != project_hash {
            anyhow::bail!(
                "project HEAD lock protects {}/{}, not {}/{}",
                self.principal_key,
                self.project_hash,
                principal_key,
                project_hash
            );
        }
        let expected_path = refs_root
            .join("projects")
            .join(principal_key)
            .join(project_hash)
            .join("lock");
        ensure_same_lock_inode(&self._lock_file, &expected_path, "project HEAD")
    }

    fn protected_directory_in_refs(
        &self,
        refs_directory: &lillux::PinnedDirectory,
        principal_key: &str,
        project_hash: &str,
    ) -> anyhow::Result<lillux::PinnedDirectory> {
        if self.principal_key != principal_key || self.project_hash != project_hash {
            anyhow::bail!(
                "project HEAD lock protects {}/{}, not {}/{}",
                self.principal_key,
                self.project_hash,
                principal_key,
                project_hash
            );
        }
        let directory =
            open_ref_subdirectory(refs_directory, ["projects", principal_key, project_hash])?
                .ok_or_else(|| anyhow!("project HEAD lock directory disappeared"))?;
        ensure_pinned_ref_lock(&self._lock_file, &directory, "project HEAD")?;
        Ok(directory)
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

pub(crate) struct DeployedProjectHeadLock {
    _lock_file: File,
    project_hash: String,
}

impl DeployedProjectHeadLock {
    pub(crate) fn acquire_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        project_hash: &str,
    ) -> anyhow::Result<Self> {
        Self::acquire_inner_in_refs_directory(refs_directory, project_hash, true)
    }

    fn acquire_inner_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        project_hash: &str,
        create: bool,
    ) -> anyhow::Result<Self> {
        deployed_project_ref_path(project_hash)?;
        let directory = if create {
            open_or_create_ref_subdirectory(refs_directory, ["deployed", "projects", project_hash])?
        } else {
            open_ref_subdirectory(refs_directory, ["deployed", "projects", project_hash])?
                .ok_or_else(|| anyhow!("deployed-project HEAD lock directory is missing"))?
        };
        let lock_file = acquire_pinned_ref_lock(&directory, create, "deployed-project HEAD")?;
        Ok(Self {
            _lock_file: lock_file,
            project_hash: project_hash.to_string(),
        })
    }

    fn protected_directory_in_refs(
        &self,
        refs_directory: &lillux::PinnedDirectory,
        project_hash: &str,
    ) -> anyhow::Result<lillux::PinnedDirectory> {
        if self.project_hash != project_hash {
            anyhow::bail!(
                "deployed-project HEAD lock protects {}, not {}",
                self.project_hash,
                project_hash
            );
        }
        let directory =
            open_ref_subdirectory(refs_directory, ["deployed", "projects", project_hash])?
                .ok_or_else(|| anyhow!("deployed-project HEAD lock directory disappeared"))?;
        ensure_pinned_ref_lock(&self._lock_file, &directory, "deployed-project HEAD")?;
        Ok(directory)
    }
}

impl Drop for DeployedProjectHeadLock {
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
#[cfg(test)]
// This test-only CAS adapter deliberately exposes every identity, trust, and
// held-lock prerequisite independently, matching the production boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn advance_verified_project_head_ref(
    refs_root: &Path,
    principal_key: &str,
    project_hash: &str,
    new_snapshot_hash: &str,
    expected_current_hash: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    project_lock: &ProjectHeadLock,
) -> anyhow::Result<()> {
    project_lock.ensure_protects(refs_root, principal_key, project_hash)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    let current =
        read_verified_project_head_ref(refs_root, principal_key, project_hash, trust_store)?
            .ok_or_else(|| {
                anyhow!(
                    "no project head ref for principal/project {}/{}",
                    principal_key,
                    project_hash
                )
            })?;

    if current.target_hash != expected_current_hash {
        anyhow::bail!(
            "project head conflict for principal/project {}/{}: expected {}, got {}",
            principal_key,
            project_hash,
            expected_current_hash,
            current.target_hash
        );
    }

    project_lock.ensure_protects(refs_root, principal_key, project_hash)?;
    write_project_head_ref_unchecked(
        refs_root,
        principal_key,
        project_hash,
        new_snapshot_hash,
        signer,
    )
}

// Signed-ref authority, CAS expectation, trust, and the held lock are explicit
// so callers cannot conflate verification with publication authority.
#[allow(clippy::too_many_arguments)]
pub(crate) fn advance_verified_project_head_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    principal_key: &str,
    project_hash: &str,
    new_snapshot_hash: &str,
    expected_current_hash: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    project_lock: &ProjectHeadLock,
) -> anyhow::Result<()> {
    let head_directory =
        project_lock.protected_directory_in_refs(refs_directory, principal_key, project_hash)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(new_snapshot_hash) {
        anyhow::bail!("invalid project snapshot hash: {new_snapshot_hash}");
    }
    let expected_ref_path = project_head_ref_path(principal_key, project_hash)?;
    let (current, current_file) = read_verified_ref_with_file_in_directory(
        &head_directory,
        std::ffi::OsStr::new("head"),
        trust_store,
    )?
    .ok_or_else(|| {
        anyhow!(
            "no project head ref for principal/project {}/{}",
            principal_key,
            project_hash
        )
    })?;
    if current.ref_path != expected_ref_path {
        anyhow::bail!(
            "project head ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            current.ref_path
        );
    }
    if current.target_hash != expected_current_hash {
        anyhow::bail!(
            "project head conflict for principal/project {}/{}: expected {}, got {}",
            principal_key,
            project_hash,
            expected_current_hash,
            current.target_hash
        );
    }
    ensure_pinned_ref_lock(&project_lock._lock_file, &head_directory, "project HEAD")?;
    write_project_head_ref_unchecked_in_head_directory(
        &head_directory,
        principal_key,
        project_hash,
        new_snapshot_hash,
        signer,
        Some(&current_file),
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
    if value.contains('\\') || value.bytes().any(|byte| byte.is_ascii_control()) {
        anyhow::bail!("{label} contains a non-canonical path character: {value:?}");
    }
    if value
        .split('/')
        .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        anyhow::bail!("{label} contains a non-canonical path component: {value}");
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

pub(crate) struct GenericHeadLock {
    _lock_file: File,
    namespace: String,
    name: String,
}

impl GenericHeadLock {
    #[cfg(test)]
    pub(crate) fn acquire(refs_root: &Path, namespace: &str, name: &str) -> anyhow::Result<Self> {
        generic_head_ref_path(namespace, name)?;
        let lock_path = refs_root
            .join("generic")
            .join(namespace)
            .join(name)
            .join("lock");
        #[cfg(unix)]
        let lock_file = open_regular_ref_path_no_follow(&lock_path, true, true, true)?
            .expect("create=true opens generic HEAD lock");
        #[cfg(not(unix))]
        let lock_file: File = anyhow::bail!("generic HEAD locking is unavailable on this platform");
        #[cfg(unix)]
        {
            let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
            if ret != 0 {
                anyhow::bail!(
                    "generic HEAD flock failed at {}: {}",
                    lock_path.display(),
                    std::io::Error::last_os_error()
                );
            }
        }
        Ok(Self {
            _lock_file: lock_file,
            namespace: namespace.to_string(),
            name: name.to_string(),
        })
    }

    pub(crate) fn acquire_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        namespace: &str,
        name: &str,
    ) -> anyhow::Result<Self> {
        Self::acquire_inner_in_refs_directory(refs_directory, namespace, name, true)
    }

    fn acquire_inner_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        namespace: &str,
        name: &str,
        create: bool,
    ) -> anyhow::Result<Self> {
        generic_head_ref_path(namespace, name)?;
        let components = std::iter::once("generic")
            .chain(namespace.split('/'))
            .chain(name.split('/'));
        let directory = if create {
            open_or_create_ref_subdirectory(refs_directory, components)?
        } else {
            open_ref_subdirectory(refs_directory, components)?
                .ok_or_else(|| anyhow!("generic HEAD lock directory is missing"))?
        };
        let lock_file = acquire_pinned_ref_lock(&directory, create, "generic HEAD")?;
        Ok(Self {
            _lock_file: lock_file,
            namespace: namespace.to_string(),
            name: name.to_string(),
        })
    }

    #[cfg(test)]
    fn ensure_protects(&self, refs_root: &Path, namespace: &str, name: &str) -> anyhow::Result<()> {
        if self.namespace != namespace || self.name != name {
            anyhow::bail!(
                "generic HEAD lock protects {}/{}, not {}/{}",
                self.namespace,
                self.name,
                namespace,
                name
            );
        }
        let expected_path = refs_root
            .join("generic")
            .join(namespace)
            .join(name)
            .join("lock");
        ensure_same_lock_inode(&self._lock_file, &expected_path, "generic HEAD")
    }

    fn protected_directory_in_refs(
        &self,
        refs_directory: &lillux::PinnedDirectory,
        namespace: &str,
        name: &str,
    ) -> anyhow::Result<lillux::PinnedDirectory> {
        if self.namespace != namespace || self.name != name {
            anyhow::bail!(
                "generic HEAD lock protects {}/{}, not {}/{}",
                self.namespace,
                self.name,
                namespace,
                name
            );
        }
        let directory = open_ref_subdirectory(
            refs_directory,
            std::iter::once("generic")
                .chain(namespace.split('/'))
                .chain(name.split('/')),
        )?
        .ok_or_else(|| anyhow!("generic HEAD lock directory disappeared"))?;
        ensure_pinned_ref_lock(&self._lock_file, &directory, "generic HEAD")?;
        Ok(directory)
    }
}

impl Drop for GenericHeadLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::flock(self._lock_file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Create a namespace-neutral signed head after proving the exact lock and
/// signer belong to the caller's authority.
#[cfg(test)]
pub(crate) fn write_verified_generic_head_ref(
    refs_root: &Path,
    namespace: &str,
    name: &str,
    target_hash: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_lock: &GenericHeadLock,
) -> anyhow::Result<()> {
    head_lock.ensure_protects(refs_root, namespace, name)?;
    if namespace == "chains" {
        anyhow::bail!(
            "chain heads must be published through StateDb so projection recovery is completed"
        );
    }
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(target_hash) {
        anyhow::bail!("invalid generic head target hash: {target_hash}");
    }
    // Validate both path components before creating a lock path.
    generic_head_ref_path(namespace, name)?;
    if let Some(current) = read_verified_generic_head_ref(refs_root, namespace, name, trust_store)?
    {
        anyhow::bail!(
            "generic head conflict for {}/{}: expected no current head, got {}",
            namespace,
            name,
            current.target_hash
        );
    }
    head_lock.ensure_protects(refs_root, namespace, name)?;
    write_generic_head_ref_unchecked(refs_root, namespace, name, target_hash, signer)
}

/// Publish a generic envelope without independently establishing writer or
/// predecessor authority. Callers must already hold the protocol-specific
/// lock and have completed those proofs.
#[cfg(test)]
pub(crate) fn write_generic_head_ref_unchecked(
    refs_root: &Path,
    namespace: &str,
    name: &str,
    target_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
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

fn write_generic_head_ref_unchecked_in_head_directory(
    head_directory: &lillux::PinnedDirectory,
    namespace: &str,
    name: &str,
    target_hash: &str,
    signer: &dyn Signer,
    expected: Option<&File>,
) -> anyhow::Result<()> {
    let ref_path = generic_head_ref_path(namespace, name)?;
    write_signed_ref_in_directory_if_same(
        head_directory,
        std::ffi::OsStr::new("head"),
        expected,
        SignedRef::new(
            ref_path,
            target_hash.to_string(),
            lillux::time::iso8601_now(),
            signer.fingerprint().to_string(),
        ),
        signer,
    )
}

pub(crate) fn write_verified_generic_head_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    namespace: &str,
    name: &str,
    target_hash: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_lock: &GenericHeadLock,
) -> anyhow::Result<()> {
    let head_directory = head_lock.protected_directory_in_refs(refs_directory, namespace, name)?;
    if namespace == "chains" {
        anyhow::bail!(
            "chain heads must be published through StateDb so projection recovery is completed"
        );
    }
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(target_hash) {
        anyhow::bail!("invalid generic head target hash: {target_hash}");
    }
    let expected_ref_path = generic_head_ref_path(namespace, name)?;
    if let Some((current, _current_file)) = read_verified_ref_with_file_in_directory(
        &head_directory,
        std::ffi::OsStr::new("head"),
        trust_store,
    )? {
        if current.ref_path != expected_ref_path {
            anyhow::bail!(
                "generic head ref_path mismatch: expected {}, got {}",
                expected_ref_path,
                current.ref_path
            );
        }
        anyhow::bail!(
            "generic head conflict for {}/{}: expected no current head, got {}",
            namespace,
            name,
            current.target_hash
        );
    }
    ensure_pinned_ref_lock(&head_lock._lock_file, &head_directory, "generic HEAD")?;
    write_generic_head_ref_unchecked_in_head_directory(
        &head_directory,
        namespace,
        name,
        target_hash,
        signer,
        None,
    )
}

/// Read a namespace-neutral head and verify its signature against trusted keys.
pub fn read_verified_generic_head_ref(
    refs_root: &Path,
    namespace: &str,
    name: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    let head_path = generic_head_file_path(refs_root, namespace, name)?;
    if !ensure_ref_path_no_symlinks(refs_root, &head_path, false)? {
        return Ok(None);
    }
    let signed_ref = read_verified_ref(&head_path, trust_store)?;
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

/// Read one namespace-neutral head relative to the exact refs-root inode
/// already selected by StateDb. Missing directories or a missing head are
/// absence; links, special files, malformed envelopes, and authority changes
/// fail closed.
pub(crate) fn read_verified_generic_head_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    namespace: &str,
    name: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    let expected_ref_path = generic_head_ref_path(namespace, name)?;
    let Some(mut directory) =
        refs_directory.open_child_directory(std::ffi::OsStr::new("generic"))?
    else {
        return Ok(None);
    };
    for component in namespace.split('/').chain(name.split('/')) {
        let Some(child) = directory.open_child_directory(std::ffi::OsStr::new(component))? else {
            return Ok(None);
        };
        directory = child;
    }
    let Some(signed_ref) =
        read_verified_ref_in_directory(&directory, std::ffi::OsStr::new("head"), trust_store)
            .with_context(|| format!("failed to verify generic head {expected_ref_path}"))?
    else {
        return Ok(None);
    };
    if signed_ref.ref_path != expected_ref_path {
        anyhow::bail!(
            "generic head ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            signed_ref.ref_path
        );
    }
    Ok(Some(signed_ref))
}

#[cfg(test)]
pub(crate) fn remove_generic_head_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    namespace: &str,
    name: &str,
    head_lock: &GenericHeadLock,
) -> anyhow::Result<bool> {
    generic_head_ref_path(namespace, name)?;
    let head_directory = head_lock.protected_directory_in_refs(refs_directory, namespace, name)?;
    ensure_pinned_ref_lock(&head_lock._lock_file, &head_directory, "generic HEAD")?;
    remove_ref_in_directory(&head_directory, std::ffi::OsStr::new("head"))
}

/// Advance a namespace-neutral signed head with compare-and-swap semantics.
///
/// `expected_current_hash = None` means the head must not exist yet.
#[cfg(test)]
// Keep namespace identity, CAS expectation, trust, and lock authority explicit
// in tests so invalid combinations remain directly constructible.
#[allow(clippy::too_many_arguments)]
pub(crate) fn advance_verified_generic_head_ref(
    refs_root: &Path,
    namespace: &str,
    name: &str,
    new_target_hash: &str,
    expected_current_hash: Option<&str>,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_lock: &GenericHeadLock,
) -> anyhow::Result<()> {
    head_lock.ensure_protects(refs_root, namespace, name)?;
    if namespace == "chains" {
        anyhow::bail!(
            "chain heads must be advanced through StateDb so projection recovery is completed"
        );
    }
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(new_target_hash) {
        anyhow::bail!("invalid generic head target hash: {new_target_hash}");
    }
    let current = read_verified_generic_head_ref(refs_root, namespace, name, trust_store)?;
    validate_generic_head_cas(namespace, name, current.as_ref(), expected_current_hash)?;
    head_lock.ensure_protects(refs_root, namespace, name)?;
    write_generic_head_ref_unchecked(refs_root, namespace, name, new_target_hash, signer)
}

// Signed-ref authority, CAS expectation, trust, and the held lock are explicit
// so callers cannot conflate verification with publication authority.
#[allow(clippy::too_many_arguments)]
pub(crate) fn advance_verified_generic_head_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    namespace: &str,
    name: &str,
    new_target_hash: &str,
    expected_current_hash: Option<&str>,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_lock: &GenericHeadLock,
) -> anyhow::Result<()> {
    let head_directory = head_lock.protected_directory_in_refs(refs_directory, namespace, name)?;
    if namespace == "chains" {
        anyhow::bail!(
            "chain heads must be advanced through StateDb so projection recovery is completed"
        );
    }
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(new_target_hash) {
        anyhow::bail!("invalid generic head target hash: {new_target_hash}");
    }
    let expected_ref_path = generic_head_ref_path(namespace, name)?;
    let current = read_verified_ref_with_file_in_directory(
        &head_directory,
        std::ffi::OsStr::new("head"),
        trust_store,
    )?;
    if let Some((current, _current_file)) = current.as_ref() {
        if current.ref_path != expected_ref_path {
            anyhow::bail!(
                "generic head ref_path mismatch: expected {}, got {}",
                expected_ref_path,
                current.ref_path
            );
        }
    }
    validate_generic_head_cas(
        namespace,
        name,
        current.as_ref().map(|(signed_ref, _)| signed_ref),
        expected_current_hash,
    )?;
    ensure_pinned_ref_lock(&head_lock._lock_file, &head_directory, "generic HEAD")?;
    write_generic_head_ref_unchecked_in_head_directory(
        &head_directory,
        namespace,
        name,
        new_target_hash,
        signer,
        current.as_ref().map(|(_, file)| file),
    )
}

fn validate_generic_head_cas(
    namespace: &str,
    name: &str,
    current: Option<&SignedRef>,
    expected_current_hash: Option<&str>,
) -> anyhow::Result<()> {
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
    Ok(())
}

/// List and cryptographically verify namespace-neutral heads beneath a prefix.
pub fn list_verified_generic_head_refs(
    refs_root: &Path,
    prefix: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Vec<GenericHeadRef>> {
    let heads = list_generic_head_envelopes_structural(refs_root, prefix)?;
    for head in &heads {
        verify_signed_ref(&head.signed_ref, trust_store)
            .with_context(|| format!("failed to verify generic head {}", head.ref_path))?;
    }
    Ok(heads)
}

/// Deterministically enumerate and verify namespace-neutral heads beneath a
/// prefix from an already-pinned refs root. Every component is classified and
/// opened relative to its retained parent directory; there is no pathname
/// fallback if the visible refs tree is renamed or replaced.
pub(crate) fn list_verified_generic_head_refs_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    prefix: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Vec<GenericHeadRef>> {
    if !prefix.is_empty() {
        validate_relative_ref_component_path("head prefix", prefix)?;
    }
    let Some(mut directory) =
        refs_directory.open_child_directory(std::ffi::OsStr::new("generic"))?
    else {
        return Ok(Vec::new());
    };
    let mut components = Vec::new();
    if !prefix.is_empty() {
        for component in prefix.split('/') {
            let Some(child) = directory.open_child_directory(std::ffi::OsStr::new(component))?
            else {
                return Ok(Vec::new());
            };
            directory = child;
            components.push(component.to_string());
        }
    }

    let mut found = Vec::new();
    collect_verified_generic_heads_in_directory(
        &directory,
        &mut components,
        trust_store,
        &mut found,
    )?;
    found.sort_by(|left, right| left.ref_path.cmp(&right.ref_path));
    Ok(found)
}

fn collect_verified_generic_heads_in_directory(
    directory: &lillux::PinnedDirectory,
    components: &mut Vec<String>,
    trust_store: &TrustStore,
    found: &mut Vec<GenericHeadRef>,
) -> anyhow::Result<()> {
    for entry_name in directory.entry_names()? {
        let entry = entry_name.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "generic ref entry name is not valid UTF-8: {}",
                directory.path().join(&entry_name).display()
            )
        })?;
        if pinned_ref_entry_kind(directory, &entry_name)? == PinnedRefEntryKind::Directory {
            let child = directory
                .open_child_directory(&entry_name)?
                .ok_or_else(|| anyhow::anyhow!("generic ref directory disappeared"))?;
            components.push(entry.to_string());
            collect_verified_generic_heads_in_directory(&child, components, trust_store, found)?;
            components.pop();
            continue;
        }
        if entry == "lock" {
            // Per-head lock anchors are mutable coordination state, not refs.
            directory
                .open_regular(&entry_name, false)?
                .ok_or_else(|| anyhow::anyhow!("generic ref lock disappeared"))?;
            continue;
        }
        if entry != "head" {
            anyhow::bail!(
                "unexpected regular file in generic ref namespace: {}",
                directory.path().join(&entry_name).display()
            );
        }
        if components.len() < 2 {
            anyhow::bail!("generic head path must contain namespace and name");
        }
        let namespace = components[0].clone();
        let name = components[1..].join("/");
        let expected_ref_path = generic_head_ref_path(&namespace, &name)?;
        let signed_ref = read_verified_ref_in_directory(directory, &entry_name, trust_store)
            .with_context(|| format!("failed to verify generic head {expected_ref_path}"))?
            .ok_or_else(|| anyhow::anyhow!("generic head disappeared during enumeration"))?;
        if signed_ref.ref_path != expected_ref_path {
            anyhow::bail!(
                "generic head ref_path mismatch: expected {}, got {}",
                expected_ref_path,
                signed_ref.ref_path
            );
        }
        found.push(GenericHeadRef {
            namespace,
            name,
            ref_path: expected_ref_path,
            target_hash: signed_ref.target_hash.clone(),
            signer: signed_ref.signer.clone(),
            updated_at: signed_ref.updated_at.clone(),
            signed_ref,
        });
    }
    Ok(())
}

/// Enumerate authoritative chain heads from the exact refs-root inode already
/// selected by StateDb. This deliberately supports only the chain namespace:
/// projection recovery must not turn a pinned root back into a pathname walk.
pub(crate) fn list_verified_chain_heads_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    trust_store: &TrustStore,
) -> anyhow::Result<Vec<GenericHeadRef>> {
    list_verified_generic_head_refs_in_directory(refs_directory, "chains", trust_store)
}

/// Structurally enumerate generic envelopes without treating them as
/// authoritative. This is intentionally crate-private and unmistakably named
/// for rebuild/repair code that performs its own trust proof.
pub(crate) fn list_generic_head_envelopes_structural(
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
    path_within_refs_root(refs_root, &root)?;
    #[cfg(not(unix))]
    anyhow::bail!("secure generic-ref enumeration is unavailable on this platform");
    #[cfg(unix)]
    let Some(root_directory) = open_directory_path_no_follow(&root, false)?
    else {
        return Ok(Vec::new());
    };

    let mut found = Vec::new();
    collect_generic_head_envelopes_structural(refs_root, &root, &root_directory, &mut found)?;
    found.sort_by(|a, b| a.ref_path.cmp(&b.ref_path));
    Ok(found)
}

fn collect_generic_head_envelopes_structural(
    refs_root: &Path,
    dir: &Path,
    directory: &File,
    found: &mut Vec<GenericHeadRef>,
) -> anyhow::Result<()> {
    for entry_name in read_ref_directory_names(directory)? {
        let path = dir.join(&entry_name);
        let entry_name_c = std::ffi::CString::new(entry_name.as_bytes())?;
        if let Some(child_directory) =
            open_child_ref_directory_no_follow(directory, &entry_name_c, &path)?
        {
            collect_generic_head_envelopes_structural(refs_root, &path, &child_directory, found)?;
            continue;
        }
        let file = open_regular_ref_at(directory, &entry_name_c, &path, false, false)?
            .ok_or_else(|| anyhow!("generic ref entry disappeared: {}", path.display()))?;
        if entry_name == "lock" {
            continue;
        }
        if entry_name != "head" {
            anyhow::bail!(
                "unexpected regular file in generic ref namespace: {}",
                path.display()
            );
        }
        let signed_ref = read_signed_ref_from_file(file)
            .with_context(|| format!("failed to read generic head {}", path.display()))?;
        let rel = path
            .strip_prefix(refs_root.join("generic"))
            .context("generic head path escaped refs root")?;
        let expected_ref_path = rel
            .to_str()
            .ok_or_else(|| anyhow!("generic head path is not valid UTF-8: {}", path.display()))?
            .replace('\\', "/");
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
        let canonical_ref_path = generic_head_ref_path(namespace, name)?;
        if canonical_ref_path != expected_ref_path {
            anyhow::bail!(
                "generic head path is not canonical: expected {}, got {}",
                canonical_ref_path,
                expected_ref_path
            );
        }
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

fn deployed_project_ref_path(project_hash: &str) -> anyhow::Result<String> {
    validate_single_ref_component("deployed project hash", project_hash)?;
    Ok(format!("deployed/projects/{project_hash}/head"))
}

fn deployed_project_file_path(refs_root: &Path, project_hash: &str) -> anyhow::Result<PathBuf> {
    Ok(refs_root.join(deployed_project_ref_path(project_hash)?))
}

fn write_deployed_project_ref_unchecked_in_head_directory(
    head_directory: &lillux::PinnedDirectory,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
    expected: Option<&File>,
) -> anyhow::Result<()> {
    let ref_path = deployed_project_ref_path(project_hash)?;
    write_signed_ref_in_directory_if_same(
        head_directory,
        std::ffi::OsStr::new("head"),
        expected,
        SignedRef::new(
            ref_path,
            project_snapshot_hash.to_string(),
            lillux::time::iso8601_now(),
            signer.fingerprint().to_string(),
        ),
        signer,
    )
}

pub(crate) fn write_verified_deployed_project_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    project_lock: &DeployedProjectHeadLock,
) -> anyhow::Result<()> {
    let head_directory = project_lock.protected_directory_in_refs(refs_directory, project_hash)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(project_snapshot_hash) {
        anyhow::bail!("invalid deployed project snapshot hash: {project_snapshot_hash}");
    }
    let expected_ref_path = deployed_project_ref_path(project_hash)?;
    if let Some((current, _current_file)) = read_verified_ref_with_file_in_directory(
        &head_directory,
        std::ffi::OsStr::new("head"),
        trust_store,
    )? {
        if current.ref_path != expected_ref_path {
            anyhow::bail!(
                "deployed project ref_path mismatch: expected {}, got {}",
                expected_ref_path,
                current.ref_path
            );
        }
        anyhow::bail!(
            "deployed project conflict for project {}: expected no current head, got {}",
            project_hash,
            current.target_hash
        );
    }
    ensure_pinned_ref_lock(
        &project_lock._lock_file,
        &head_directory,
        "deployed-project HEAD",
    )?;
    write_deployed_project_ref_unchecked_in_head_directory(
        &head_directory,
        project_hash,
        project_snapshot_hash,
        signer,
        None,
    )
}

/// Read the trust-verified, exact-path-bound deployed ref for a live project.
pub fn read_verified_deployed_project_ref(
    refs_root: &Path,
    project_hash: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    let head_path = deployed_project_file_path(refs_root, project_hash)?;
    if !ensure_ref_path_no_symlinks(refs_root, &head_path, false)? {
        return Ok(None);
    }
    let signed_ref = read_verified_ref(&head_path, trust_store)?;
    let expected_ref_path = deployed_project_ref_path(project_hash)?;
    if signed_ref.ref_path != expected_ref_path {
        anyhow::bail!(
            "deployed project ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            signed_ref.ref_path
        );
    }
    Ok(Some(signed_ref))
}

pub(crate) fn read_verified_deployed_project_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    project_hash: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    let expected_ref_path = deployed_project_ref_path(project_hash)?;
    let Some(directory) =
        open_ref_subdirectory(refs_directory, ["deployed", "projects", project_hash])?
    else {
        return Ok(None);
    };
    let Some(signed_ref) =
        read_verified_ref_in_directory(&directory, std::ffi::OsStr::new("head"), trust_store)
            .with_context(|| {
                format!("failed to verify deployed project head {expected_ref_path}")
            })?
    else {
        return Ok(None);
    };
    if signed_ref.ref_path != expected_ref_path {
        anyhow::bail!(
            "deployed project ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            signed_ref.ref_path
        );
    }
    Ok(Some(signed_ref))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDeployedProjectHead {
    pub project_hash: String,
    pub signed_ref: SignedRef,
}

/// Deterministically enumerate deployed-project heads without following any
/// symlink in the authoritative namespace.
pub fn list_verified_deployed_project_refs(
    refs_root: &Path,
    trust_store: &TrustStore,
) -> anyhow::Result<Vec<VerifiedDeployedProjectHead>> {
    let projects_root = refs_root.join("deployed/projects");
    path_within_refs_root(refs_root, &projects_root)?;
    #[cfg(not(unix))]
    anyhow::bail!("secure deployed-ref enumeration is unavailable on this platform");
    #[cfg(unix)]
    let Some(projects_directory) = open_directory_path_no_follow(&projects_root, false)?
    else {
        return Ok(Vec::new());
    };
    let mut heads = Vec::new();
    for project_name in read_ref_directory_names(&projects_directory)? {
        let project_hash = project_name.into_string().map_err(|name| {
            anyhow!(
                "deployed project ref entry is not valid UTF-8: {}",
                Path::new(&name).display()
            )
        })?;
        deployed_project_ref_path(&project_hash)?;
        let project_path = projects_root.join(&project_hash);
        let project_name = std::ffi::CString::new(project_hash.as_bytes())?;
        let Some(project_directory) =
            open_child_ref_directory_no_follow(&projects_directory, &project_name, &project_path)?
        else {
            open_regular_ref_at(
                &projects_directory,
                &project_name,
                &project_path,
                false,
                false,
            )?;
            continue;
        };
        let mut signed_ref = None;
        for child_name in read_ref_directory_names(&project_directory)? {
            let child_path = project_path.join(&child_name);
            let child_name_c = std::ffi::CString::new(child_name.as_bytes())?;
            if open_child_ref_directory_no_follow(&project_directory, &child_name_c, &child_path)?
                .is_some()
            {
                anyhow::bail!(
                    "unexpected directory in deployed-project head namespace: {}",
                    child_path.display()
                );
            }
            let file =
                open_regular_ref_at(&project_directory, &child_name_c, &child_path, false, false)?
                    .ok_or_else(|| anyhow!("deployed project head entry disappeared"))?;
            if child_name == "head" {
                signed_ref = Some(read_signed_ref_from_file(file)?);
            }
        }
        if let Some(signed_ref) = signed_ref {
            verify_signed_ref(&signed_ref, trust_store)?;
            let expected_ref_path = deployed_project_ref_path(&project_hash)?;
            if signed_ref.ref_path != expected_ref_path {
                anyhow::bail!(
                    "deployed project ref_path mismatch: expected {}, got {}",
                    expected_ref_path,
                    signed_ref.ref_path
                );
            }
            heads.push(VerifiedDeployedProjectHead {
                project_hash,
                signed_ref,
            });
        }
    }
    heads.sort_by(|left, right| left.project_hash.cmp(&right.project_hash));
    Ok(heads)
}

pub(crate) fn list_verified_deployed_project_refs_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    trust_store: &TrustStore,
) -> anyhow::Result<Vec<VerifiedDeployedProjectHead>> {
    let Some(projects) = open_ref_subdirectory(refs_directory, ["deployed", "projects"])? else {
        return Ok(Vec::new());
    };
    let mut heads = Vec::new();
    for project_name in projects.entry_names()? {
        let project_hash = project_name
            .to_str()
            .ok_or_else(|| anyhow!("deployed project ref entry is not valid UTF-8"))?;
        let expected_ref_path = deployed_project_ref_path(project_hash)?;
        if pinned_ref_entry_kind(&projects, &project_name)? != PinnedRefEntryKind::Directory {
            anyhow::bail!(
                "deployed project ref entry is not a directory: {}",
                projects.path().join(&project_name).display()
            );
        }
        let project = projects
            .open_child_directory(&project_name)?
            .ok_or_else(|| anyhow!("deployed project head directory disappeared"))?;
        if let Some(signed_ref) = read_verified_closed_head_directory(
            &project,
            &expected_ref_path,
            "deployed project head",
            trust_store,
        )? {
            heads.push(VerifiedDeployedProjectHead {
                project_hash: project_hash.to_string(),
                signed_ref,
            });
        }
    }
    heads.sort_by(|left, right| left.project_hash.cmp(&right.project_hash));
    Ok(heads)
}

pub(crate) fn advance_verified_deployed_project_ref_in_directory(
    refs_directory: &lillux::PinnedDirectory,
    project_hash: &str,
    new_snapshot_hash: &str,
    expected_current_hash: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    project_lock: &DeployedProjectHeadLock,
) -> anyhow::Result<()> {
    let head_directory = project_lock.protected_directory_in_refs(refs_directory, project_hash)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if !is_canonical_hash(new_snapshot_hash) {
        anyhow::bail!("invalid deployed project snapshot hash: {new_snapshot_hash}");
    }
    let expected_ref_path = deployed_project_ref_path(project_hash)?;
    let (current, current_file) = read_verified_ref_with_file_in_directory(
        &head_directory,
        std::ffi::OsStr::new("head"),
        trust_store,
    )?
    .ok_or_else(|| anyhow!("no deployed project ref for project {}", project_hash))?;
    if current.ref_path != expected_ref_path {
        anyhow::bail!(
            "deployed project ref_path mismatch: expected {}, got {}",
            expected_ref_path,
            current.ref_path
        );
    }
    if current.target_hash != expected_current_hash {
        anyhow::bail!(
            "deployed project conflict for project {}: expected {}, got {}",
            project_hash,
            expected_current_hash,
            current.target_hash
        );
    }
    ensure_pinned_ref_lock(
        &project_lock._lock_file,
        &head_directory,
        "deployed-project HEAD",
    )?;
    write_deployed_project_ref_unchecked_in_head_directory(
        &head_directory,
        project_hash,
        new_snapshot_hash,
        signer,
        Some(&current_file),
    )
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
            "ab".repeat(32),
        )
    }

    fn trust_store(signer: &TestSigner) -> TrustStore {
        let mut trust = TrustStore::new();
        trust.insert(signer.fingerprint().to_string(), signer.verifying_key());
        trust
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
    fn signed_ref_validation_rejects_noncanonical_timestamp_and_signer() {
        let mut r = make_signed_ref();
        r.updated_at = "2026-04-21T12:00:00.000Z".to_string();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());

        r.updated_at = "2026-04-21T12:00:00Z".to_string();
        r.signer = "AA".repeat(32);
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
    fn principal_storage_key_rejects_bare_hex() {
        assert!(super::principal_storage_key("abc123").is_err());
    }

    #[test]
    fn principal_storage_key_accepts_only_canonical_fingerprint() {
        let raw = "ab".repeat(32);
        assert_eq!(
            super::principal_storage_key(&format!("fp:{raw}")).unwrap(),
            raw
        );
        assert!(super::principal_storage_key(&format!("fp:{}", "AB".repeat(32))).is_err());
        assert!(super::principal_storage_key("fp:abc123").is_err());
    }

    #[test]
    fn project_head_requires_trust_exact_path_and_exact_lock_root() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_refs = first.path().join("refs");
        let second_refs = second.path().join("refs");
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let principal = "principal-a";
        let project = "project-a";
        let first_hash = "11".repeat(32);
        let second_hash = "22".repeat(32);
        let first_lock = ProjectHeadLock::acquire(&first_refs, principal, project).unwrap();

        write_verified_project_head_ref(
            &first_refs,
            principal,
            project,
            &first_hash,
            &signer,
            &trust,
            &first_lock,
        )
        .unwrap();
        let head = read_verified_project_head_ref(&first_refs, principal, project, &trust)
            .unwrap()
            .unwrap();
        assert_eq!(head.ref_path, "projects/principal-a/project-a/head");

        let wrong_root_lock = ProjectHeadLock::acquire(&second_refs, principal, project).unwrap();
        let wrong_root = advance_verified_project_head_ref(
            &first_refs,
            principal,
            project,
            &second_hash,
            &first_hash,
            &signer,
            &trust,
            &wrong_root_lock,
        )
        .unwrap_err();
        assert!(wrong_root.to_string().contains("different refs root"));

        let mismatched = SignedRef::new(
            "projects/principal-a/other-project/head".to_string(),
            first_hash,
            lillux::time::iso8601_now(),
            signer.fingerprint().to_string(),
        );
        let head_path = project_head_file_path(&first_refs, principal, project).unwrap();
        write_signed_ref(&head_path, mismatched, &signer).unwrap();
        let mismatch =
            read_verified_project_head_ref(&first_refs, principal, project, &trust).unwrap_err();
        assert!(mismatch.to_string().contains("ref_path mismatch"));
    }

    #[test]
    fn generic_head_write_read_advance_and_list() {
        let tempdir = tempfile::tempdir().unwrap();
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let first = "11".repeat(32);
        let second = "22".repeat(32);
        let admissions_lock =
            GenericHeadLock::acquire(&refs_root, "admissions/policy-a", "subject-a").unwrap();

        advance_verified_generic_head_ref(
            &refs_root,
            "admissions/policy-a",
            "subject-a",
            &first,
            None,
            &signer,
            &trust,
            &admissions_lock,
        )
        .unwrap();

        let read =
            read_verified_generic_head_ref(&refs_root, "admissions/policy-a", "subject-a", &trust)
                .unwrap()
                .unwrap();
        assert_eq!(read.ref_path, "admissions/policy-a/subject-a/head");
        assert_eq!(read.target_hash, first);

        let conflict = advance_verified_generic_head_ref(
            &refs_root,
            "admissions/policy-a",
            "subject-a",
            &second,
            Some(&"33".repeat(32)),
            &signer,
            &trust,
            &admissions_lock,
        )
        .unwrap_err();
        assert!(conflict.to_string().contains("generic head conflict"));

        advance_verified_generic_head_ref(
            &refs_root,
            "admissions/policy-a",
            "subject-a",
            &second,
            Some(&first),
            &signer,
            &trust,
            &admissions_lock,
        )
        .unwrap();

        let collections_lock =
            GenericHeadLock::acquire(&refs_root, "collections", "accepted/root-b").unwrap();
        write_verified_generic_head_ref(
            &refs_root,
            "collections",
            "accepted/root-b",
            &"44".repeat(32),
            &signer,
            &trust,
            &collections_lock,
        )
        .unwrap();

        let admissions = list_verified_generic_head_refs(&refs_root, "admissions", &trust).unwrap();
        assert_eq!(admissions.len(), 1);
        assert_eq!(admissions[0].namespace, "admissions");
        assert_eq!(admissions[0].name, "policy-a/subject-a");
        assert_eq!(admissions[0].target_hash, second);

        let all = list_verified_generic_head_refs(&refs_root, "", &trust).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].ref_path, "admissions/policy-a/subject-a/head");
        assert_eq!(all[1].ref_path, "collections/accepted/root-b/head");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_generic_reads_do_not_rebind_and_reject_untyped_entries() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().unwrap();
        let refs_root = tempdir.path().join("refs");
        let displaced_refs = tempdir.path().join("refs.displaced");
        let outside = tempdir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let original_target = "11".repeat(32);
        let replacement_target = "22".repeat(32);

        let original_lock =
            GenericHeadLock::acquire(&refs_root, "admissions/policy", "subject").unwrap();
        write_verified_generic_head_ref(
            &refs_root,
            "admissions/policy",
            "subject",
            &original_target,
            &signer,
            &trust,
            &original_lock,
        )
        .unwrap();
        drop(original_lock);
        let pinned = lillux::PinnedDirectory::open(&refs_root)
            .unwrap()
            .expect("refs root exists");

        fs::rename(&refs_root, &displaced_refs).unwrap();
        let replacement_lock =
            GenericHeadLock::acquire(&refs_root, "admissions/policy", "subject").unwrap();
        write_verified_generic_head_ref(
            &refs_root,
            "admissions/policy",
            "subject",
            &replacement_target,
            &signer,
            &trust,
            &replacement_lock,
        )
        .unwrap();

        let read = read_verified_generic_head_ref_in_directory(
            &pinned,
            "admissions/policy",
            "subject",
            &trust,
        )
        .unwrap()
        .unwrap();
        assert_eq!(read.target_hash, original_target);
        let listed =
            list_verified_generic_head_refs_in_directory(&pinned, "admissions", &trust).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].target_hash, original_target);
        assert_eq!(
            read_verified_generic_head_ref(&refs_root, "admissions/policy", "subject", &trust,)
                .unwrap()
                .unwrap()
                .target_hash,
            replacement_target
        );

        let bad_entry = displaced_refs.join("generic/admissions/bad-entry");
        symlink(&outside, &bad_entry).unwrap();
        assert!(
            list_verified_generic_head_refs_in_directory(&pinned, "admissions", &trust).is_err()
        );
        fs::remove_file(&bad_entry).unwrap();
        let fifo = std::ffi::CString::new(bad_entry.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            list_verified_generic_head_refs_in_directory(&pinned, "admissions", &trust).is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_non_chain_locks_and_mutations_stay_on_displaced_refs_root() {
        let tempdir = tempfile::tempdir().unwrap();
        let refs_root = tempdir.path().join("refs");
        let displaced_refs = tempdir.path().join("refs.displaced");
        fs::create_dir(&refs_root).unwrap();
        let refs_directory = lillux::PinnedDirectory::open(&refs_root)
            .unwrap()
            .expect("refs root exists");
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let first = "11".repeat(32);
        let second = "22".repeat(32);

        let project_lock =
            ProjectHeadLock::acquire_in_refs_directory(&refs_directory, "principal-a", "project-a")
                .unwrap();
        write_verified_project_head_ref_in_directory(
            &refs_directory,
            "principal-a",
            "project-a",
            &first,
            &signer,
            &trust,
            &project_lock,
        )
        .unwrap();
        let deployed_lock =
            DeployedProjectHeadLock::acquire_in_refs_directory(&refs_directory, "project-a")
                .unwrap();
        write_verified_deployed_project_ref_in_directory(
            &refs_directory,
            "project-a",
            &first,
            &signer,
            &trust,
            &deployed_lock,
        )
        .unwrap();
        let generic_lock = GenericHeadLock::acquire_in_refs_directory(
            &refs_directory,
            "admissions/policy",
            "subject",
        )
        .unwrap();
        write_verified_generic_head_ref_in_directory(
            &refs_directory,
            "admissions/policy",
            "subject",
            &first,
            &signer,
            &trust,
            &generic_lock,
        )
        .unwrap();

        fs::rename(&refs_root, &displaced_refs).unwrap();
        fs::create_dir(&refs_root).unwrap();
        let replacement = lillux::PinnedDirectory::open(&refs_root)
            .unwrap()
            .expect("replacement refs root exists");
        let replacement_lock = GenericHeadLock::acquire_in_refs_directory(
            &replacement,
            "admissions/policy",
            "subject",
        )
        .unwrap();
        assert!(advance_verified_generic_head_ref_in_directory(
            &refs_directory,
            "admissions/policy",
            "subject",
            &second,
            Some(&first),
            &signer,
            &trust,
            &replacement_lock,
        )
        .is_err());

        advance_verified_project_head_ref_in_directory(
            &refs_directory,
            "principal-a",
            "project-a",
            &second,
            &first,
            &signer,
            &trust,
            &project_lock,
        )
        .unwrap();
        advance_verified_deployed_project_ref_in_directory(
            &refs_directory,
            "project-a",
            &second,
            &first,
            &signer,
            &trust,
            &deployed_lock,
        )
        .unwrap();
        advance_verified_generic_head_ref_in_directory(
            &refs_directory,
            "admissions/policy",
            "subject",
            &second,
            Some(&first),
            &signer,
            &trust,
            &generic_lock,
        )
        .unwrap();

        assert_eq!(
            read_verified_project_head_ref_in_directory(
                &refs_directory,
                "principal-a",
                "project-a",
                &trust,
            )
            .unwrap()
            .unwrap()
            .target_hash,
            second
        );
        assert_eq!(
            read_verified_deployed_project_ref_in_directory(&refs_directory, "project-a", &trust,)
                .unwrap()
                .unwrap()
                .target_hash,
            second
        );
        assert_eq!(
            read_verified_generic_head_ref_in_directory(
                &refs_directory,
                "admissions/policy",
                "subject",
                &trust,
            )
            .unwrap()
            .unwrap()
            .target_hash,
            second
        );
        assert_eq!(
            list_verified_project_head_refs_in_directory(&refs_directory, &trust)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_verified_deployed_project_refs_in_directory(&refs_directory, &trust)
                .unwrap()
                .len(),
            1
        );
        validate_authoritative_ref_namespaces_in_directory(&refs_directory).unwrap();
        assert!(remove_generic_head_ref_in_directory(
            &refs_directory,
            "admissions/policy",
            "subject",
            &generic_lock,
        )
        .unwrap());
        assert!(read_verified_generic_head_ref_in_directory(
            &refs_directory,
            "admissions/policy",
            "subject",
            &trust,
        )
        .unwrap()
        .is_none());

        assert!(read_verified_project_head_ref_in_directory(
            &replacement,
            "principal-a",
            "project-a",
            &trust,
        )
        .unwrap()
        .is_none());
        assert!(
            read_verified_deployed_project_ref_in_directory(&replacement, "project-a", &trust,)
                .unwrap()
                .is_none()
        );
        assert!(read_verified_generic_head_ref_in_directory(
            &replacement,
            "admissions/policy",
            "subject",
            &trust,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn generic_head_rejects_unsafe_paths() {
        let tempdir = tempfile::tempdir().unwrap();
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let target = "11".repeat(32);
        let valid_lock = GenericHeadLock::acquire(&refs_root, "namespace", "name").unwrap();

        assert!(write_verified_generic_head_ref(
            &refs_root,
            "../escape",
            "name",
            &target,
            &signer,
            &trust,
            &valid_lock,
        )
        .is_err());
        assert!(write_verified_generic_head_ref(
            &refs_root,
            "namespace",
            "../escape",
            &target,
            &signer,
            &trust,
            &valid_lock,
        )
        .is_err());
        assert!(list_verified_generic_head_refs(&refs_root, "../escape", &trust).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn authoritative_ref_enumerators_fail_closed_on_symlink_entries() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().unwrap();
        let outside = tempdir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let trust = TrustStore::new();

        let generic_refs = tempdir.path().join("generic-refs");
        fs::create_dir_all(generic_refs.join("generic")).unwrap();
        symlink(&outside, generic_refs.join("generic/admissions")).unwrap();
        assert!(list_verified_generic_head_refs(&generic_refs, "", &trust).is_err());

        let project_refs = tempdir.path().join("project-refs");
        fs::create_dir_all(project_refs.join("projects")).unwrap();
        symlink(&outside, project_refs.join("projects/principal-a")).unwrap();
        assert!(list_verified_project_head_refs(&project_refs, &trust).is_err());

        let deployed_refs = tempdir.path().join("deployed-refs");
        fs::create_dir_all(deployed_refs.join("deployed/projects")).unwrap();
        symlink(&outside, deployed_refs.join("deployed/projects/project-a")).unwrap();
        assert!(list_verified_deployed_project_refs(&deployed_refs, &trust).is_err());
    }

    #[test]
    fn generic_advance_rejects_untrusted_current_envelope() {
        let tempdir = tempfile::tempdir().unwrap();
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let trust = trust_store(&signer);
        let head_lock =
            GenericHeadLock::acquire(&refs_root, "admissions/policy", "subject").unwrap();
        let first = "11".repeat(32);
        advance_verified_generic_head_ref(
            &refs_root,
            "admissions/policy",
            "subject",
            &first,
            None,
            &signer,
            &trust,
            &head_lock,
        )
        .unwrap();

        let head_path = generic_head_file_path(&refs_root, "admissions/policy", "subject").unwrap();
        let mut tampered = read_signed_ref_envelope_structural(&head_path).unwrap();
        tampered.signature = "AAAA".to_string();
        let encoded = lillux::canonical_json(&tampered.to_value()).unwrap();
        lillux::atomic_write(&head_path, encoded.as_bytes()).unwrap();

        let error = advance_verified_generic_head_ref(
            &refs_root,
            "admissions/policy",
            "subject",
            &"22".repeat(32),
            Some(&first),
            &signer,
            &trust,
            &head_lock,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("signature"));
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

        let read_ref = read_signed_ref_envelope_structural(&path).unwrap();
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
