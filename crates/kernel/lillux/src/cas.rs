use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::atomic_fs::atomic_write_with_mode;

/// Owner token carried by every secure temporary the CAS publishes into its
/// shard directories, whether by single-entry store or batch commit.
pub const CAS_TEMP_PURPOSE: crate::TempPurpose = crate::TempPurpose::new("cas");

mod record_canonical;
pub use record_canonical::{
    CanonicalRecordStorageQuote, canonical_record_bytes, canonical_record_quote_work_bound,
    canonical_record_storage_quote,
};

struct FixedJsonString {
    bytes: [u8; 64],
    len: usize,
    overflow: bool,
}

impl FixedJsonString {
    fn new() -> Self {
        Self {
            bytes: [0; 64],
            len: 0,
            overflow: false,
        }
    }

    fn push_ascii(&mut self, byte: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        } else {
            self.overflow = true;
        }
    }

    fn mark_non_ascii(&mut self) {
        self.overflow = true;
    }

    fn equals(&self, expected: &[u8]) -> bool {
        !self.overflow && self.len == expected.len() && &self.bytes[..self.len] == expected
    }
}

struct JsonDiscriminatorReader<R> {
    reader: R,
    peeked: Option<u8>,
    digest: Sha256,
}

struct SnapshotFile {
    file: Option<File>,
}

impl SnapshotFile {
    fn create() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd as _;
            let fd = unsafe {
                libc::memfd_create(
                    c"ryeos-cas-read-snapshot".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                file: Some(unsafe { File::from_raw_fd(fd) }),
            })
        }
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("bounded CAS kind selection requires sealed Linux anonymous snapshots")
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("live CAS read snapshot")
    }

    fn seal(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            const F_ADD_SEALS: libc::c_int = 1033;
            let seals =
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
            let result = unsafe { libc::fcntl(self.file_mut().as_raw_fd(), F_ADD_SEALS, seals) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("bounded CAS kind selection requires sealed Linux anonymous snapshots")
    }
}

impl Drop for SnapshotFile {
    fn drop(&mut self) {
        drop(self.file.take());
    }
}

struct SnapshotTeeReader<'a, R> {
    inner: R,
    snapshot: &'a mut File,
}

impl<R: Read> Read for SnapshotTeeReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(bytes)?;
        if read != 0 {
            self.snapshot.write_all(&bytes[..read])?;
        }
        Ok(read)
    }
}

impl<R: Read> JsonDiscriminatorReader<R> {
    fn next(&mut self) -> Result<Option<u8>> {
        if self.peeked.is_some() {
            return Ok(self.peeked.take());
        }
        self.next_raw()
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        if self.peeked.is_none() {
            self.peeked = self.next_raw()?;
        }
        Ok(self.peeked)
    }

    fn next_raw(&mut self) -> Result<Option<u8>> {
        let mut byte = [0_u8; 1];
        match self.reader.read(&mut byte)? {
            0 => Ok(None),
            _ => {
                self.digest.update(byte);
                Ok(Some(byte[0]))
            }
        }
    }

    fn non_whitespace(&mut self) -> Result<Option<u8>> {
        loop {
            match self.next()? {
                Some(b' ' | b'\n' | b'\r' | b'\t') => continue,
                byte => return Ok(byte),
            }
        }
    }

    fn expect(&mut self, expected: u8) -> Result<()> {
        match self.next()? {
            Some(byte) if byte == expected => Ok(()),
            _ => anyhow::bail!("malformed CAS object while inspecting its kind"),
        }
    }

    fn hex_quad(&mut self) -> Result<u16> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let digit = match self.next()? {
                Some(digit @ b'0'..=b'9') => digit - b'0',
                Some(digit @ b'a'..=b'f') => digit - b'a' + 10,
                Some(digit @ b'A'..=b'F') => digit - b'A' + 10,
                _ => anyhow::bail!("malformed CAS object unicode escape"),
            };
            value = (value << 4) | u16::from(digit);
        }
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<FixedJsonString> {
        let mut token = FixedJsonString::new();
        loop {
            match self.next()? {
                Some(b'"') => return Ok(token),
                Some(b'\\') => match self.next()? {
                    Some(b'"') => token.push_ascii(b'"'),
                    Some(b'\\') => token.push_ascii(b'\\'),
                    Some(b'/') => token.push_ascii(b'/'),
                    Some(b'b') => token.push_ascii(8),
                    Some(b'f') => token.push_ascii(12),
                    Some(b'n') => token.push_ascii(b'\n'),
                    Some(b'r') => token.push_ascii(b'\r'),
                    Some(b't') => token.push_ascii(b'\t'),
                    Some(b'u') => {
                        let first = self.hex_quad()?;
                        let scalar = if (0xd800..=0xdbff).contains(&first) {
                            self.expect(b'\\')?;
                            self.expect(b'u')?;
                            let second = self.hex_quad()?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                anyhow::bail!("malformed CAS object surrogate pair");
                            }
                            0x10000
                                + (((u32::from(first) - 0xd800) << 10)
                                    | (u32::from(second) - 0xdc00))
                        } else if (0xdc00..=0xdfff).contains(&first) {
                            anyhow::bail!("malformed CAS object surrogate pair");
                        } else {
                            u32::from(first)
                        };
                        if scalar <= 0x7f {
                            token.push_ascii(scalar as u8);
                        } else {
                            token.mark_non_ascii();
                        }
                    }
                    _ => anyhow::bail!("malformed CAS object string escape"),
                },
                Some(byte @ 0x00..=0x1f) => {
                    let _ = byte;
                    anyhow::bail!("malformed CAS object control character in string");
                }
                Some(byte @ 0x20..=0x7f) => token.push_ascii(byte),
                Some(_) => token.mark_non_ascii(),
                None => anyhow::bail!("truncated CAS object string"),
            }
        }
    }

    fn parse_number(&mut self, first: u8) -> Result<()> {
        let integer_first = if first == b'-' {
            self.next()?
                .ok_or_else(|| anyhow::anyhow!("truncated CAS object number"))?
        } else {
            first
        };
        match integer_first {
            b'0' => {
                if matches!(self.peek()?, Some(b'0'..=b'9')) {
                    anyhow::bail!("malformed CAS object number");
                }
            }
            b'1'..=b'9' => {
                while matches!(self.peek()?, Some(b'0'..=b'9')) {
                    self.next()?;
                }
            }
            _ => anyhow::bail!("malformed CAS object number"),
        }
        if self.peek()? == Some(b'.') {
            self.next()?;
            if !matches!(self.peek()?, Some(b'0'..=b'9')) {
                anyhow::bail!("malformed CAS object fraction");
            }
            while matches!(self.peek()?, Some(b'0'..=b'9')) {
                self.next()?;
            }
        }
        if matches!(self.peek()?, Some(b'e' | b'E')) {
            self.next()?;
            if matches!(self.peek()?, Some(b'+' | b'-')) {
                self.next()?;
            }
            if !matches!(self.peek()?, Some(b'0'..=b'9')) {
                anyhow::bail!("malformed CAS object exponent");
            }
            while matches!(self.peek()?, Some(b'0'..=b'9')) {
                self.next()?;
            }
        }
        Ok(())
    }

    fn skip_value(&mut self, first: u8, depth: usize) -> Result<()> {
        if depth > 128 {
            anyhow::bail!("CAS object nesting exceeds discriminator limit");
        }
        match first {
            b'"' => {
                self.parse_string()?;
            }
            b'{' => {
                match self.non_whitespace()? {
                    Some(b'}') => return Ok(()),
                    Some(b'"') => {}
                    _ => anyhow::bail!("malformed CAS object map"),
                }
                loop {
                    self.parse_string()?;
                    if self.non_whitespace()? != Some(b':') {
                        anyhow::bail!("malformed CAS object map");
                    }
                    let value = self
                        .non_whitespace()?
                        .ok_or_else(|| anyhow::anyhow!("truncated CAS object map"))?;
                    self.skip_value(value, depth + 1)?;
                    match self.non_whitespace()? {
                        Some(b',') => {
                            if self.non_whitespace()? != Some(b'"') {
                                anyhow::bail!("malformed CAS object map");
                            }
                        }
                        Some(b'}') => break,
                        _ => anyhow::bail!("malformed CAS object map"),
                    }
                }
            }
            b'[' => {
                match self.non_whitespace()? {
                    Some(b']') => return Ok(()),
                    Some(first) => self.skip_value(first, depth + 1)?,
                    None => anyhow::bail!("truncated CAS object array"),
                }
                loop {
                    match self.non_whitespace()? {
                        Some(b',') => {
                            let value = self
                                .non_whitespace()?
                                .ok_or_else(|| anyhow::anyhow!("truncated CAS object array"))?;
                            self.skip_value(value, depth + 1)?;
                        }
                        Some(b']') => break,
                        _ => anyhow::bail!("malformed CAS object array"),
                    }
                }
            }
            b't' => {
                for byte in b"rue" {
                    self.expect(*byte)?;
                }
            }
            b'f' => {
                for byte in b"alse" {
                    self.expect(*byte)?;
                }
            }
            b'n' => {
                for byte in b"ull" {
                    self.expect(*byte)?;
                }
            }
            b'-' | b'0'..=b'9' => self.parse_number(first)?,
            _ => anyhow::bail!("malformed CAS object value"),
        }
        Ok(())
    }

    fn capsule_kind(mut self) -> Result<(bool, String)> {
        if self.non_whitespace()? != Some(b'{') {
            anyhow::bail!("CAS object is not a JSON map");
        }
        let mut capsule = false;
        let mut saw_kind = false;
        match self.non_whitespace()? {
            Some(b'}') => {}
            Some(b'"') => {
                self.peeked = Some(b'"');
                loop {
                    self.expect(b'"')?;
                    let key = self.parse_string()?;
                    if self.non_whitespace()? != Some(b':') {
                        anyhow::bail!("malformed CAS object map");
                    }
                    let first = self
                        .non_whitespace()?
                        .ok_or_else(|| anyhow::anyhow!("truncated CAS object map"))?;
                    if key.equals(b"kind") {
                        if std::mem::replace(&mut saw_kind, true) {
                            anyhow::bail!("duplicate CAS object kind field");
                        }
                        if first == b'"' {
                            let value = self.parse_string()?;
                            capsule = value.equals(b"admitted_launch_capsule")
                                || value.equals(b"persistent_session_capsule");
                        } else {
                            capsule = true;
                            self.skip_value(first, 0)?;
                        }
                    } else {
                        self.skip_value(first, 0)?;
                    }
                    match self.non_whitespace()? {
                        Some(b',') => {
                            if self.non_whitespace()? != Some(b'"') {
                                anyhow::bail!("malformed CAS object map");
                            }
                            self.peeked = Some(b'"');
                        }
                        Some(b'}') => break,
                        _ => anyhow::bail!("malformed CAS object map"),
                    }
                }
            }
            _ => anyhow::bail!("malformed CAS object map"),
        }
        if self.non_whitespace()?.is_some() {
            anyhow::bail!("trailing bytes after CAS object JSON map");
        }
        Ok((capsule, format!("{:x}", self.digest.finalize())))
    }
}

/// Inspect a bounded JSON object without materializing a serde `Value`.
/// Duplicate decoded `kind` keys are rejected, so the result cannot inherit
/// serde's last-key-wins behavior at a pre-allocation size boundary.
pub fn json_object_is_capsule_kind(raw: &[u8]) -> Result<bool> {
    let (capsule, _) = JsonDiscriminatorReader {
        reader: std::io::Cursor::new(raw),
        peeked: None,
        digest: Sha256::new(),
    }
    .capsule_kind()?;
    Ok(capsule)
}

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
        let prepared_entry = directory
            .prepare_atomic_create(name, data, 0o644, CAS_TEMP_PURPOSE)
            .with_context(|| format!("prepare immutable CAS batch entry {}", target.display()))?;
        match prepared_entry {
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
        let publication = temp.publish();
        match publication {
            Ok(true) => {}
            Ok(false) => {
                let (target, data) = &writes[index];
                let verification = verify_batch_entry(&directory, &name, target, data);
                if let Err(error) = verification {
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

/// Failure to encode a value with the RyeOS canonical JSON contract.
///
/// The current `serde_json::Value` domain is fully encodable. The explicit
/// result keeps failures at this persistence boundary visible if that domain
/// is ever extended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalJsonError;

impl std::fmt::Display for CanonicalJsonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("canonical JSON encoding failed")
    }
}

impl std::error::Error for CanonicalJsonError {}

/// Serialize a value using the sole canonical JSON encoding for RyeOS CAS
/// objects.
///
/// These bytes are a persistence and signing protocol: object keys use
/// decoded-string ordering, non-ASCII scalars use lowercase `\u` escapes, and
/// numbers use decode-stable `serde_json::Number` rendering (including `0.0`
/// versus `0`). Typed floating-point values are normalized through the same
/// number decoder used when CAS objects are read. This is deliberately not
/// RFC 8785/JCS.
pub fn canonical_json(v: &serde_json::Value) -> Result<String, CanonicalJsonError> {
    String::from_utf8(canonical_json_bounded(v, usize::MAX)?).map_err(|_| CanonicalJsonError)
}

fn append_canonical_bounded(
    out: &mut Vec<u8>,
    bytes: &[u8],
    maximum: usize,
) -> Result<(), CanonicalJsonError> {
    if out.len().saturating_add(bytes.len()) > maximum {
        return Err(CanonicalJsonError);
    }
    out.extend_from_slice(bytes);
    Ok(())
}

fn write_string_bounded(
    value: &str,
    out: &mut Vec<u8>,
    maximum: usize,
) -> Result<(), CanonicalJsonError> {
    append_canonical_bounded(out, b"\"", maximum)?;
    for ch in value.chars() {
        let escaped: Option<&[u8]> = match ch {
            '"' => Some(b"\\\""),
            '\\' => Some(b"\\\\"),
            '\x08' => Some(b"\\b"),
            '\x0c' => Some(b"\\f"),
            '\n' => Some(b"\\n"),
            '\r' => Some(b"\\r"),
            '\t' => Some(b"\\t"),
            c if (c as u32) < 0x20 => Some(match c {
                '\0' => b"\\u0000",
                '\x01' => b"\\u0001",
                '\x02' => b"\\u0002",
                '\x03' => b"\\u0003",
                '\x04' => b"\\u0004",
                '\x05' => b"\\u0005",
                '\x06' => b"\\u0006",
                '\x07' => b"\\u0007",
                '\x0b' => b"\\u000b",
                '\x0e' => b"\\u000e",
                '\x0f' => b"\\u000f",
                '\x10' => b"\\u0010",
                '\x11' => b"\\u0011",
                '\x12' => b"\\u0012",
                '\x13' => b"\\u0013",
                '\x14' => b"\\u0014",
                '\x15' => b"\\u0015",
                '\x16' => b"\\u0016",
                '\x17' => b"\\u0017",
                '\x18' => b"\\u0018",
                '\x19' => b"\\u0019",
                '\x1a' => b"\\u001a",
                '\x1b' => b"\\u001b",
                '\x1c' => b"\\u001c",
                '\x1d' => b"\\u001d",
                '\x1e' => b"\\u001e",
                '\x1f' => b"\\u001f",
                _ => unreachable!(),
            }),
            c if c.is_ascii() => {
                let byte = [c as u8];
                append_canonical_bounded(out, &byte, maximum)?;
                None
            }
            c => {
                let scalar = c as u32;
                let mut escaped = [0_u8; 12];
                let len = if scalar <= 0xffff {
                    write_unicode_escape(scalar, &mut escaped[..6]);
                    6
                } else {
                    let offset = scalar - 0x10000;
                    write_unicode_escape(0xd800 + (offset >> 10), &mut escaped[..6]);
                    write_unicode_escape(0xdc00 + (offset & 0x3ff), &mut escaped[6..12]);
                    12
                };
                append_canonical_bounded(out, &escaped[..len], maximum)?;
                None
            }
        };
        if let Some(escaped) = escaped {
            append_canonical_bounded(out, escaped, maximum)?;
        }
    }
    append_canonical_bounded(out, b"\"", maximum)
}

fn write_unicode_escape(value: u32, output: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output[0] = b'\\';
    output[1] = b'u';
    output[2] = HEX[((value >> 12) & 0xf) as usize];
    output[3] = HEX[((value >> 8) & 0xf) as usize];
    output[4] = HEX[((value >> 4) & 0xf) as usize];
    output[5] = HEX[(value & 0xf) as usize];
}

/// Canonicalize an already materialized JSON value under a byte ceiling.
pub fn canonical_json_bounded(
    value: &serde_json::Value,
    maximum_bytes: usize,
) -> Result<Vec<u8>, CanonicalJsonError> {
    let mut canonical = Vec::new();
    write_value_bounded(value, &mut canonical, maximum_bytes)?;
    Ok(canonical)
}

fn write_value_bounded(
    value: &serde_json::Value,
    out: &mut Vec<u8>,
    maximum: usize,
) -> Result<(), CanonicalJsonError> {
    use serde_json::Value;
    match value {
        Value::Null => append_canonical_bounded(out, b"null", maximum),
        Value::Bool(true) => append_canonical_bounded(out, b"true", maximum),
        Value::Bool(false) => append_canonical_bounded(out, b"false", maximum),
        Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                return write_unsigned_decimal(value, out, maximum);
            }
            if let Some(value) = number.as_i64() {
                return write_signed_decimal(value, out, maximum);
            }
            let rendered = number.to_string();
            let normalized = serde_json::from_str::<serde_json::Number>(&rendered)
                .map_err(|_| CanonicalJsonError)?;
            append_canonical_bounded(out, normalized.to_string().as_bytes(), maximum)
        }
        Value::String(string) => write_string_bounded(string, out, maximum),
        Value::Array(values) => {
            append_canonical_bounded(out, b"[", maximum)?;
            for (index, item) in values.iter().enumerate() {
                if index != 0 {
                    append_canonical_bounded(out, b",", maximum)?;
                }
                write_value_bounded(item, out, maximum)?;
            }
            append_canonical_bounded(out, b"]", maximum)
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            append_canonical_bounded(out, b"{", maximum)?;
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    append_canonical_bounded(out, b",", maximum)?;
                }
                write_string_bounded(key, out, maximum)?;
                append_canonical_bounded(out, b":", maximum)?;
                write_value_bounded(&map[key], out, maximum)?;
            }
            append_canonical_bounded(out, b"}", maximum)
        }
    }
}

fn write_unsigned_decimal(
    mut value: u64,
    out: &mut Vec<u8>,
    maximum: usize,
) -> Result<(), CanonicalJsonError> {
    let mut digits = [0_u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    append_canonical_bounded(out, &digits[start..], maximum)
}

fn write_signed_decimal(
    value: i64,
    out: &mut Vec<u8>,
    maximum: usize,
) -> Result<(), CanonicalJsonError> {
    if value >= 0 {
        return write_unsigned_decimal(value as u64, out, maximum);
    }
    append_canonical_bounded(out, b"-", maximum)?;
    // `unsigned_abs` also handles i64::MIN without signed overflow.
    write_unsigned_decimal(value.unsigned_abs(), out, maximum)
}

/// Cap both the typed JSON serialization and the RyeOS canonical output bytes.
/// The intermediate `Value` and object-key sorting workspace can use more
/// memory than `maximum_bytes`; this is an output-byte bound, not a total
/// process-memory ceiling. The returned bytes are suitable for exact hashing
/// and CAS persistence.
pub fn canonical_json_bytes_bounded<T: serde::Serialize>(
    value: &T,
    maximum_bytes: usize,
) -> Result<Vec<u8>> {
    struct CappedVec {
        bytes: Vec<u8>,
        maximum: usize,
    }
    impl std::io::Write for CappedVec {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > self.maximum {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "serialized JSON exceeds byte ceiling",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut raw = CappedVec {
        bytes: Vec::new(),
        maximum: maximum_bytes,
    };
    serde_json::to_writer(&mut raw, value)?;
    let parsed: serde_json::Value = serde_json::from_slice(&raw.bytes)?;
    canonical_json_bounded(&parsed, maximum_bytes).map_err(anyhow::Error::from)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedBlobOutcome {
    pub hash: String,
    pub size: u64,
    pub normalized_mode: u32,
    pub created: bool,
}

/// Exact outcome of pruning interrupted streaming blob captures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlobCapturePruneReport {
    pub files: usize,
    pub bytes: u64,
}

const BLOB_CAPTURE_STAGING_DIRECTORY: &str = ".staging";

/// Whether one CAS namespace child is structural metadata rather than a hash
/// shard. The CAS layer owns this layout contract; callers must not duplicate
/// its private directory names.
pub fn is_reserved_namespace_entry(namespace: &str, entry: &str) -> bool {
    namespace == "blobs" && entry == BLOB_CAPTURE_STAGING_DIRECTORY
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

    /// Capacity observed through the exact CAS root descriptor. Storage
    /// policy remains caller-owned; this preserves descriptor authority when
    /// a caller must reserve bytes and file identities before publication.
    pub fn filesystem_capacity(&self) -> Result<crate::FilesystemCapacity> {
        let root = match self.pinned_root.as_ref() {
            Some(root) => root.try_clone()?,
            None => crate::secure_fs::PinnedDirectory::open(&self.root)?.ok_or_else(|| {
                anyhow::anyhow!("CAS root is unavailable: {}", self.root.display())
            })?,
        };
        root.filesystem_capacity()
    }

    /// Reclaim exact interrupted streaming-capture files.
    ///
    /// The caller must exclude concurrent CAS mutation for the full call. An
    /// unknown entry fails closed rather than being treated as disposable.
    /// Dry-run opens and validates the existing namespace without creating or
    /// deleting anything.
    pub fn prune_abandoned_blob_captures(&self, dry_run: bool) -> Result<BlobCapturePruneReport> {
        let root = match self.pinned_root.as_ref() {
            Some(root) => root.try_clone()?,
            None => match crate::secure_fs::PinnedDirectory::open(&self.root)? {
                Some(root) => root,
                None => return Ok(BlobCapturePruneReport::default()),
            },
        };
        let Some(blobs) = root.open_child_directory(OsStr::new("blobs"))? else {
            return Ok(BlobCapturePruneReport::default());
        };
        let staging_name = OsStr::new(BLOB_CAPTURE_STAGING_DIRECTORY);
        let Some(staging) = blobs.open_child_directory(staging_name)? else {
            return Ok(BlobCapturePruneReport::default());
        };

        let mut report = BlobCapturePruneReport::default();
        for name in staging.entry_names()? {
            let text = name.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "non-UTF8 CAS blob capture staging entry under {}",
                    staging.path().display()
                )
            })?;
            if !is_blob_capture_staging_name(text) {
                anyhow::bail!(
                    "unexpected CAS blob capture staging entry: {}",
                    staging.path().join(&name).display()
                );
            }
            let file = staging.open_regular(&name, false)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "CAS blob capture staging entry is not a regular file: {}",
                    staging.path().join(&name).display()
                )
            })?;
            let size = file.metadata()?.len();
            if !dry_run {
                staging.remove_if_same(&name, &file)?;
            }
            report.files += 1;
            report.bytes = report.bytes.saturating_add(size);
        }
        if !dry_run
            && !blobs
                .remove_empty_child_if_same(staging_name, &staging)
                .map(|receipt| receipt.is_some())?
        {
            anyhow::bail!(
                "CAS blob capture staging directory changed during cleanup: {}",
                staging.path().display()
            );
        }
        Ok(report)
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

    /// Read and verify an exact CAS blob without allocating beyond the caller's
    /// byte ceiling. The limit is enforced while reading the pinned entry, not
    /// by trusting an earlier pathname or size observation.
    pub fn get_blob_bounded(&self, hash: &str, maximum_bytes: u64) -> Result<Option<Vec<u8>>> {
        if !canonical_cas_hash(hash) {
            return Ok(None);
        }
        let Some((file, path)) =
            open_existing_entry(&self.root, self.pinned_root.as_ref(), "blobs", hash, "")?
        else {
            return Ok(None);
        };
        let limit = maximum_bytes
            .checked_add(1)
            .context("CAS blob byte ceiling overflow")?;
        let mut bytes = Vec::new();
        file.take(limit)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read bounded CAS entry {}", path.display()))?;
        if bytes.len() as u64 > maximum_bytes {
            anyhow::bail!("CAS blob {} exceeds {} bytes", hash, maximum_bytes);
        }
        let actual_hash = sha256_hex(&bytes);
        if actual_hash != hash {
            anyhow::bail!(
                "CAS corruption: entry at {} hashes to {}, expected {}",
                path.display(),
                actual_hash,
                hash
            );
        }
        Ok(Some(bytes))
    }

    /// Open one descriptor-pinned CAS blob for bounded streaming protocols.
    /// The CAS publication path already verified the content address; callers
    /// must verify the complete digest while consuming the stream before they
    /// make it authoritative in another store.
    pub fn open_blob(&self, hash: &str) -> Result<Option<(fs::File, u64)>> {
        if !canonical_cas_hash(hash) {
            return Ok(None);
        }
        let Some((file, _path)) =
            open_existing_entry(&self.root, self.pinned_root.as_ref(), "blobs", hash, "")?
        else {
            return Ok(None);
        };
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("CAS blob {hash} is not a regular file");
        }
        Ok(Some((file, metadata.len())))
    }

    /// Verify a potentially large retained blob by streaming its exact
    /// descriptor-pinned bytes. This supplies historical CAS readers with a
    /// bounded digest check without allocating an executable-sized buffer.
    pub fn verify_blob_bounded(&self, hash: &str, maximum_bytes: u64) -> Result<Option<u64>> {
        let Some((mut file, size)) = self.open_blob(hash)? else {
            return Ok(None);
        };
        if size > maximum_bytes {
            anyhow::bail!("CAS blob {hash} exceeds {maximum_bytes} bytes");
        }
        let mut digest = Sha256::new();
        let mut observed = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            observed = observed
                .checked_add(u64::try_from(read)?)
                .context("CAS blob length overflow")?;
            if observed > maximum_bytes {
                anyhow::bail!("CAS blob {hash} exceeds {maximum_bytes} bytes during read");
            }
            digest.update(&buffer[..read]);
        }
        let actual = format!("{:x}", digest.finalize());
        if observed != size || actual != hash {
            anyhow::bail!(
                "CAS blob {hash} changed while streaming: observed {observed} bytes, digest {actual}"
            );
        }
        Ok(Some(observed))
    }

    /// Open one descriptor-pinned CAS object without allocating its body.
    ///
    /// This is the bounded-transport counterpart to [`Self::get_object`]. The
    /// caller must read no more than its protocol limit and must verify the
    /// content address and canonical JSON encoding before trusting the value.
    pub fn open_object(&self, hash: &str) -> Result<Option<(fs::File, u64)>> {
        if !canonical_cas_hash(hash) {
            return Ok(None);
        }
        let Some((file, _path)) = open_existing_entry(
            &self.root,
            self.pinned_root.as_ref(),
            "objects",
            hash,
            ".json",
        )?
        else {
            return Ok(None);
        };
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("CAS object {hash} is not a regular file");
        }
        Ok(Some((file, metadata.len())))
    }

    /// Read one canonical CAS object under an exact byte ceiling, checking
    /// both its content address and canonical JSON encoding before returning
    /// a value to a historical authority reader.
    pub fn get_object_bounded(
        &self,
        hash: &str,
        maximum_bytes: u64,
    ) -> Result<Option<serde_json::Value>> {
        let Some(bytes) = self.get_object_raw_bytes_bounded(hash, maximum_bytes)? else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let maximum = usize::try_from(maximum_bytes).unwrap_or(usize::MAX);
        if canonical_json_bounded(&value, maximum)?.as_slice() != bytes {
            anyhow::bail!("CAS object {hash} is not canonical JSON");
        }
        Ok(Some(value))
    }

    /// Read exact canonical CAS bytes under a pre-parse byte ceiling. The
    /// hash and canonical encoding are checked before the caller can decode a
    /// typed object.
    pub fn get_object_bytes_bounded(
        &self,
        hash: &str,
        maximum_bytes: u64,
    ) -> Result<Option<Vec<u8>>> {
        let Some(bytes) = self.get_object_raw_bytes_bounded(hash, maximum_bytes)? else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let maximum = usize::try_from(maximum_bytes).unwrap_or(usize::MAX);
        if canonical_json_bounded(&value, maximum)?.as_slice() != bytes {
            anyhow::bail!("CAS object {hash} is not canonical JSON");
        }
        Ok(Some(bytes))
    }

    /// Read raw addressed object bytes under a pre-parse size ceiling. This
    /// deliberately leaves JSON interpretation to a caller that can inspect
    /// an object discriminator before materializing its full value.
    pub fn get_object_raw_bytes_bounded(
        &self,
        hash: &str,
        maximum_bytes: u64,
    ) -> Result<Option<Vec<u8>>> {
        let Some((file, size)) = self.open_object(hash)? else {
            return Ok(None);
        };
        let bytes =
            crate::secure_fs::read_open_regular_file_exact_bounded(file, size, maximum_bytes)?;
        if sha256_hex(&bytes) != hash {
            anyhow::bail!("CAS object {hash} changed from its content address");
        }
        Ok(Some(bytes))
    }

    /// Take a bounded sealed anonymous snapshot while scanning the exact pinned
    /// JSON object through a fixed-size discriminator reader. Only after the
    /// snapshot digest and kind ceiling pass is a raw body Vec allocated.
    /// Full serde parsing, canonical-byte validation and addressed-hash checks
    /// remain the caller's responsibility.
    pub fn get_object_raw_bytes_bounded_with_capsule_ceiling(
        &self,
        hash: &str,
        maximum_bytes: u64,
        capsule_maximum_bytes: u64,
    ) -> Result<Option<Vec<u8>>> {
        self.get_object_raw_bytes_bounded_with_capsule_ceiling_after_scan(
            hash,
            maximum_bytes,
            capsule_maximum_bytes,
            || Ok(()),
        )
    }

    fn get_object_raw_bytes_bounded_with_capsule_ceiling_after_scan(
        &self,
        hash: &str,
        maximum_bytes: u64,
        capsule_maximum_bytes: u64,
        after_scan: impl FnOnce() -> Result<()>,
    ) -> Result<Option<Vec<u8>>> {
        let Some((mut file, size)) = self.open_object(hash)? else {
            return Ok(None);
        };
        if size > maximum_bytes {
            anyhow::bail!("CAS object {hash} exceeds byte limit {maximum_bytes}");
        }
        let mut snapshot = SnapshotFile::create()?;
        let (capsule, scanned_hash) = JsonDiscriminatorReader {
            reader: BufReader::new(SnapshotTeeReader {
                inner: (&mut file).take(size),
                snapshot: snapshot.file_mut(),
            }),
            peeked: None,
            digest: Sha256::new(),
        }
        .capsule_kind()?;
        if scanned_hash != hash {
            anyhow::bail!("CAS object {hash} changed during bounded kind inspection");
        }
        snapshot.seal()?;
        let selected_maximum = if capsule {
            maximum_bytes.min(capsule_maximum_bytes)
        } else {
            maximum_bytes
        };
        if size > selected_maximum {
            anyhow::bail!("CAS object {hash} exceeds selected byte limit {selected_maximum}");
        }
        after_scan()?;
        snapshot.file_mut().seek(SeekFrom::Start(0))?;
        let length =
            usize::try_from(size).context("bounded CAS object size exceeds this platform")?;
        let mut bytes = vec![0_u8; length];
        snapshot.file_mut().read_exact(&mut bytes)?;
        let mut trailing = [0_u8; 1];
        if snapshot.file_mut().read(&mut trailing)? != 0 {
            anyhow::bail!("bounded CAS read snapshot exceeds its scanned size");
        }
        if sha256_hex(&bytes) != hash {
            anyhow::bail!("CAS read snapshot for object {hash} changed from its content address");
        }
        Ok(Some(bytes))
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
            let canonical_hash = sha256_hex(canonical.as_bytes());
            anyhow::bail!(
                "CAS object bytes satisfy address {hash} but violate the RyeOS canonical JSON contract: {} (canonical bytes would address {canonical_hash})",
                path.display()
            );
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

    /// Stream one already-open regular file into CAS without buffering the
    /// payload in memory. The source inode is compared before and after the
    /// read, so concurrent mutation fails rather than publishing torn bytes.
    pub fn put_blob_from_open_regular(
        &self,
        source: fs::File,
        display_path: &Path,
    ) -> Result<StreamedBlobOutcome> {
        self.put_blob_from_open_regular_inner(source, display_path, None, None)
    }

    /// Bounded form of [`Self::put_blob_from_open_regular`]. The bound is
    /// checked against both the opening metadata and the bytes actually read,
    /// before an over-limit chunk is written to CAS staging.
    pub fn put_blob_from_open_regular_bounded(
        &self,
        source: fs::File,
        display_path: &Path,
        max_bytes: u64,
    ) -> Result<StreamedBlobOutcome> {
        self.put_blob_from_open_regular_inner(source, display_path, Some(max_bytes), None)
    }

    /// Cooperatively bounded ingestion: check the monotonic deadline between
    /// reads, writes, verification and publication. This cannot interrupt a
    /// kernel filesystem stall; the caller still needs an outer lifetime bound.
    pub fn put_blob_from_open_regular_with_deadline(
        &self,
        source: fs::File,
        display_path: &Path,
        max_bytes: u64,
        deadline: crate::time::MonotonicDeadline,
    ) -> Result<StreamedBlobOutcome> {
        self.put_blob_from_open_regular_inner(source, display_path, Some(max_bytes), Some(deadline))
    }

    fn put_blob_from_open_regular_inner(
        &self,
        mut source: fs::File,
        display_path: &Path,
        max_bytes: Option<u64>,
        deadline: Option<crate::time::MonotonicDeadline>,
    ) -> Result<StreamedBlobOutcome> {
        check_capture_deadline(deadline)?;
        #[cfg(not(unix))]
        {
            let _ = (&mut source, display_path, max_bytes);
            anyhow::bail!("streaming regular-file CAS ingestion is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

            let before = source.metadata()?;
            if !before.file_type().is_file() {
                anyhow::bail!(
                    "streaming CAS source is not a regular file: {}",
                    display_path.display()
                );
            }
            if let Some(max_bytes) = max_bytes
                && before.len() > max_bytes
            {
                anyhow::bail!(
                    "streaming CAS source exceeds {max_bytes} bytes: {}",
                    display_path.display()
                );
            }
            let normalized_mode = if before.permissions().mode() & 0o111 == 0 {
                0o644
            } else {
                0o755
            };

            let root = match self.pinned_root.as_ref() {
                Some(root) => root.try_clone()?,
                None => crate::secure_fs::PinnedDirectory::open_or_create(&self.root)?,
            };
            let blobs = root.open_or_create_child(OsStr::new("blobs"), 0o777)?;
            let staging =
                blobs.open_or_create_child(OsStr::new(BLOB_CAPTURE_STAGING_DIRECTORY), 0o700)?;
            let staging_name = OsString::from(format!(
                "capture.{}.{}.tmp",
                std::process::id(),
                crate::atomic_fs::next_temp_sequence()
            ));
            let mut staged = staging.open_regular_create(&staging_name, true, true, 0o600)?;

            let result = (|| {
                let mut digest = Sha256::new();
                let mut size = 0_u64;
                let mut buffer = vec![0_u8; 1024 * 1024];
                loop {
                    check_capture_deadline(deadline)?;
                    let read = source.read(&mut buffer).with_context(|| {
                        format!("stream project file {} into CAS", display_path.display())
                    })?;
                    if read == 0 {
                        break;
                    }
                    let next_size = size
                        .checked_add(read as u64)
                        .ok_or_else(|| anyhow::anyhow!("project file size overflow"))?;
                    if let Some(max_bytes) = max_bytes
                        && next_size > max_bytes
                    {
                        anyhow::bail!(
                            "streaming CAS source exceeds {max_bytes} bytes while reading: {}",
                            display_path.display()
                        );
                    }
                    digest.update(&buffer[..read]);
                    check_capture_deadline(deadline)?;
                    staged.write_all(&buffer[..read])?;
                    size = next_size;
                }
                check_capture_deadline(deadline)?;
                staged.sync_all()?;
                check_capture_deadline(deadline)?;

                let after = source.metadata()?;
                let unchanged = before.dev() == after.dev()
                    && before.ino() == after.ino()
                    && before.size() == after.size()
                    && before.mtime() == after.mtime()
                    && before.mtime_nsec() == after.mtime_nsec()
                    && before.ctime() == after.ctime()
                    && before.ctime_nsec() == after.ctime_nsec()
                    && before.mode() == after.mode()
                    && size == after.size();
                if !unchanged {
                    anyhow::bail!(
                        "project file changed while being captured: {}",
                        display_path.display()
                    );
                }

                let hash = format!("{:x}", digest.finalize());
                let (target_parent, target_name, target_path) = open_or_create_entry_parent(
                    &self.root,
                    self.pinned_root.as_ref(),
                    "blobs",
                    &hash,
                    "",
                )?;
                let existing = target_parent.open_regular(&target_name, false)?;
                let created = if let Some(existing) = existing {
                    verify_existing_streamed_entry(existing, &hash, size, &target_path, deadline)?;
                    false
                } else if target_parent.publish_regular_link_from(
                    &target_name,
                    &staging,
                    &staging_name,
                    &staged,
                )? {
                    true
                } else {
                    let existing = target_parent
                        .open_regular(&target_name, false)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "CAS publication winner disappeared: {}",
                                target_path.display()
                            )
                        })?;
                    verify_existing_streamed_entry(existing, &hash, size, &target_path, deadline)?;
                    false
                };
                if !created {
                    let _ = staging.remove_if_same(&staging_name, &staged);
                }
                check_capture_deadline(deadline)?;
                Ok(StreamedBlobOutcome {
                    hash,
                    size,
                    normalized_mode,
                    created,
                })
            })();
            if result.is_err() {
                let _ = staging.remove_if_same(&staging_name, &staged);
            }
            result
        }
    }

    /// Stream a verified CAS blob into a create-new regular destination.
    /// Neither the CAS entry nor the destination's path components may be
    /// symlinks. Partial destinations are removed before returning an error.
    pub fn materialize_blob_to_new_file(
        &self,
        hash: &str,
        target: &Path,
        normalized_mode: u32,
    ) -> Result<u64> {
        let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
        let parent = crate::secure_fs::PinnedDirectory::open_or_create(parent_path)?;
        let name = target
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("materialization target has no file name"))?;
        self.materialize_blob_to_new_regular(hash, &parent, name, normalized_mode)
    }

    /// Stream a verified CAS blob into one create-new child of an exact pinned
    /// directory. This is the authority-preserving form for callers that
    /// already hold the destination inode and must not convert it back into a
    /// `/proc/self/fd` pathname for a second traversal.
    pub fn materialize_blob_to_new_regular(
        &self,
        hash: &str,
        parent: &crate::secure_fs::PinnedDirectory,
        name: &OsStr,
        normalized_mode: u32,
    ) -> Result<u64> {
        if !matches!(normalized_mode, 0o644 | 0o755) {
            anyhow::bail!("unsupported materialized project mode {normalized_mode:#o}");
        }
        if !canonical_cas_hash(hash) {
            anyhow::bail!("invalid CAS blob hash {hash}");
        }
        let (mut source, source_path) =
            open_existing_entry(&self.root, self.pinned_root.as_ref(), "blobs", hash, "")?
                .ok_or_else(|| anyhow::anyhow!("CAS blob {hash} is missing"))?;
        let mut destination = parent.open_regular_create(name, true, true, normalized_mode)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            destination.set_permissions(fs::Permissions::from_mode(normalized_mode))?;
        }
        let result = (|| {
            let mut digest = Sha256::new();
            let mut size = 0_u64;
            let mut buffer = vec![0_u8; 1024 * 1024];
            loop {
                let read = source
                    .read(&mut buffer)
                    .with_context(|| format!("read verified CAS blob {}", source_path.display()))?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
                destination.write_all(&buffer[..read])?;
                size = size
                    .checked_add(read as u64)
                    .ok_or_else(|| anyhow::anyhow!("CAS blob size overflow"))?;
            }
            destination.sync_all()?;
            parent.sync()?;
            let actual = format!("{:x}", digest.finalize());
            if actual != hash {
                anyhow::bail!(
                    "CAS corruption: entry at {} hashes to {}, expected {}",
                    source_path.display(),
                    actual,
                    hash
                );
            }
            Ok(size)
        })();
        if result.is_err() {
            let _ = parent.remove_if_same(name, &destination);
        }
        result
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

    /// Persist exact canonical object bytes after enforcing the same cap used
    /// by the producer. No reserialization occurs between hashing and write.
    pub fn store_canonical_object_bytes_bounded(
        &self,
        bytes: &[u8],
        maximum_bytes: usize,
    ) -> Result<String> {
        if bytes.len() > maximum_bytes {
            anyhow::bail!("canonical CAS object exceeds {maximum_bytes} bytes");
        }
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        if canonical_json_bounded(&value, maximum_bytes)?.as_slice() != bytes {
            anyhow::bail!("CAS object bytes are not canonical JSON");
        }
        let hash = sha256_hex(bytes);
        store_exact_entry(
            &self.root,
            self.pinned_root.as_ref(),
            "objects",
            &hash,
            ".json",
            bytes,
        )?;
        Ok(hash)
    }
}

fn is_blob_capture_staging_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("capture.") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(".tmp") else {
        return false;
    };
    let Some((pid, sequence)) = rest.split_once('.') else {
        return false;
    };
    !pid.is_empty()
        && !sequence.is_empty()
        && !sequence.contains('.')
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
}

fn verify_existing_streamed_entry(
    mut file: fs::File,
    expected_hash: &str,
    expected_size: u64,
    path: &Path,
    deadline: Option<crate::time::MonotonicDeadline>,
) -> Result<()> {
    check_capture_deadline(deadline)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() != expected_size {
        anyhow::bail!(
            "CAS collision: existing entry has wrong type or size: {}",
            path.display()
        );
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        check_capture_deadline(deadline)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let actual = format!("{:x}", digest.finalize());
    if actual != expected_hash {
        anyhow::bail!(
            "CAS collision: existing entry at {} hashes to {}, expected {}",
            path.display(),
            actual,
            expected_hash
        );
    }
    Ok(())
}

fn check_capture_deadline(deadline: Option<crate::time::MonotonicDeadline>) -> Result<()> {
    if deadline.is_some_and(|deadline| deadline.has_elapsed()) {
        anyhow::bail!("regular-file capture deadline expired");
    }
    Ok(())
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

    let publication = parent.atomic_write_if_same(&name, None, bytes, 0o644, CAS_TEMP_PURPOSE);
    match publication {
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
            let membership = store.has(&hash);
            match membership {
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

#[cfg(test)]
mod bounded_cas_tests {
    use super::*;

    fn scan_kind(source: &[u8]) -> Result<(bool, String)> {
        JsonDiscriminatorReader {
            reader: BufReader::new(source),
            peeked: None,
            digest: Sha256::new(),
        }
        .capsule_kind()
    }

    #[test]
    fn bounded_blob_and_object_reads_accept_exact_size_and_reject_one_byte_short() {
        let directory = tempfile::tempdir().unwrap();
        let cas = CasStore::new(directory.path().join("objects"));

        let blob = vec![0xa5; 257];
        let blob_hash = cas.store_blob(&blob).unwrap();
        assert_eq!(
            cas.get_blob_bounded(&blob_hash, u64::try_from(blob.len()).unwrap())
                .unwrap(),
            Some(blob.clone())
        );
        assert!(
            cas.get_blob_bounded(&blob_hash, u64::try_from(blob.len() - 1).unwrap())
                .is_err()
        );

        let object =
            serde_json::json!({"kind":"admitted_launch_capsule", "payload":"x".repeat(91)});
        let canonical = canonical_json(&object).unwrap();
        let object_hash = cas.store_object(&object).unwrap();
        let exact_limit = u64::try_from(canonical.len()).unwrap();
        assert_eq!(
            cas.get_object_bounded(&object_hash, exact_limit).unwrap(),
            Some(object)
        );
        assert!(
            cas.get_object_bounded(&object_hash, exact_limit - 1)
                .is_err()
        );
        assert_eq!(
            cas.get_object_raw_bytes_bounded_with_capsule_ceiling(
                &object_hash,
                exact_limit,
                exact_limit,
            )
            .unwrap(),
            Some(canonical.clone().into_bytes()),
        );
        assert!(
            cas.get_object_raw_bytes_bounded_with_capsule_ceiling(
                &object_hash,
                exact_limit,
                exact_limit - 1,
            )
            .is_err()
        );
    }

    #[test]
    fn kind_scan_is_bounded_and_generic_objects_keep_their_selected_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let cas = CasStore::new(directory.path().join("objects"));
        let generic = serde_json::json!({
            "aa_payload": {"nested": ["x".repeat(128)]},
            "kind": "ordinary_object",
        });
        let generic_bytes = canonical_json(&generic).unwrap().into_bytes();
        let generic_hash = cas.store_object(&generic).unwrap();
        assert!(generic_bytes.len() > 64);
        assert_eq!(
            cas.get_object_raw_bytes_bounded_with_capsule_ceiling(
                &generic_hash,
                u64::try_from(generic_bytes.len()).unwrap(),
                64,
            )
            .unwrap(),
            Some(generic_bytes),
        );

        let capsule = serde_json::json!({"kind":"admitted_launch_capsule", "payload":"safe"});
        let capsule_bytes = canonical_json(&capsule).unwrap().into_bytes();
        let capsule_hash = cas.store_object(&capsule).unwrap();
        let exact = u64::try_from(capsule_bytes.len()).unwrap();
        assert_eq!(
            cas.get_object_raw_bytes_bounded_with_capsule_ceiling(&capsule_hash, exact, exact)
                .unwrap(),
            Some(capsule_bytes),
        );
        assert!(
            cas.get_object_raw_bytes_bounded_with_capsule_ceiling(&capsule_hash, exact, exact - 1)
                .is_err()
        );
    }

    #[test]
    fn kind_scan_handles_escaped_and_duplicate_discriminators_without_string_allocation() {
        let (capsule, _) =
            scan_kind(br#"{"a":{"large":[1,2,3]},"ki\u006ed":"persistent_session_capsule"}"#)
                .unwrap();
        assert!(capsule);
        assert!(
            scan_kind(br#"{"kind":"ordinary_object","kind":"admitted_launch_capsule"}"#).is_err()
        );
        let (capsule, _) =
            scan_kind(format!(r#"{{"kind":"{}"}}"#, "k".repeat(80)).as_bytes()).unwrap();
        assert!(!capsule);
        let non_ascii_kind = r#"{"kind":"ordinary_世界"}"#;
        assert!(!scan_kind(non_ascii_kind.as_bytes()).unwrap().0);
        let oversized_key = format!(
            r#"{{"{}":{{"nested":true}},"kind":"ordinary_object"}}"#,
            "x".repeat(4096)
        );
        let (capsule, _) = scan_kind(oversized_key.as_bytes()).unwrap();
        assert!(!capsule);
    }

    #[test]
    fn discriminator_hash_covers_numeric_delimiters_and_requeued_quotes_once() {
        for source in [
            br#"{"kind":"ordinary","n":1}"#.as_slice(),
            br#"{"kind":"ordinary","n":-2}"#.as_slice(),
            br#"{"kind":"ordinary","n":1.25}"#.as_slice(),
            br#"{"kind":"ordinary","n":1e+2}"#.as_slice(),
            br#"{"a":{"b":[true,false,null,1]},"kind":"ordinary"}"#.as_slice(),
            br#"{"kind":"ordinary","tail":"quoted"}"#.as_slice(),
        ] {
            let (_, scanned_hash) = scan_kind(source).unwrap();
            assert_eq!(scanned_hash, sha256_hex(source));
        }
    }

    #[test]
    fn capsule_kind_and_body_share_one_snapshot_across_interpass_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let cas = CasStore::new(directory.path().join("objects"));
        let payload = "x".repeat(16 * 1024 * 1024 + 256);
        let capsule = serde_json::json!({"kind":"admitted_launch_capsule", "payload":payload});
        let capsule_bytes = canonical_json(&capsule).unwrap().into_bytes();
        let generic_empty = serde_json::json!({"kind":"ordinary_object", "payload":""});
        let generic_empty_bytes = canonical_json(&generic_empty).unwrap();
        let generic_padding = capsule_bytes.len() - generic_empty_bytes.len();
        let generic = serde_json::json!({
            "kind":"ordinary_object",
            "payload":"x".repeat(generic_padding),
        });
        let generic_bytes = canonical_json(&generic).unwrap().into_bytes();
        assert_eq!(generic_bytes.len(), capsule_bytes.len());
        assert!(capsule_bytes.len() > 16 * 1024 * 1024);
        let generic_hash = cas.store_object(&generic).unwrap();
        let generic_path = shard_path(cas.root(), "objects", &generic_hash, ".json");

        let snapshot = cas
            .get_object_raw_bytes_bounded_with_capsule_ceiling_after_scan(
                &generic_hash,
                u64::try_from(capsule_bytes.len()).unwrap(),
                16 * 1024 * 1024,
                || {
                    // This changes the addressed path after the complete scan but
                    // before the output Vec is allocated. The loader must use the
                    // exact same staged snapshot for classification and bytes.
                    let mut stored = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&generic_path)?;
                    stored.write_all(&capsule_bytes)?;
                    stored.set_len(u64::try_from(capsule_bytes.len()).unwrap())?;
                    stored.sync_all()?;
                    Ok(())
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(snapshot, generic_bytes);
        assert_eq!(sha256_hex(&snapshot), generic_hash);
    }
}
