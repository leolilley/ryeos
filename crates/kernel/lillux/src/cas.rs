use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::atomic_fs::atomic_write_with_mode;

// ── Public library primitives ──────────────────────────────────────

pub fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

pub fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn shard_path(root: &Path, namespace: &str, hash: &str, ext: &str) -> PathBuf {
    root.join(namespace)
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(format!("{hash}{ext}"))
}

/// Atomically write a batch of files with a single durability barrier.
///
/// Each file is written under a hidden temporary name. One batch barrier makes
/// every complete temp durable before any target becomes visible; create-only
/// renames then publish the immutable names, and a second batch barrier makes
/// that namespace update durable. An existing target is accepted only when its
/// exact bytes match. A crash may leave a hidden temp or omit some targets, but
/// it can never leave a visible target naming partial bytes.
pub fn atomic_write_batch(writes: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    #[cfg(unix)]
    {
        atomic_write_batch_unix(None, writes)
    }
    #[cfg(not(unix))]
    {
        let _ = writes;
        anyhow::bail!("durable CAS batch writes are unavailable on this platform")
    }
}

/// Atomically write a batch beneath an already-pinned CAS root.
///
/// Every target must be lexically beneath `root.path()`. Parent directories
/// are traversed and created descriptor-relative to the retained root inode,
/// so replacing any pathname ancestor cannot rebind a multi-object commit.
pub fn atomic_write_batch_in_pinned_root(
    root: &crate::secure_fs::PinnedDirectory,
    writes: &[(PathBuf, Vec<u8>)],
) -> Result<()> {
    #[cfg(unix)]
    {
        atomic_write_batch_unix(Some(root), writes)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, writes);
        anyhow::bail!("durable CAS batch writes are unavailable on this platform")
    }
}

#[cfg(unix)]
fn atomic_write_batch_unix(
    pinned_root: Option<&crate::secure_fs::PinnedDirectory>,
    writes: &[(PathBuf, Vec<u8>)],
) -> Result<()> {
    let mut first_directory = None;
    let mut filesystem_device = None;
    let mut prepared = Vec::new();
    for (index, (target, data)) in writes.iter().enumerate() {
        let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
        let name = target.file_name().ok_or_else(|| {
            anyhow::anyhow!("CAS batch target has no file name: {}", target.display())
        })?;
        let directory = match pinned_root {
            Some(root) => open_batch_parent_in_pinned_root(root, target)?,
            None => crate::secure_fs::PinnedDirectory::open_or_create(parent_path)
                .with_context(|| format!("open CAS batch parent {}", parent_path.display()))?,
        };
        let device = directory.filesystem_device()?;
        match filesystem_device {
            None => filesystem_device = Some(device),
            Some(expected) if expected != device => {
                anyhow::bail!("CAS batch spans multiple filesystems")
            }
            Some(_) => {}
        }
        if first_directory.is_none() {
            first_directory = Some(directory.try_clone()?);
        }
        match directory
            .prepare_atomic_create(name, data, 0o644)
            .with_context(|| format!("prepare immutable CAS batch entry {}", target.display()))?
        {
            Some(temp) => prepared.push((directory, name.to_os_string(), index, temp)),
            None => verify_batch_entry(&directory, name, target, data)?,
        }
    }
    if prepared.is_empty() {
        return Ok(());
    }
    let first_directory = first_directory
        .as_ref()
        .expect("a prepared CAS batch has a first directory");

    if let Err(flush_error) = sync_write_batch(first_directory) {
        drop(prepared);
        return match sync_write_batch(first_directory) {
            Ok(()) => Err(flush_error).context("flush hidden CAS batch entries"),
            Err(cleanup_error) => Err(flush_error).context(format!(
                "flush hidden CAS batch entries; flushing temp cleanup also failed: {cleanup_error:#}"
            )),
        };
    }

    let mut publication_error = None;
    for (directory, name, index, temp) in prepared {
        if publication_error.is_some() {
            drop(temp);
            continue;
        }
        match temp.publish() {
            Ok(true) => {}
            Ok(false) => {
                let (target, data) = &writes[index];
                if let Err(error) = verify_batch_entry(&directory, &name, target, data) {
                    publication_error = Some(error);
                }
            }
            Err(error) => publication_error = Some(error),
        }
    }
    let durability = sync_write_batch(first_directory);
    match (publication_error, durability) {
        (None, Ok(())) => Ok(()),
        (Some(error), Ok(())) => Err(error).context("publish immutable CAS batch entries"),
        (None, Err(error)) => Err(error).context("make CAS batch publication durable"),
        (Some(error), Err(durability_error)) => Err(error).context(format!(
            "publish immutable CAS batch entries; publication durability also failed: {durability_error:#}"
        )),
    }
}

#[cfg(unix)]
fn open_batch_parent_in_pinned_root(
    root: &crate::secure_fs::PinnedDirectory,
    target: &Path,
) -> Result<crate::secure_fs::PinnedDirectory> {
    use std::path::Component;

    let relative = target.strip_prefix(root.path()).with_context(|| {
        format!(
            "CAS batch target {} is outside pinned root {}",
            target.display(),
            root.path().display()
        )
    })?;
    let parent = relative.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "CAS batch target has no relative parent: {}",
            target.display()
        )
    })?;
    let mut directory = root.try_clone()?;
    for component in parent.components() {
        let name = match component {
            Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!(
                    "CAS batch target has an unsafe relative path: {}",
                    target.display()
                )
            }
        };
        directory = directory
            .open_or_create_child(name, 0o777)
            .with_context(|| {
                format!(
                    "open CAS batch parent beneath pinned root for {}",
                    target.display()
                )
            })?;
    }
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn sync_write_batch(directory: &crate::secure_fs::PinnedDirectory) -> Result<()> {
    directory.sync_filesystem()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn sync_write_batch(_directory: &crate::secure_fs::PinnedDirectory) -> Result<()> {
    anyhow::bail!("batched crash-safe CAS publication requires Linux syncfs")
}

fn verify_batch_entry(
    directory: &crate::secure_fs::PinnedDirectory,
    name: &OsStr,
    target: &Path,
    data: &[u8],
) -> Result<()> {
    let file = directory
        .open_regular(name, false)?
        .ok_or_else(|| anyhow::anyhow!("CAS batch entry disappeared: {}", target.display()))?;
    let existing = read_open_file(file, target)?;
    if existing.as_slice() != data {
        anyhow::bail!(
            "immutable CAS batch entry differs from requested bytes: {}",
            target.display()
        );
    }
    Ok(())
}

fn read_open_file(mut file: fs::File, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read regular file {}", path.display()))?;
    Ok(bytes)
}

/// Materialize a blob from CAS to a target path, setting Unix permission
/// bits so the result is executable. Like `atomic_write` but preserves
/// the exec mode from the `ItemSource` record.
///
/// Unsupported platforms fail closed rather than materializing with a mode
/// that was not enforced.
pub fn materialize_executable(target: &Path, data: &[u8], mode: u32) -> Result<()> {
    atomic_write_with_mode(target, data, mode)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalJsonError;

impl std::fmt::Display for CanonicalJsonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "JSON number is not exactly representable as a finite IEEE 754 binary64 value",
        )
    }
}

impl std::error::Error for CanonicalJsonError {}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0C' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\x20' => {
                use std::fmt::Write as _;
                write!(out, "\\u{:04x}", c as u32).expect("writing to a String cannot fail");
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_number(number: &serde_json::Number, out: &mut String) -> Result<(), CanonicalJsonError> {
    let value = if let Some(value) = number.as_i64() {
        let binary64 = value as f64;
        if binary64 < i64::MIN as f64 || binary64 >= -(i64::MIN as f64) || binary64 as i64 != value
        {
            return Err(CanonicalJsonError);
        }
        binary64
    } else if let Some(value) = number.as_u64() {
        let binary64 = value as f64;
        if binary64 >= (u64::MAX as f64) + 1.0 || binary64 as u64 != value {
            return Err(CanonicalJsonError);
        }
        binary64
    } else {
        number
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or(CanonicalJsonError)?
    };

    if value == 0.0 {
        out.push('0');
        return Ok(());
    }

    let mut buffer = ryu::Buffer::new();
    let shortest = buffer.format_finite(value);
    let (sign, magnitude) = shortest
        .strip_prefix('-')
        .map_or(("", shortest), |magnitude| ("-", magnitude));
    let (mantissa, exponent) =
        magnitude
            .split_once('e')
            .map_or((magnitude, 0), |(mantissa, exponent)| {
                (
                    mantissa,
                    exponent
                        .parse::<i32>()
                        .expect("ryu emits a valid decimal exponent"),
                )
            });
    let integer_digits = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let mut digits = mantissa.replace('.', "");
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let decimal_point = integer_digits + exponent;

    out.push_str(sign);
    if decimal_point > 0 && decimal_point <= 21 {
        let decimal_point = decimal_point as usize;
        if decimal_point >= digits.len() {
            out.push_str(&digits);
            out.extend(std::iter::repeat('0').take(decimal_point - digits.len()));
        } else {
            out.push_str(&digits[..decimal_point]);
            out.push('.');
            out.push_str(&digits[decimal_point..]);
        }
    } else if decimal_point > -6 && decimal_point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat('0').take((-decimal_point) as usize));
        out.push_str(&digits);
    } else {
        out.push(digits.as_bytes()[0] as char);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        let scientific_exponent = decimal_point - 1;
        if scientific_exponent >= 0 {
            out.push('+');
        }
        use std::fmt::Write as _;
        write!(out, "{scientific_exponent}").expect("writing to a String cannot fail");
    }
    Ok(())
}

fn write_canonical_json(v: &serde_json::Value, out: &mut String) -> Result<(), CanonicalJsonError> {
    match v {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(true) => out.push_str("true"),
        serde_json::Value::Bool(false) => out.push_str("false"),
        serde_json::Value::Number(number) => write_number(number, out)?,
        serde_json::Value::String(string) => write_string(string, out),
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_canonical_json(&map[key], out)?;
            }
            out.push('}');
        }
        serde_json::Value::Array(arr) => {
            out.push('[');
            for (index, value) in arr.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(']');
        }
    }
    Ok(())
}

pub fn canonical_json(v: &serde_json::Value) -> Result<String, CanonicalJsonError> {
    let mut canonical = String::new();
    write_canonical_json(v, &mut canonical)?;
    Ok(canonical)
}

// ── CasStore ───────────────────────────────────────────────────────

pub struct CasStore {
    root: PathBuf,
    pinned_root: Option<crate::secure_fs::PinnedDirectory>,
}

/// Result of an immutable CAS publication. `created` is false only when the
/// exact verified bytes already occupied the addressed typed entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasPutOutcome {
    pub hash: String,
    pub created: bool,
}

impl CasStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            pinned_root: None,
        }
    }

    /// Bind every subsequent CAS operation to one already-open root inode.
    /// This is the authority-preserving form for operations spanning more than
    /// one object read or write.
    pub fn from_pinned_root(root: crate::secure_fs::PinnedDirectory) -> Self {
        Self {
            root: root.path().to_path_buf(),
            pinned_root: Some(root),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return whether a verified blob exists. A malformed hash is absent;
    /// namespace traversal failures and corrupt entries are errors.
    pub fn has_blob(&self, hash: &str) -> Result<bool> {
        Ok(self.get_blob(hash)?.is_some())
    }

    /// Return whether a verified, canonically encoded JSON object exists.
    /// A malformed hash is absent; authority failures are errors.
    pub fn has_object(&self, hash: &str) -> Result<bool> {
        Ok(self.get_object(hash)?.is_some())
    }

    /// Return whether the digest exists as a valid entry in either typed CAS
    /// namespace. Both namespaces are checked so corruption is not hidden by a
    /// valid entry of the other kind.
    pub fn has(&self, hash: &str) -> Result<bool> {
        let has_blob = self.has_blob(hash)?;
        let has_object = self.has_object(hash)?;
        Ok(has_blob || has_object)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        if !canonical_cas_hash(hash) {
            return Ok(None);
        }
        let Some((file, path)) =
            open_existing_entry(&self.root, self.pinned_root.as_ref(), "blobs", hash, "")?
        else {
            return Ok(None);
        };
        Ok(Some(read_verified_entry(file, hash, &path)?))
    }

    pub fn get_object(&self, hash: &str) -> Result<Option<serde_json::Value>> {
        if !canonical_cas_hash(hash) {
            return Ok(None);
        }
        let Some((file, path)) = open_existing_entry(
            &self.root,
            self.pinned_root.as_ref(),
            "objects",
            hash,
            ".json",
        )?
        else {
            return Ok(None);
        };
        let data = read_verified_entry(file, hash, &path)?;
        let value: serde_json::Value = serde_json::from_slice(&data)
            .with_context(|| format!("decode CAS object {}", path.display()))?;
        let canonical = canonical_json(&value)
            .with_context(|| format!("canonicalize CAS object {}", path.display()))?;
        if canonical.as_bytes() != data {
            anyhow::bail!("CAS object is not canonically encoded: {}", path.display());
        }
        Ok(Some(value))
    }

    pub fn store_blob(&self, data: &[u8]) -> Result<String> {
        Ok(self.put_blob(data)?.hash)
    }

    pub fn put_blob(&self, data: &[u8]) -> Result<CasPutOutcome> {
        let hash = sha256_hex(data);
        let created = store_exact_entry(
            &self.root,
            self.pinned_root.as_ref(),
            "blobs",
            &hash,
            "",
            data,
        )?;
        Ok(CasPutOutcome { hash, created })
    }

    pub fn store_object(&self, value: &serde_json::Value) -> Result<String> {
        Ok(self.put_object(value)?.hash)
    }

    pub fn put_object(&self, value: &serde_json::Value) -> Result<CasPutOutcome> {
        let json = canonical_json(value)?;
        let hash = sha256_hex(json.as_bytes());
        let created = store_exact_entry(
            &self.root,
            self.pinned_root.as_ref(),
            "objects",
            &hash,
            ".json",
            json.as_bytes(),
        )?;
        Ok(CasPutOutcome { hash, created })
    }
}

fn canonical_cas_hash(hash: &str) -> bool {
    valid_hash(hash) && !hash.bytes().any(|byte| byte.is_ascii_uppercase())
}

fn open_existing_entry(
    root_path: &Path,
    pinned_root: Option<&crate::secure_fs::PinnedDirectory>,
    namespace: &str,
    hash: &str,
    extension: &str,
) -> Result<Option<(fs::File, PathBuf)>> {
    let root = match pinned_root {
        Some(root) => root.try_clone()?,
        None => {
            let Some(root) = crate::secure_fs::PinnedDirectory::open(root_path)? else {
                return Ok(None);
            };
            root
        }
    };
    let Some(namespace_dir) = root.open_child_directory(OsStr::new(namespace))? else {
        return Ok(None);
    };
    let Some(first_shard) = namespace_dir.open_child_directory(OsStr::new(&hash[..2]))? else {
        return Ok(None);
    };
    let Some(second_shard) = first_shard.open_child_directory(OsStr::new(&hash[2..4]))? else {
        return Ok(None);
    };
    let name = OsString::from(format!("{hash}{extension}"));
    let path = second_shard.path().join(&name);
    Ok(second_shard
        .open_regular(&name, false)?
        .map(|file| (file, path)))
}

fn open_or_create_entry_parent(
    root_path: &Path,
    pinned_root: Option<&crate::secure_fs::PinnedDirectory>,
    namespace: &str,
    hash: &str,
    extension: &str,
) -> Result<(crate::secure_fs::PinnedDirectory, OsString, PathBuf)> {
    debug_assert!(canonical_cas_hash(hash));
    let root = match pinned_root {
        Some(root) => root.try_clone()?,
        None => crate::secure_fs::PinnedDirectory::open_or_create(root_path)?,
    };
    let namespace_dir = root.open_or_create_child(OsStr::new(namespace), 0o777)?;
    let first_shard = namespace_dir.open_or_create_child(OsStr::new(&hash[..2]), 0o777)?;
    let second_shard = first_shard.open_or_create_child(OsStr::new(&hash[2..4]), 0o777)?;
    let name = OsString::from(format!("{hash}{extension}"));
    let path = second_shard.path().join(&name);
    Ok((second_shard, name, path))
}

fn read_verified_entry(mut file: fs::File, expected_hash: &str, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read CAS entry {}", path.display()))?;
    let actual_hash = sha256_hex(&bytes);
    if actual_hash != expected_hash {
        anyhow::bail!(
            "CAS corruption: entry at {} hashes to {}, expected {}",
            path.display(),
            actual_hash,
            expected_hash
        );
    }
    Ok(bytes)
}

fn verify_existing_exact_entry(
    file: fs::File,
    expected_hash: &str,
    expected_bytes: &[u8],
    path: &Path,
) -> Result<()> {
    let existing = read_verified_entry(file, expected_hash, path)?;
    if existing != expected_bytes {
        anyhow::bail!(
            "CAS collision: existing entry at {} differs from the bytes addressed by {}",
            path.display(),
            expected_hash
        );
    }
    Ok(())
}

fn store_exact_entry(
    root_path: &Path,
    pinned_root: Option<&crate::secure_fs::PinnedDirectory>,
    namespace: &str,
    hash: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<bool> {
    let (parent, name, path) =
        open_or_create_entry_parent(root_path, pinned_root, namespace, hash, extension)?;
    if let Some(existing) = parent.open_regular(&name, false)? {
        verify_existing_exact_entry(existing, hash, bytes, &path)?;
        return Ok(false);
    }

    match parent.atomic_write_if_same(&name, None, bytes, 0o644) {
        Ok(()) => Ok(true),
        Err(publication_error) => match parent.open_regular(&name, false) {
            Ok(Some(existing)) => {
                verify_existing_exact_entry(existing, hash, bytes, &path).with_context(|| {
                    format!(
                        "CAS publication at {} raced with an invalid entry",
                        path.display()
                    )
                })?;
                Ok(false)
            }
            Ok(None) => Err(publication_error)
                .with_context(|| format!("publish CAS entry {}", path.display())),
            Err(verification_error) => Err(verification_error).with_context(|| {
                format!(
                    "inspect CAS entry {} after publication failed: {publication_error:#}",
                    path.display()
                )
            }),
        },
    }
}

// ── CLI interface ──────────────────────────────────────────────────

use clap::Subcommand;

#[derive(Subcommand)]
pub enum CasAction {
    /// Store content from stdin
    Store {
        #[arg(long)]
        root: String,
        #[arg(long)]
        blob: bool,
    },
    /// Fetch content by hash
    Fetch {
        #[arg(long)]
        root: String,
        #[arg(long)]
        hash: String,
        #[arg(long)]
        blob: bool,
    },
    /// Verify integrity of stored content
    Verify {
        #[arg(long)]
        root: String,
        #[arg(long)]
        hash: String,
        #[arg(long)]
        blob: bool,
    },
    /// Check if a hash exists
    Has {
        #[arg(long)]
        root: String,
        #[arg(long)]
        hash: String,
    },
}

pub fn run(action: CasAction) -> serde_json::Value {
    if matches!(
        &action,
        CasAction::Fetch { .. } | CasAction::Verify { .. } | CasAction::Has { .. }
    ) {
        let h = match &action {
            CasAction::Fetch { hash, .. }
            | CasAction::Verify { hash, .. }
            | CasAction::Has { hash, .. } => hash,
            _ => unreachable!(),
        };
        if !valid_hash(h) {
            eprintln!("invalid hash: expected 64 hex chars");
            std::process::exit(1);
        }
    }
    match action {
        CasAction::Store { root, blob } => cli_store(&root, blob),
        CasAction::Fetch { root, hash, blob } => cli_fetch(&root, &hash, blob),
        CasAction::Verify { root, hash, blob } => {
            let store = CasStore::new(PathBuf::from(&root));
            if blob {
                match store.get_blob(&hash) {
                    Ok(Some(data)) => {
                        serde_json::json!({ "valid": sha256_hex(&data) == hash, "hash": hash })
                    }
                    _ => serde_json::json!({ "valid": false, "hash": hash }),
                }
            } else {
                match store.get_object(&hash) {
                    Ok(Some(val)) => {
                        let valid = canonical_json(&val)
                            .is_ok_and(|canon| sha256_hex(canon.as_bytes()) == hash);
                        serde_json::json!({ "valid": valid, "hash": hash })
                    }
                    _ => serde_json::json!({ "valid": false, "hash": hash }),
                }
            }
        }
        CasAction::Has { root, hash } => {
            let store = CasStore::new(PathBuf::from(&root));
            match store.has(&hash) {
                Ok(exists) => serde_json::json!({ "exists": exists, "hash": hash }),
                Err(error) => serde_json::json!({ "error": error.to_string(), "hash": hash }),
            }
        }
    }
}

fn cli_store(root: &str, blob: bool) -> serde_json::Value {
    let mut data = Vec::new();
    if let Err(e) = std::io::Read::read_to_end(&mut std::io::stdin(), &mut data) {
        return serde_json::json!({ "error": format!("stdin: {e}") });
    }
    let store = CasStore::new(PathBuf::from(root));
    if blob {
        match store.store_blob(&data) {
            Ok(hash) => serde_json::json!({ "hash": hash }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        }
    } else {
        let value: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(e) => return serde_json::json!({ "error": format!("invalid JSON: {e}") }),
        };
        match store.store_object(&value) {
            Ok(hash) => serde_json::json!({ "hash": hash }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        }
    }
}

fn cli_fetch(root: &str, hash: &str, blob: bool) -> serde_json::Value {
    let store = CasStore::new(PathBuf::from(root));
    let data = if blob {
        store.get_blob(hash)
    } else {
        store.get_object(hash).and_then(|opt| {
            opt.map(|value| canonical_json(&value).map(String::into_bytes))
                .transpose()
                .map_err(Into::into)
        })
    };
    match data {
        Ok(Some(bytes)) => {
            let _ = std::io::Write::write_all(&mut std::io::stdout(), &bytes);
            std::process::exit(0);
        }
        _ => {
            eprintln!("not found");
            std::process::exit(1);
        }
    }
}
