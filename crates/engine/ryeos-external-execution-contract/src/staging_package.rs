//! Provider-neutral semantic inventory for one external guest staging package.
//!
//! This is not a filesystem extractor or a delivery receipt. The producer
//! enumerates exact inherited authorities through Lillux; the guest importer
//! must verify every streamed byte and materialize only through Lillux's
//! descriptor-relative, no-follow operations before starting the existing
//! fixed-FD supervisor. Neither a provider upload acknowledgement nor this
//! manifest alone authorizes Ready.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    ExternalGuestInputProjection, GuestMountContentAuthority, GuestMountKind, canonical_json,
    digest, from_json_slice_strict,
};

pub const GUEST_STAGING_PACKAGE_SCHEMA: u32 = 1;
pub const GUEST_IMPORT_TICKET_SCHEMA: u32 = 1;
// The base CAS transfer alone admits up to 400,010 filesystem entries. Other
// inputs share this package and must be accounted for by backend admission.
pub const MAX_GUEST_STAGING_ENTRIES: usize = 500_000;
pub const MAX_GUEST_STAGING_MANIFEST_BYTES: usize = 128 * 1024 * 1024;
pub const GUEST_STAGING_STREAM_MAGIC: &[u8; 16] = b"RYEOS-GUESTPKG-1";
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Retained semantic coordinates supplied independently of the stream. The
/// caller must source these from the committed activation and signed artifacts,
/// never from the package or an untrusted guest-supplied response.
pub struct GuestStagingExpected<'a> {
    pub inputs: &'a ExternalGuestInputProjection,
    pub activation_request_digest: &'a str,
    pub bootstrap_sha256: &'a str,
    pub supervisor_sha256: &'a str,
    pub launcher_sha256: &'a str,
    pub maximum_regular_bytes: u64,
    pub maximum_framed_bytes: u64,
}

/// Public, occurrence-bound expectations carried separately from the upload.
/// The controller derives this from its committed activation, exact prepared
/// package and signed artifact generation. It contains no bootstrap secret or
/// process-local descriptor. A ticket is not a Ready claim or a substitute for
/// the guest's full-package digest and staged-content checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestImportTicket {
    pub schema: u32,
    pub binding_hash: String,
    pub allocation_request_digest: String,
    pub occurrence_id: String,
    pub activation_request_digest: String,
    pub guest_input_identity: String,
    pub payload_sha256: String,
    pub manifest_sha256: String,
    pub framed_bytes: u64,
    pub regular_bytes: u64,
    pub bootstrap_sha256: String,
    pub supervisor_sha256: String,
    pub launcher_sha256: String,
    pub maximum_regular_bytes: u64,
    pub maximum_framed_bytes: u64,
}

/// Coordinates retained independently of the ticket and uploaded package.
/// The importer obtains these from its committed placement/activation, not
/// from values echoed by the upload or its caller.
pub struct GuestImportContext<'a> {
    pub binding_hash: &'a str,
    pub allocation_request_digest: &'a str,
    pub occurrence_id: &'a str,
    pub activation_request_digest: &'a str,
}

impl GuestImportTicket {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == GUEST_IMPORT_TICKET_SCHEMA,
            "unknown guest import ticket schema"
        );
        for (value, label) in [
            (&self.binding_hash, "binding"),
            (&self.allocation_request_digest, "allocation request"),
            (&self.activation_request_digest, "activation request"),
            (&self.guest_input_identity, "guest input"),
            (&self.payload_sha256, "package payload"),
            (&self.manifest_sha256, "package manifest"),
            (&self.bootstrap_sha256, "bootstrap"),
            (&self.supervisor_sha256, "supervisor"),
            (&self.launcher_sha256, "launcher"),
        ] {
            digest(value, label)?;
        }
        ensure!(
            !self.occurrence_id.is_empty()
                && self.occurrence_id.len() <= 512
                && self.occurrence_id.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                }),
            "guest import ticket occurrence is invalid"
        );
        ensure!(
            self.regular_bytes > 0
                && self.regular_bytes <= self.maximum_regular_bytes
                && self.framed_bytes >= self.regular_bytes.saturating_add(20)
                && self.framed_bytes <= self.maximum_framed_bytes
                && self.maximum_framed_bytes <= 4 * 1024 * 1024 * 1024,
            "guest import ticket package exceeds its bounds"
        );
        Ok(())
    }

    pub fn validate_for_context(&self, context: &GuestImportContext<'_>) -> Result<()> {
        self.validate()?;
        ensure!(
            self.binding_hash == context.binding_hash
                && self.allocation_request_digest == context.allocation_request_digest
                && self.occurrence_id == context.occurrence_id
                && self.activation_request_digest == context.activation_request_digest,
            "guest import ticket differs from retained placement"
        );
        Ok(())
    }

    /// After independently verifying the upload's exact length and full
    /// digest, join its decoded inventory to this separately carried ticket.
    pub fn validate_verified_manifest(
        &self,
        context: &GuestImportContext<'_>,
        manifest: &GuestStagingPackageManifest,
        observed_payload_sha256: &str,
        observed_framed_bytes: u64,
    ) -> Result<()> {
        self.validate_for_context(context)?;
        ensure!(
            observed_payload_sha256 == self.payload_sha256
                && observed_framed_bytes == self.framed_bytes
                && manifest.activation_request_digest == self.activation_request_digest
                && manifest.guest_input_identity == self.guest_input_identity
                && manifest.bootstrap_sha256 == self.bootstrap_sha256
                && manifest.supervisor_sha256 == self.supervisor_sha256
                && manifest.launcher_sha256 == self.launcher_sha256
                && manifest.total_regular_bytes == self.regular_bytes
                && manifest.framed_bytes()? == self.framed_bytes
                && manifest.identity_digest()? == self.manifest_sha256,
            "verified guest package differs from retained import ticket"
        );
        Ok(())
    }

    pub fn staging_expected<'a>(
        &'a self,
        context: &GuestImportContext<'_>,
        inputs: &'a ExternalGuestInputProjection,
    ) -> Result<GuestStagingExpected<'a>> {
        self.validate_for_context(context)?;
        ensure!(
            inputs.identity_digest()? == self.guest_input_identity,
            "guest import inputs differ from retained ticket"
        );
        Ok(GuestStagingExpected {
            inputs,
            activation_request_digest: &self.activation_request_digest,
            bootstrap_sha256: &self.bootstrap_sha256,
            supervisor_sha256: &self.supervisor_sha256,
            launcher_sha256: &self.launcher_sha256,
            maximum_regular_bytes: self.maximum_regular_bytes,
            maximum_framed_bytes: self.maximum_framed_bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestStagingPackageManifest {
    pub schema: u32,
    pub activation_request_digest: String,
    pub guest_input_identity: String,
    pub bootstrap_sha256: String,
    pub supervisor_sha256: String,
    pub launcher_sha256: String,
    pub total_regular_bytes: u64,
    pub entries: Vec<GuestStagingEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestStagingEntry {
    Directory {
        path: String,
        mode: u32,
    },
    RegularFile {
        path: String,
        mode: u32,
        bytes: u64,
        sha256: String,
    },
    /// Inert target bytes for an admitted external product. They are never
    /// followed while staging; product-manifest verification must prove the
    /// complete internal symlink graph before this tree can be installed.
    Symlink {
        path: String,
        target: String,
    },
}

impl GuestStagingEntry {
    pub fn path(&self) -> &str {
        match self {
            Self::Directory { path, .. }
            | Self::RegularFile { path, .. }
            | Self::Symlink { path, .. } => path,
        }
    }

    fn is_directory(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }
}

#[derive(Clone, Copy)]
enum RootKind<'a> {
    Directory {
        allows_symlinks: bool,
    },
    File {
        bytes: Option<u64>,
        hash: Option<&'a str>,
        mode: Option<u32>,
    },
}

impl GuestStagingPackageManifest {
    /// Digest of the exact typed manifest encoding carried on the package
    /// wire. Hashing a generic JSON value changes field order and therefore
    /// does not identify these bytes.
    pub fn identity_digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }

    /// Decode only a bounded, duplicate-key-free manifest. Callers must still
    /// validate it against the retained activation before reading entry bytes.
    pub fn from_bounded_json(bytes: &[u8]) -> Result<Self> {
        from_json_slice_strict(bytes, MAX_GUEST_STAGING_MANIFEST_BYTES)
    }

    /// Exact uncompressed wire length, including magic and manifest framing.
    /// Backend admission must compare this with its signed upload limit before
    /// staging or provider contact.
    pub fn framed_bytes(&self) -> Result<u64> {
        let manifest_bytes = canonical_json(self)?.len();
        ensure!(
            manifest_bytes <= MAX_GUEST_STAGING_MANIFEST_BYTES,
            "guest staging manifest exceeds its encoded bound"
        );
        20_u64
            .checked_add(u64::try_from(manifest_bytes)?)
            .and_then(|bytes| bytes.checked_add(self.total_regular_bytes))
            .ok_or_else(|| anyhow::anyhow!("guest staging framed byte count overflow"))
    }

    /// Validate the complete package inventory against the retained activation
    /// and guest projection. `maximum_regular_bytes` comes from the admitted
    /// placement binding; transport framing bytes require a separate bound.
    pub fn validate_for(
        &self,
        inputs: &ExternalGuestInputProjection,
        activation_request_digest: &str,
        bootstrap_sha256: &str,
        supervisor_sha256: &str,
        launcher_sha256: &str,
        maximum_regular_bytes: u64,
    ) -> Result<()> {
        inputs.validate()?;
        ensure!(
            self.schema == GUEST_STAGING_PACKAGE_SCHEMA,
            "guest staging schema changed"
        );
        for (value, label) in [
            (&self.activation_request_digest, "activation request"),
            (&self.guest_input_identity, "guest input identity"),
            (&self.bootstrap_sha256, "bootstrap"),
            (&self.supervisor_sha256, "supervisor"),
            (&self.launcher_sha256, "launcher"),
        ] {
            digest(value, label)?;
        }
        ensure!(
            self.activation_request_digest == activation_request_digest
                && self.guest_input_identity == inputs.identity_digest()?
                && self.bootstrap_sha256 == bootstrap_sha256
                && self.supervisor_sha256 == supervisor_sha256
                && self.launcher_sha256 == launcher_sha256,
            "guest staging identity changed"
        );
        ensure!(
            !self.entries.is_empty() && self.entries.len() <= MAX_GUEST_STAGING_ENTRIES,
            "guest staging entry count exceeds its bound"
        );

        let mut roots = BTreeMap::from([
            (
                "base".to_owned(),
                RootKind::Directory {
                    allows_symlinks: false,
                },
            ),
            (
                "bootstrap".to_owned(),
                RootKind::File {
                    bytes: None,
                    hash: Some(&self.bootstrap_sha256),
                    mode: None,
                },
            ),
            (
                "supervisor".to_owned(),
                RootKind::File {
                    bytes: None,
                    hash: Some(&self.supervisor_sha256),
                    mode: None,
                },
            ),
            (
                "launcher".to_owned(),
                RootKind::File {
                    bytes: None,
                    hash: Some(&self.launcher_sha256),
                    mode: None,
                },
            ),
        ]);
        if let Some(output) = &inputs.workspace_outputs {
            roots.insert(
                "workspace_outputs".to_owned(),
                RootKind::File {
                    bytes: Some(output.bytes),
                    hash: Some(&output.authority_hash),
                    mode: None,
                },
            );
        }
        for (index, input) in inputs.inputs.iter().enumerate() {
            if matches!(
                &input.content_authority,
                GuestMountContentAuthority::PrivateScratch { .. }
            ) {
                continue;
            }
            let kind = match input.kind {
                GuestMountKind::Directory => RootKind::Directory {
                    allows_symlinks: matches!(
                        &input.content_authority,
                        GuestMountContentAuthority::ProductManifest { .. }
                    ),
                },
                GuestMountKind::RegularFile => RootKind::File {
                    bytes: Some(input.bytes),
                    hash: match &input.content_authority {
                        GuestMountContentAuthority::RawFile { sha256 } => Some(sha256),
                        _ => None,
                    },
                    mode: input.normalized_mode,
                },
            };
            roots.insert(format!("input-{index:02}"), kind);
        }
        for (index, (_, hash, bytes)) in inputs.record_descriptors().enumerate() {
            roots.insert(
                format!("record-{index:02}"),
                RootKind::File {
                    bytes: Some(bytes),
                    hash: Some(hash),
                    mode: None,
                },
            );
        }

        let mut previous: Option<&str> = None;
        let mut directories = BTreeSet::new();
        let mut observed_roots = BTreeSet::new();
        let mut total = 0_u64;
        for entry in &self.entries {
            let path = entry.path();
            validate_package_path(path)?;
            ensure!(
                previous.is_none_or(|old| old < path),
                "guest staging entries are not uniquely sorted"
            );
            previous = Some(path);
            let (root, parent) = match path.rsplit_once('/') {
                Some((parent, _)) => (path.split('/').next().unwrap_or_default(), Some(parent)),
                None => (path, None),
            };
            let expected = roots
                .get(root)
                .ok_or_else(|| anyhow::anyhow!("guest staging has an undeclared root"))?;
            match parent {
                None => {
                    observed_roots.insert(root);
                    ensure!(
                        entry.is_directory() == matches!(expected, RootKind::Directory { .. }),
                        "guest staging root kind changed"
                    );
                    if root == "base" {
                        ensure!(
                            matches!(entry, GuestStagingEntry::Directory { mode: 0o700, .. }),
                            "guest base transfer root must remain owner-private"
                        );
                    }
                    if let (
                        RootKind::File { bytes, hash, mode },
                        GuestStagingEntry::RegularFile {
                            bytes: actual_bytes,
                            sha256,
                            mode: actual_mode,
                            ..
                        },
                    ) = (expected, entry)
                    {
                        ensure!(
                            bytes.is_none_or(|value| value == *actual_bytes)
                                && hash.is_none_or(|value| value == sha256)
                                && mode.is_none_or(|value| value == *actual_mode),
                            "guest staging root bytes or mode changed"
                        );
                    }
                }
                Some(parent) => {
                    ensure!(
                        matches!(expected, RootKind::Directory { .. }),
                        "guest staging descends through a file root"
                    );
                    ensure!(
                        directories.contains(parent),
                        "guest staging parent directory is absent"
                    );
                }
            }
            match entry {
                GuestStagingEntry::Directory { mode, .. } => {
                    ensure!(
                        matches!(*mode, 0o700 | 0o755),
                        "guest staging directory mode is invalid"
                    );
                    directories.insert(path);
                }
                GuestStagingEntry::RegularFile {
                    mode,
                    bytes,
                    sha256,
                    ..
                } => {
                    ensure!(
                        matches!(*mode, 0o600 | 0o644 | 0o700 | 0o755)
                            || (*mode == 0o500 && matches!(path, "supervisor" | "launcher")),
                        "guest staging file mode is invalid"
                    );
                    if matches!(path, "supervisor" | "launcher") {
                        ensure!(
                            matches!(*mode, 0o500 | 0o700 | 0o755),
                            "guest executable root lost its executable mode"
                        );
                    }
                    if path == "bootstrap" {
                        ensure!(*mode == 0o600, "guest bootstrap must remain owner-private");
                    }
                    digest(sha256, "guest staging file")?;
                    total = total
                        .checked_add(*bytes)
                        .ok_or_else(|| anyhow::anyhow!("guest staging byte count overflow"))?;
                    ensure!(
                        total <= maximum_regular_bytes,
                        "guest staging transfer budget exceeded"
                    );
                }
                GuestStagingEntry::Symlink { target, .. } => {
                    ensure!(
                        parent.is_some()
                            && matches!(
                                expected,
                                RootKind::Directory {
                                    allows_symlinks: true
                                }
                            )
                            && !target.is_empty()
                            && target.len() <= 4096
                            && !target.as_bytes().contains(&0),
                        "guest staging symlink is not an admitted product entry"
                    );
                }
            }
        }
        ensure!(
            observed_roots.len() == roots.len()
                && roots
                    .keys()
                    .all(|root| observed_roots.contains(root.as_str())),
            "guest staging root inventory is incomplete"
        );
        ensure!(
            self.total_regular_bytes == total,
            "guest staging byte total changed"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_GUEST_STAGING_MANIFEST_BYTES,
            "guest staging manifest exceeds its encoded bound"
        );
        Ok(())
    }
}

/// Write the canonical inventory header before any entry payload. Each
/// regular file's bytes then follows in manifest order, without compression or
/// implicit path headers. The producer must validate and retain the complete
/// private package before provider upload; this function is not publication.
fn write_staging_header<W: Write>(
    writer: &mut W,
    manifest: &GuestStagingPackageManifest,
    expected: &GuestStagingExpected<'_>,
) -> Result<()> {
    manifest.validate_for(
        expected.inputs,
        expected.activation_request_digest,
        expected.bootstrap_sha256,
        expected.supervisor_sha256,
        expected.launcher_sha256,
        expected.maximum_regular_bytes,
    )?;
    ensure!(
        manifest.framed_bytes()? <= expected.maximum_framed_bytes,
        "guest staging framed transfer budget exceeded"
    );
    let encoded = canonical_json(manifest)?;
    let length = u32::try_from(encoded.len())?;
    writer.write_all(GUEST_STAGING_STREAM_MAGIC)?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&encoded)?;
    Ok(())
}

/// Read a bounded canonical inventory before accepting any payload bytes.
/// Consumers must still verify every entry and the exact base/content closure
/// in a private guest staging area before installing supervisor descriptors.
fn read_staging_header<R: Read>(
    reader: &mut R,
    expected: &GuestStagingExpected<'_>,
) -> Result<GuestStagingPackageManifest> {
    let mut magic = [0_u8; 16];
    reader.read_exact(&mut magic)?;
    ensure!(
        &magic == GUEST_STAGING_STREAM_MAGIC,
        "guest staging stream magic changed"
    );
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_be_bytes(length))?;
    ensure!(
        length > 0 && length <= MAX_GUEST_STAGING_MANIFEST_BYTES,
        "guest staging header length is invalid"
    );
    let mut encoded = vec![0_u8; length];
    reader.read_exact(&mut encoded)?;
    let manifest = GuestStagingPackageManifest::from_bounded_json(&encoded)?;
    ensure!(
        canonical_json(&manifest)? == encoded,
        "guest staging header is noncanonical"
    );
    manifest.validate_for(
        expected.inputs,
        expected.activation_request_digest,
        expected.bootstrap_sha256,
        expected.supervisor_sha256,
        expected.launcher_sha256,
        expected.maximum_regular_bytes,
    )?;
    ensure!(
        manifest.framed_bytes()? <= expected.maximum_framed_bytes,
        "guest staging framed transfer budget exceeded"
    );
    Ok(manifest)
}

/// Copy one declared file through a bounded buffer while checking its exact
/// size and digest. A digest failure leaves `writer` contaminated; it must be
/// private, unpublished staging and discarded by the caller on any error.
fn copy_staging_file<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    entry: &GuestStagingEntry,
) -> Result<()> {
    let GuestStagingEntry::RegularFile { bytes, sha256, .. } = entry else {
        bail!("guest staging payload requested for a non-file entry");
    };
    let mut remaining = *bytes;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; STREAM_CHUNK_BYTES];
    while remaining > 0 {
        let length = usize::try_from(remaining.min(STREAM_CHUNK_BYTES as u64))?;
        reader.read_exact(&mut buffer[..length])?;
        digest.update(&buffer[..length]);
        writer.write_all(&buffer[..length])?;
        remaining -= u64::try_from(length)?;
    }
    ensure!(
        hex::encode(digest.finalize()) == *sha256,
        "guest staging file digest changed"
    );
    Ok(())
}

/// Refuse undeclared trailing bytes after all manifest payloads were copied.
/// A caller-owned deadline must bound a slow or stalled stream.
fn require_staging_eof<R: Read>(reader: &mut R) -> Result<()> {
    let mut trailing = [0_u8; 1];
    ensure!(
        reader.read(&mut trailing)? == 0,
        "guest staging stream has trailing bytes"
    );
    Ok(())
}

fn next_regular_entry(
    manifest: &GuestStagingPackageManifest,
    start: usize,
) -> Option<(usize, &GuestStagingEntry)> {
    manifest
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, entry)| matches!(entry, GuestStagingEntry::RegularFile { .. }))
}

/// Sequential producer over a private package file. A failed copy permanently
/// aborts the writer; the caller must discard the contaminated package.
#[must_use = "finish every declared file before using the private package"]
pub struct GuestStagingStreamWriter<W: Write> {
    writer: W,
    manifest: GuestStagingPackageManifest,
    next_index: usize,
    failed: bool,
}

impl<W: Write> GuestStagingStreamWriter<W> {
    pub fn new(
        mut writer: W,
        manifest: GuestStagingPackageManifest,
        expected: &GuestStagingExpected<'_>,
    ) -> Result<Self> {
        write_staging_header(&mut writer, &manifest, expected)?;
        Ok(Self {
            writer,
            manifest,
            next_index: 0,
            failed: false,
        })
    }

    pub fn next_file(&self) -> Option<&GuestStagingEntry> {
        next_regular_entry(&self.manifest, self.next_index).map(|(_, entry)| entry)
    }

    pub fn copy_next_file<R: Read>(&mut self, reader: &mut R) -> Result<()> {
        ensure!(!self.failed, "guest staging writer was aborted");
        let (index, entry) = next_regular_entry(&self.manifest, self.next_index)
            .ok_or_else(|| anyhow::anyhow!("guest staging has no remaining file"))?;
        let result = copy_staging_file(reader, &mut self.writer, entry)
            .and_then(|()| require_staging_eof(reader));
        match result {
            Ok(()) => {
                self.next_index = index + 1;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub fn finish(self) -> Result<W> {
        ensure!(
            !self.failed && next_regular_entry(&self.manifest, self.next_index).is_none(),
            "guest staging writer did not complete its inventory"
        );
        Ok(self.writer)
    }
}

/// Sequential consumer over a retained immutable package file. Its writer for
/// each entry must be an unpublished private file. Neither successful `finish`
/// nor a provider upload receipt replaces base/content authority verification.
#[must_use = "finish every declared file and check EOF before using staged input"]
pub struct GuestStagingStreamReader<R: Read> {
    reader: R,
    manifest: GuestStagingPackageManifest,
    next_index: usize,
    failed: bool,
}

impl<R: Read> GuestStagingStreamReader<R> {
    pub fn new(mut reader: R, expected: &GuestStagingExpected<'_>) -> Result<Self> {
        let manifest = read_staging_header(&mut reader, expected)?;
        Ok(Self {
            reader,
            manifest,
            next_index: 0,
            failed: false,
        })
    }

    pub fn manifest(&self) -> &GuestStagingPackageManifest {
        &self.manifest
    }

    pub fn next_file(&self) -> Option<&GuestStagingEntry> {
        next_regular_entry(&self.manifest, self.next_index).map(|(_, entry)| entry)
    }

    pub fn copy_next_file<W: Write>(&mut self, writer: &mut W) -> Result<()> {
        ensure!(!self.failed, "guest staging reader was aborted");
        let (index, entry) = next_regular_entry(&self.manifest, self.next_index)
            .ok_or_else(|| anyhow::anyhow!("guest staging has no remaining file"))?;
        let result = copy_staging_file(&mut self.reader, writer, entry);
        match result {
            Ok(()) => {
                self.next_index = index + 1;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub fn finish(mut self) -> Result<R> {
        ensure!(
            !self.failed && next_regular_entry(&self.manifest, self.next_index).is_none(),
            "guest staging reader did not complete its inventory"
        );
        require_staging_eof(&mut self.reader)?;
        Ok(self.reader)
    }
}

fn validate_package_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path.split('/').count() <= 32
            && path.split('/').all(|part| {
                !part.is_empty() && part.len() <= 255 && part != "." && part != ".."
            }),
        "guest staging path is not normalized"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, GuestBaseSnapshotInput, GuestMountAccess,
        GuestMountInput, GuestMountRole, GuestProductManifestKind,
    };

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn fixture() -> (ExternalGuestInputProjection, GuestStagingPackageManifest) {
        let inputs = ExternalGuestInputProjection {
            schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 56,
                snapshot_hash: hash('a'),
                closure_digest: hash('b'),
                object_count: 3,
                blob_count: 1,
                total_bytes: 2,
            },
            workspace_outputs: None,
            inputs: vec![GuestMountInput {
                role: GuestMountRole::Configuration,
                authority_id: "config".to_owned(),
                descriptor: 64,
                destination: "/runtime/config".to_owned(),
                kind: GuestMountKind::RegularFile,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: Some(0o644),
                content_authority: GuestMountContentAuthority::RawFile { sha256: hash('c') },
                bytes: 3,
            }],
            executable_search: vec![],
            environment: BTreeMap::new(),
        };
        let entries = vec![
            GuestStagingEntry::Directory {
                path: "base".to_owned(),
                mode: 0o700,
            },
            GuestStagingEntry::RegularFile {
                path: "base/object".to_owned(),
                mode: 0o600,
                bytes: 2,
                sha256: hash('d'),
            },
            GuestStagingEntry::RegularFile {
                path: "bootstrap".to_owned(),
                mode: 0o600,
                bytes: 4,
                sha256: hash('e'),
            },
            GuestStagingEntry::RegularFile {
                path: "input-00".to_owned(),
                mode: 0o644,
                bytes: 3,
                sha256: hash('c'),
            },
            GuestStagingEntry::RegularFile {
                path: "launcher".to_owned(),
                mode: 0o755,
                bytes: 5,
                sha256: hash('f'),
            },
            GuestStagingEntry::RegularFile {
                path: "supervisor".to_owned(),
                mode: 0o755,
                bytes: 6,
                sha256: hash('0'),
            },
        ];
        let manifest = GuestStagingPackageManifest {
            schema: GUEST_STAGING_PACKAGE_SCHEMA,
            activation_request_digest: hash('1'),
            guest_input_identity: inputs.identity_digest().unwrap(),
            bootstrap_sha256: hash('e'),
            supervisor_sha256: hash('0'),
            launcher_sha256: hash('f'),
            total_regular_bytes: 20,
            entries,
        };
        (inputs, manifest)
    }

    fn validates(
        inputs: &ExternalGuestInputProjection,
        package: &GuestStagingPackageManifest,
    ) -> bool {
        package
            .validate_for(inputs, &hash('1'), &hash('e'), &hash('0'), &hash('f'), 20)
            .is_ok()
    }

    #[test]
    fn independently_carried_import_ticket_binds_exact_verified_package() {
        let (inputs, manifest) = fixture();
        let ticket = GuestImportTicket {
            schema: GUEST_IMPORT_TICKET_SCHEMA,
            binding_hash: hash('3'),
            allocation_request_digest: hash('4'),
            occurrence_id: "occ-fixture".into(),
            activation_request_digest: manifest.activation_request_digest.clone(),
            guest_input_identity: manifest.guest_input_identity.clone(),
            payload_sha256: hash('2'),
            manifest_sha256: hex::encode(Sha256::digest(canonical_json(&manifest).unwrap())),
            framed_bytes: manifest.framed_bytes().unwrap(),
            regular_bytes: manifest.total_regular_bytes,
            bootstrap_sha256: manifest.bootstrap_sha256.clone(),
            supervisor_sha256: manifest.supervisor_sha256.clone(),
            launcher_sha256: manifest.launcher_sha256.clone(),
            maximum_regular_bytes: 1024,
            maximum_framed_bytes: 4096,
        };
        let context = GuestImportContext {
            binding_hash: &ticket.binding_hash,
            allocation_request_digest: &ticket.allocation_request_digest,
            occurrence_id: &ticket.occurrence_id,
            activation_request_digest: &ticket.activation_request_digest,
        };
        ticket.validate().unwrap();
        ticket.staging_expected(&context, &inputs).unwrap();
        ticket
            .validate_verified_manifest(&context, &manifest, &hash('2'), ticket.framed_bytes)
            .unwrap();

        let mut wrong = ticket.clone();
        wrong.occurrence_id = "../other".into();
        assert!(wrong.validate().is_err());
        wrong.occurrence_id = "occ-other".into();
        assert!(wrong.staging_expected(&context, &inputs).is_err());
        wrong = ticket.clone();
        wrong.binding_hash = hash('7');
        assert!(wrong.staging_expected(&context, &inputs).is_err());
        wrong = ticket.clone();
        wrong.allocation_request_digest = hash('8');
        assert!(wrong.staging_expected(&context, &inputs).is_err());
        wrong = ticket.clone();
        wrong.activation_request_digest = hash('9');
        assert!(wrong.staging_expected(&context, &inputs).is_err());
        let mut wrong = ticket.clone();
        wrong.framed_bytes += 1;
        assert!(
            wrong
                .validate_verified_manifest(&context, &manifest, &hash('2'), wrong.framed_bytes)
                .is_err()
        );
        let mut changed_manifest = manifest.clone();
        changed_manifest.launcher_sha256 = hash('5');
        assert!(
            ticket
                .validate_verified_manifest(
                    &context,
                    &changed_manifest,
                    &hash('2'),
                    ticket.framed_bytes
                )
                .is_err()
        );
        assert!(
            ticket
                .validate_verified_manifest(&context, &manifest, &hash('6'), ticket.framed_bytes)
                .is_err()
        );
    }

    #[test]
    fn sealed_executable_capture_mode_is_limited_to_launcher_and_supervisor() {
        let (inputs, mut package) = fixture();
        for entry in &mut package.entries {
            if let GuestStagingEntry::RegularFile { path, mode, .. } = entry
                && matches!(path.as_str(), "launcher" | "supervisor")
            {
                *mode = 0o500;
            }
        }
        assert!(validates(&inputs, &package));
        for entry in &mut package.entries {
            if let GuestStagingEntry::RegularFile { path, mode, .. } = entry
                && path == "launcher"
            {
                *mode = 0o400;
            }
        }
        assert!(!validates(&inputs, &package));
        for mode in [0o600, 0o644] {
            let (inputs, mut package) = fixture();
            for entry in &mut package.entries {
                if let GuestStagingEntry::RegularFile {
                    path,
                    mode: entry_mode,
                    ..
                } = entry
                    && path == "supervisor"
                {
                    *entry_mode = mode;
                }
            }
            assert!(!validates(&inputs, &package));
        }
        let (inputs, mut package) = fixture();
        for entry in &mut package.entries {
            if let GuestStagingEntry::RegularFile { path, mode, .. } = entry
                && path == "bootstrap"
            {
                *mode = 0o644;
            }
        }
        assert!(!validates(&inputs, &package));
    }

    #[test]
    fn accepts_exact_inventory_and_refuses_mutation() {
        let (inputs, mut package) = fixture();
        assert!(validates(&inputs, &package));

        package.entries[3] = GuestStagingEntry::RegularFile {
            path: "input-00".to_owned(),
            mode: 0o755,
            bytes: 3,
            sha256: hash('c'),
        };
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries[3] = GuestStagingEntry::RegularFile {
            path: "input-00".to_owned(),
            mode: 0o644,
            bytes: 3,
            sha256: hash('d'),
        };
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries.remove(3);
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries.push(GuestStagingEntry::Directory {
            path: "undeclared".to_owned(),
            mode: 0o700,
        });
        assert!(!validates(&inputs, &package));
    }

    #[test]
    fn refuses_unsafe_paths_order_and_budget() {
        let (inputs, mut package) = fixture();
        package.entries[0] = GuestStagingEntry::Directory {
            path: "base".to_owned(),
            mode: 0o755,
        };
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries[1] = GuestStagingEntry::RegularFile {
            path: "base/../object".to_owned(),
            mode: 0o600,
            bytes: 2,
            sha256: hash('d'),
        };
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.entries.swap(1, 2);
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        package.total_regular_bytes = 21;
        assert!(!validates(&inputs, &package));
        package = fixture().1;
        assert!(
            package
                .validate_for(&inputs, &hash('1'), &hash('e'), &hash('0'), &hash('f'), 19)
                .is_err()
        );
        package = fixture().1;
        assert!(
            package
                .validate_for(&inputs, &hash('1'), &hash('d'), &hash('0'), &hash('f'), 20)
                .is_err()
        );
    }

    #[test]
    fn product_symlinks_have_no_payload_and_cannot_enter_other_roots() {
        let (mut inputs, mut package) = fixture();
        let input = &mut inputs.inputs[0];
        input.role = GuestMountRole::Product;
        input.kind = GuestMountKind::Directory;
        input.normalized_mode = None;
        input.bytes = 2;
        input.content_authority = GuestMountContentAuthority::ProductManifest {
            manifest_kind: GuestProductManifestKind::Content,
            manifest_hash: hash('4'),
            manifest_descriptor: 65,
            manifest_bytes: 3,
        };
        package.guest_input_identity = inputs.identity_digest().unwrap();
        package.entries.splice(
            3..4,
            [
                GuestStagingEntry::Directory {
                    path: "input-00".to_owned(),
                    mode: 0o700,
                },
                GuestStagingEntry::Directory {
                    path: "input-00/bin".to_owned(),
                    mode: 0o755,
                },
                GuestStagingEntry::RegularFile {
                    path: "input-00/bin/tool".to_owned(),
                    mode: 0o755,
                    bytes: 2,
                    sha256: hash('d'),
                },
                GuestStagingEntry::Symlink {
                    path: "input-00/current".to_owned(),
                    target: "bin/tool".to_owned(),
                },
            ],
        );
        package.entries.insert(
            package.entries.len() - 1,
            GuestStagingEntry::RegularFile {
                path: "record-00".to_owned(),
                mode: 0o600,
                bytes: 3,
                sha256: hash('4'),
            },
        );
        package.total_regular_bytes = 22;
        let valid = |package: &GuestStagingPackageManifest| {
            package
                .validate_for(&inputs, &hash('1'), &hash('e'), &hash('0'), &hash('f'), 22)
                .is_ok()
        };
        assert!(valid(&package));
        assert_eq!(
            package
                .entries
                .iter()
                .filter(|entry| matches!(entry, GuestStagingEntry::RegularFile { .. }))
                .count(),
            6
        );
        let mut changed = package.clone();
        changed.entries[6] = GuestStagingEntry::Symlink {
            path: "input-00/current".to_owned(),
            target: "".to_owned(),
        };
        assert!(!valid(&changed));
        let mut changed = package.clone();
        changed.entries[6] = GuestStagingEntry::Symlink {
            path: "input-00/current".to_owned(),
            target: "x".repeat(4097),
        };
        assert!(!valid(&changed));
        let (ordinary, mut nonproduct) = fixture();
        nonproduct.entries.insert(
            2,
            GuestStagingEntry::Symlink {
                path: "base/link".to_owned(),
                target: "object".to_owned(),
            },
        );
        assert!(!validates(&ordinary, &nonproduct));
    }

    #[test]
    fn bounded_decoder_refuses_duplicate_keys() {
        let (_, package) = fixture();
        let encoded = canonical_json(&package).unwrap();
        assert_eq!(
            GuestStagingPackageManifest::from_bounded_json(&encoded).unwrap(),
            package
        );
        assert!(
            GuestStagingPackageManifest::from_bounded_json(b"{\"schema\":1,\"schema\":2}").is_err()
        );
    }

    #[test]
    fn framed_stream_round_trips_and_refuses_truncation_tampering_and_trailing_bytes() {
        let (mut inputs, mut manifest) = fixture();
        let payloads: [&[u8]; 5] = [b"bb", b"boot", b"cfg", b"start", b"runrun"];
        for (entry, payload) in manifest
            .entries
            .iter_mut()
            .filter(|entry| !entry.is_directory())
            .zip(payloads)
        {
            let GuestStagingEntry::RegularFile { sha256, .. } = entry else {
                unreachable!()
            };
            *sha256 = hex::encode(Sha256::digest(payload));
        }
        let GuestMountContentAuthority::RawFile { sha256 } =
            &mut inputs.inputs[0].content_authority
        else {
            unreachable!()
        };
        *sha256 = hex::encode(Sha256::digest(payloads[2]));
        manifest.guest_input_identity = inputs.identity_digest().unwrap();
        manifest.bootstrap_sha256 = hex::encode(Sha256::digest(payloads[1]));
        manifest.launcher_sha256 = hex::encode(Sha256::digest(payloads[3]));
        manifest.supervisor_sha256 = hex::encode(Sha256::digest(payloads[4]));
        let expected = GuestStagingExpected {
            inputs: &inputs,
            activation_request_digest: &manifest.activation_request_digest,
            bootstrap_sha256: &manifest.bootstrap_sha256,
            supervisor_sha256: &manifest.supervisor_sha256,
            launcher_sha256: &manifest.launcher_sha256,
            maximum_regular_bytes: 20,
            maximum_framed_bytes: manifest.framed_bytes().unwrap(),
        };

        let writer =
            GuestStagingStreamWriter::new(Vec::new(), manifest.clone(), &expected).unwrap();
        assert!(writer.finish().is_err());
        let mut writer =
            GuestStagingStreamWriter::new(Vec::new(), manifest.clone(), &expected).unwrap();
        for payload in payloads {
            assert!(writer.next_file().is_some());
            writer.copy_next_file(&mut payload.as_ref()).unwrap();
        }
        assert!(writer.next_file().is_none());
        let stream = writer.finish().unwrap();
        assert_eq!(
            u64::try_from(stream.len()).unwrap(),
            manifest.framed_bytes().unwrap()
        );
        let mut writer =
            GuestStagingStreamWriter::new(Vec::new(), manifest.clone(), &expected).unwrap();
        assert!(writer.copy_next_file(&mut b"bbx".as_slice()).is_err());
        assert!(writer.finish().is_err());
        let header_end = stream.len() - 20;
        let mut reader = GuestStagingStreamReader::new(stream.as_slice(), &expected).unwrap();
        assert_eq!(reader.manifest(), &manifest);
        for payload in payloads {
            let mut observed = Vec::new();
            reader.copy_next_file(&mut observed).unwrap();
            assert_eq!(observed, payload);
        }
        assert!(reader.next_file().is_none());
        reader.finish().unwrap();

        assert!(GuestStagingStreamReader::new(&stream[..header_end - 1], &expected).is_err());
        let mut truncated =
            GuestStagingStreamReader::new(&stream[..stream.len() - 1], &expected).unwrap();
        for _ in 0..4 {
            truncated.copy_next_file(&mut Vec::new()).unwrap();
        }
        assert!(truncated.copy_next_file(&mut Vec::new()).is_err());
        assert!(truncated.copy_next_file(&mut Vec::new()).is_err());
        assert!(truncated.finish().is_err());

        let mut tampered = stream.clone();
        tampered[header_end] ^= 1;
        let mut reader = GuestStagingStreamReader::new(tampered.as_slice(), &expected).unwrap();
        assert!(reader.copy_next_file(&mut Vec::new()).is_err());

        let mut trailing = stream;
        trailing.push(1);
        let mut reader = GuestStagingStreamReader::new(trailing.as_slice(), &expected).unwrap();
        for _ in 0..5 {
            reader.copy_next_file(&mut Vec::new()).unwrap();
        }
        assert!(reader.finish().is_err());

        let mut wrong_magic = trailing.clone();
        wrong_magic[0] ^= 1;
        assert!(GuestStagingStreamReader::new(wrong_magic.as_slice(), &expected).is_err());
        let mut excessive_header = trailing.clone();
        excessive_header[16..20].copy_from_slice(
            &u32::try_from(MAX_GUEST_STAGING_MANIFEST_BYTES + 1)
                .unwrap()
                .to_be_bytes(),
        );
        assert!(GuestStagingStreamReader::new(excessive_header.as_slice(), &expected).is_err());

        struct RefuseWrite;
        impl Write for RefuseWrite {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("staging write refused"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut reader = GuestStagingStreamReader::new(trailing.as_slice(), &expected).unwrap();
        assert!(reader.copy_next_file(&mut RefuseWrite).is_err());
        assert!(reader.copy_next_file(&mut Vec::new()).is_err());
        assert!(reader.finish().is_err());
    }

    #[test]
    fn zero_length_file_requires_empty_digest() {
        let entry = GuestStagingEntry::RegularFile {
            path: "empty".to_owned(),
            mode: 0o600,
            bytes: 0,
            sha256: hex::encode(Sha256::digest([])),
        };
        copy_staging_file(&mut &[][..], &mut Vec::new(), &entry).unwrap();
        let GuestStagingEntry::RegularFile { sha256, .. } = &entry else {
            unreachable!()
        };
        let mut changed = entry.clone();
        if let GuestStagingEntry::RegularFile { sha256: digest, .. } = &mut changed {
            *digest = hash('a');
        }
        assert_ne!(sha256, &hash('a'));
        assert!(copy_staging_file(&mut &[][..], &mut Vec::new(), &changed).is_err());
    }
}
