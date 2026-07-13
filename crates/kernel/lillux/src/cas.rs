use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use sha2::{Digest, Sha256};

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

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Hold an interprocess lock associated with `target` for the entire operation.
/// Callers performing read-modify-write must place the read inside this scope.
pub fn with_exclusive_file_lock<T>(
    target: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let file_name = target.file_name().ok_or_else(|| {
            std::io::Error::new(ErrorKind::InvalidInput, "lock target has no file name")
        })?;
        let lock_path = parent.join(format!(".{}.lock", file_name.to_string_lossy()));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = operation();
        drop(lock);
        return result;
    }

    #[cfg(not(unix))]
    operation()
}

pub fn atomic_write(target: &Path, data: &[u8]) -> Result<()> {
    atomic_write_portable(target, data, None)
}

/// Atomically replace `target` with private data.
///
/// On Unix the temporary file is created as `0600` before any bytes are
/// written, so secret material is never briefly exposed under a permissive
/// mode. The file and containing directory are synced before success returns.
pub fn atomic_write_private(target: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        return atomic_write_private_unix(target, data);
    }
    #[cfg(not(unix))]
    {
        atomic_write_portable(target, data, None)
    }
}

/// Remove a file and durably record the directory update. Missing files are
/// already in the requested state.
pub fn remove_file_durable(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            sync_parent_dir(path)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn atomic_write_portable(target: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    #[cfg(not(unix))]
    let _ = mode;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut last_collision = None;
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = target.with_extension(format!("tmp.{}.{sequence}", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }

        let mut file = match options.open(&tmp) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                last_collision = Some(err);
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        let write_result = (|| -> std::io::Result<()> {
            file.write_all(data)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, target)?;
            sync_parent_dir(target)?;
            Ok(())
        })();
        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(err.into());
        }
        return Ok(());
    }

    Err(last_collision
        .unwrap_or_else(|| std::io::Error::new(ErrorKind::AlreadyExists, "temp file collision"))
        .into())
}

/// Private atomic replacement relative to an already-open final parent.
///
/// Ancestor symlinks remain supported for app-root compatibility, but the
/// final parent itself must be a real directory. Holding its descriptor across
/// create, rename, and fsync prevents a concurrent parent swap from redirecting
/// secret material.
#[cfg(unix)]
fn atomic_write_private_unix(target: &Path, data: &[u8]) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let parent_c = CString::new(parent.as_os_str().as_bytes())?;
    let parent_fd = unsafe {
        libc::open(
            parent_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if parent_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let parent_file = unsafe { fs::File::from_raw_fd(parent_fd) };

    let file_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidInput, "atomic target has no file name")
    })?;
    let target_name = CString::new(file_name.as_bytes())?;
    let mut last_collision = None;

    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp_name = CString::new(format!(
            ".{}.tmp.{}.{sequence}",
            file_name.to_string_lossy(),
            std::process::id()
        ))?;
        let fd = unsafe {
            libc::openat(
                parent_file.as_raw_fd(),
                tmp_name.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::AlreadyExists {
                last_collision = Some(err);
                continue;
            }
            return Err(err.into());
        }

        let mut tmp_file = unsafe { fs::File::from_raw_fd(fd) };
        let write_result = (|| -> std::io::Result<()> {
            tmp_file.write_all(data)?;
            tmp_file.sync_all()?;
            drop(tmp_file);
            let renamed = unsafe {
                libc::renameat(
                    parent_file.as_raw_fd(),
                    tmp_name.as_ptr(),
                    parent_file.as_raw_fd(),
                    target_name.as_ptr(),
                )
            };
            if renamed != 0 {
                return Err(std::io::Error::last_os_error());
            }
            parent_file.sync_all()
        })();

        if let Err(err) = write_result {
            unsafe {
                libc::unlinkat(parent_file.as_raw_fd(), tmp_name.as_ptr(), 0);
            }
            return Err(err.into());
        }
        return Ok(());
    }

    Err(last_collision
        .unwrap_or_else(|| std::io::Error::new(ErrorKind::AlreadyExists, "temp file collision"))
        .into())
}

#[cfg(unix)]
fn sync_parent_dir(target: &Path) -> std::io::Result<()> {
    fs::File::open(target.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_target: &Path) -> std::io::Result<()> {
    Ok(())
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
    if let [(target, data)] = writes {
        return atomic_write(target, data);
    }
    for (target, data) in writes {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut written = false;
        for _ in 0..128 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
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

#[cfg(not(target_os = "linux"))]
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
/// On non-Unix platforms, the mode is ignored (the file is still written).
pub fn materialize_executable(target: &Path, data: &[u8], mode: u32) -> Result<()> {
    atomic_write(target, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(target, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
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
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if c.is_ascii() => out.push(c),
            c if (c as u32) <= 0xFFFF => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => {
                let n = c as u32 - 0x10000;
                out.push_str(&format!(
                    "\\u{:04x}\\u{:04x}",
                    0xD800 + (n >> 10),
                    0xDC00 + (n & 0x3FF)
                ));
            }
        }
    }
    out.push('"');
    out
}

pub fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => escape_string(s),
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let entries: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", escape_string(k), canonical_json(&map[*k])))
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        serde_json::Value::Array(arr) => {
            format!(
                "[{}]",
                arr.iter().map(canonical_json).collect::<Vec<_>>().join(",")
            )
        }
    }
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    #[test]
    fn private_replace_writes_complete_value_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.pem");
        atomic_write_private(&target, b"old").unwrap();
        atomic_write_private(&target, b"new-value").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new-value");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], target.file_name().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn private_replace_creates_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.pem");
        atomic_write_private(&target, b"secret").unwrap();
        assert_eq!(fs::metadata(target).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn private_replace_rejects_symlink_as_final_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = dir.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();
        let result = atomic_write_private(&linked_parent.join("secret.pem"), b"secret");
        assert!(result.is_err());
        assert!(!real_parent.join("secret.pem").exists());
    }
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
        if dest.exists() {
            return Ok(hash);
        }
        atomic_write(&dest, data)?;
        Ok(hash)
    }

    pub fn store_object(&self, value: &serde_json::Value) -> Result<String> {
        let json = canonical_json(value);
        let hash = sha256_hex(json.as_bytes());
        let dest = shard_path(&self.root, "objects", &hash, ".json");
        if dest.exists() {
            return Ok(hash);
        }
        atomic_write(&dest, json.as_bytes())?;
        Ok(hash)
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
                        let canon = canonical_json(&val);
                        serde_json::json!({ "valid": sha256_hex(canon.as_bytes()) == hash, "hash": hash })
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
            .map(|opt| opt.map(|v| canonical_json(&v).into_bytes()))
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
