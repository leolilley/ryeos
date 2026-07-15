use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::io::{ErrorKind, Write};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

#[cfg(unix)]
use crate::atomic_fs::next_temp_sequence;
use crate::atomic_fs::{atomic_write, atomic_write_with_mode};

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
/// Each file is written tmp+rename like [`atomic_write`], but per-file
/// fsyncs are deferred: on Linux one `syncfs` flushes the whole batch
/// (one journal commit instead of one per file); elsewhere each file is
/// fsynced in a second pass. Only use this when the batch shares one
/// downstream durability point (e.g. CAS event objects flushed before
/// the chain head that references them advances): a crash mid-batch may
/// leave some files missing or empty, which is only safe while nothing
/// references them yet.
pub fn atomic_write_batch(writes: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    #[cfg(unix)]
    {
        atomic_write_batch_unix(writes)
    }
    #[cfg(not(unix))]
    {
        let _ = writes;
        anyhow::bail!("durable CAS batch writes are unavailable on this platform")
    }
}

#[cfg(unix)]
fn atomic_write_batch_unix(writes: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    if let [(target, data)] = writes {
        atomic_write(target, data)?;
        return Ok(());
    }
    for (target, data) in writes {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut written = false;
        for _ in 0..128 {
            let sequence = next_temp_sequence();
            let tmp = target.with_extension(format!("tmp.{}.{sequence}", std::process::id()));
            let mut file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(file) => file,
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            };
            let result = (|| -> std::io::Result<()> {
                file.write_all(data)?;
                drop(file);
                fs::rename(&tmp, target)
            })();
            if let Err(err) = result {
                let _ = fs::remove_file(&tmp);
                return Err(err.into());
            }
            written = true;
            break;
        }
        if !written {
            return Err(std::io::Error::new(
                ErrorKind::AlreadyExists,
                "atomic batch temp file collision",
            )
            .into());
        }
    }
    sync_write_batch(writes)
}

#[cfg(target_os = "linux")]
fn sync_write_batch(writes: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let Some((first, _)) = writes.first() else {
        return Ok(());
    };
    let dir = fs::File::open(first.parent().unwrap_or_else(|| Path::new(".")))?;
    if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn sync_write_batch(writes: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    for (target, _) in writes {
        fs::File::open(target)?.sync_all()?;
    }
    Ok(())
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
        formatter.write_str("JSON number is not exactly representable as a finite IEEE 754 binary64 value")
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
        if binary64 < i64::MIN as f64 || binary64 >= -(i64::MIN as f64) || binary64 as i64 != value {
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
        number.as_f64().filter(|value| value.is_finite()).ok_or(CanonicalJsonError)?
    };

    if value == 0.0 {
        out.push('0');
        return Ok(());
    }

    let mut buffer = ryu::Buffer::new();
    let shortest = buffer.format_finite(value);
    let (sign, magnitude) = shortest.strip_prefix('-').map_or(("", shortest), |magnitude| ("-", magnitude));
    let (mantissa, exponent) = magnitude
        .split_once('e')
        .map_or((magnitude, 0), |(mantissa, exponent)| {
            (mantissa, exponent.parse::<i32>().expect("ryu emits a valid decimal exponent"))
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
}

impl CasStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn has_blob(&self, hash: &str) -> bool {
        valid_hash(hash) && shard_path(&self.root, "blobs", hash, "").exists()
    }

    pub fn has_object(&self, hash: &str) -> bool {
        valid_hash(hash) && shard_path(&self.root, "objects", hash, ".json").exists()
    }

    pub fn has(&self, hash: &str) -> bool {
        self.has_blob(hash) || self.has_object(hash)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        if !valid_hash(hash) {
            return Ok(None);
        }
        let path = shard_path(&self.root, "blobs", hash, "");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read(&path)?))
    }

    pub fn get_object(&self, hash: &str) -> Result<Option<serde_json::Value>> {
        if !valid_hash(hash) {
            return Ok(None);
        }
        let path = shard_path(&self.root, "objects", hash, ".json");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path)?;
        Ok(Some(serde_json::from_slice(&data)?))
    }

    pub fn store_blob(&self, data: &[u8]) -> Result<String> {
        let hash = sha256_hex(data);
        let dest = shard_path(&self.root, "blobs", &hash, "");
        if verify_existing_cas_entry(&dest, &hash, "blob")? {
            return Ok(hash);
        }
        atomic_write(&dest, data)?;
        Ok(hash)
    }

    pub fn store_object(&self, value: &serde_json::Value) -> Result<String> {
        let json = canonical_json(value)?;
        let hash = sha256_hex(json.as_bytes());
        let dest = shard_path(&self.root, "objects", &hash, ".json");
        if verify_existing_cas_entry(&dest, &hash, "object")? {
            return Ok(hash);
        }
        atomic_write(&dest, json.as_bytes())?;
        Ok(hash)
    }
}

/// Verify a pre-existing content-addressed entry before deduplicating a write.
///
/// Path existence alone is not evidence that the content at that address is
/// intact. A corrupt or substituted entry must fail closed; otherwise a
/// publisher can successfully emit a signed manifest that resolves to bytes
/// different from the hashes it commits to.
fn verify_existing_cas_entry(path: &Path, expected_hash: &str, kind: &str) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect existing CAS {kind} {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!(
            "existing CAS {kind} at {} is not a regular file",
            path.display()
        );
    }

    let bytes =
        fs::read(path).with_context(|| format!("read existing CAS {kind} {}", path.display()))?;
    let actual_hash = sha256_hex(&bytes);
    if actual_hash != expected_hash {
        bail!(
            "existing CAS {kind} integrity failure at {}: address is {}, content hashes to {}",
            path.display(),
            expected_hash,
            actual_hash
        );
    }
    Ok(true)
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
            serde_json::json!({ "exists": store.has(&hash), "hash": hash })
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
        store
            .get_object(hash)
            .and_then(|opt| {
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
