//! Signed pictures a bundle ships.
//!
//! A picture is a signed `picture` record (YAML under `.ai/pictures`) naming
//! one or more renditions: image bytes stored under `.ai/pictures/blobs`, each
//! named by the SHA-256 of its bytes. The publisher covers every blob with a
//! signed picture manifest (`.ai/refs/bundles/pictures`, a signed ref to a
//! source manifest whose item sources are `pictures/<sha256>`), so bytes that
//! are not named by a signed record, or records whose bytes changed, never
//! verify.
//!
//! This module holds the shared mechanism: the record shape, the pure-Rust
//! image header inspection, the publisher-side source check, and the
//! install/serve-side verification of an installed bundle.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use ryeos_engine::contracts::{SignatureEnvelope, TrustClass};
use ryeos_engine::executor_resolution::{
    MAX_EXECUTOR_MANIFEST_REF_BYTES, PICTURE_MANIFEST_REF_DOMAIN, verify_executor_manifest_object,
    verify_picture_item_source, verify_signed_manifest_ref_in_domain,
};
use ryeos_engine::item_resolution::parse_signature_header;
use ryeos_engine::trust::{SignerContext, TrustStore};
use serde::Deserialize;

/// Directory under `.ai` holding picture records and their blobs.
pub const PICTURES_DIR: &str = "pictures";
/// Directory under `.ai/pictures` holding the image bytes.
pub const PICTURE_BLOBS_DIR: &str = "blobs";
/// Prefix of every picture item ref in the picture manifest.
pub const PICTURE_ITEM_REF_PREFIX: &str = "pictures/";

/// Largest picture blob a bundle may ship or the daemon will serve.
pub const MAX_PICTURE_BYTES: u64 = 2 * 1024 * 1024;
/// Largest width or height of a picture.
pub const MAX_PICTURE_DIMENSION: u32 = 4096;
/// Largest pixel count (width times height) of a picture.
pub const MAX_PICTURE_PIXELS: u64 = 16_000_000;
/// Largest signed record file read.
const MAX_RECORD_BYTES: u64 = 256 * 1024;
/// Largest picture-manifest object and item-source object read.
const MAX_MANIFEST_OBJECT_BYTES: u64 = 1024 * 1024;
const MAX_ITEM_SOURCE_OBJECT_BYTES: u64 = 64 * 1024;
/// Most renditions one record may name.
const MAX_RENDITIONS_PER_RECORD: usize = 8;

/// Licences a picture may declare. The `picture` kind schema carries the same
/// list as its `licence` enum; a test pins the two together.
pub const ACCEPTED_LICENCES: [&str; 2] = ["cc0", "public-domain"];

/// Image formats a picture may be stored in. A mechanical choice of decoder,
/// not policy: SVG and GIF are not images the client draws as pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PictureMediaType {
    Png,
    Jpeg,
    Webp,
}

impl PictureMediaType {
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    /// File extension of a blob stored in this format.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    pub fn from_mime(value: &str) -> Option<Self> {
        match value {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }

    fn from_extension(value: &str) -> Option<Self> {
        match value {
            "png" => Some(Self::Png),
            "jpg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }
}

/// Dimensions read from an image header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageFacts {
    pub width: u32,
    pub height: u32,
}

/// Why image bytes were refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRefusal {
    Empty,
    TooLarge {
        bytes: u64,
        max: u64,
    },
    /// The bytes do not start with the magic of the declared type.
    MagicMismatch {
        declared: &'static str,
    },
    /// The image is animated or carries animation chunks.
    Animated,
    Malformed(&'static str),
    DimensionsOutOfBounds {
        width: u32,
        height: u32,
    },
}

impl ImageRefusal {
    /// Stable machine-readable name of the refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLarge { .. } => "too_large",
            Self::MagicMismatch { .. } => "magic_mismatch",
            Self::Animated => "animated",
            Self::Malformed(_) => "malformed",
            Self::DimensionsOutOfBounds { .. } => "dimensions_out_of_bounds",
        }
    }
}

impl std::fmt::Display for ImageRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "image is empty"),
            Self::TooLarge { bytes, max } => {
                write!(f, "image is {bytes} bytes, over the {max} byte cap")
            }
            Self::MagicMismatch { declared } => {
                write!(f, "image bytes are not {declared}")
            }
            Self::Animated => write!(f, "animated images are refused"),
            Self::Malformed(detail) => write!(f, "malformed image: {detail}"),
            Self::DimensionsOutOfBounds { width, height } => {
                write!(f, "image is {width}x{height}, outside the size caps")
            }
        }
    }
}

impl std::error::Error for ImageRefusal {}

/// Check image bytes against the declared type and the size caps, reading
/// only headers. Never decodes pixels.
pub fn inspect_image(bytes: &[u8], declared: PictureMediaType) -> Result<ImageFacts, ImageRefusal> {
    if bytes.is_empty() {
        return Err(ImageRefusal::Empty);
    }
    let length = bytes.len() as u64;
    if length > MAX_PICTURE_BYTES {
        return Err(ImageRefusal::TooLarge {
            bytes: length,
            max: MAX_PICTURE_BYTES,
        });
    }
    let (width, height) = match declared {
        PictureMediaType::Png => inspect_png(bytes)?,
        PictureMediaType::Jpeg => inspect_jpeg(bytes)?,
        PictureMediaType::Webp => inspect_webp(bytes)?,
    };
    if width == 0
        || height == 0
        || width > MAX_PICTURE_DIMENSION
        || height > MAX_PICTURE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PICTURE_PIXELS
    {
        return Err(ImageRefusal::DimensionsOutOfBounds { width, height });
    }
    Ok(ImageFacts { width, height })
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn inspect_png(bytes: &[u8]) -> Result<(u32, u32), ImageRefusal> {
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return Err(ImageRefusal::MagicMismatch {
            declared: PictureMediaType::Png.mime(),
        });
    }
    let mut offset = PNG_SIGNATURE.len();
    let mut dimensions = None;
    let mut saw_data = false;
    loop {
        let header = bytes
            .get(offset..offset + 8)
            .ok_or(ImageRefusal::Malformed("truncated png chunk header"))?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let data_start = offset + 8;
        let data_end = data_start
            .checked_add(length)
            .ok_or(ImageRefusal::Malformed("png chunk length overflows"))?;
        let chunk_end = data_end
            .checked_add(4)
            .ok_or(ImageRefusal::Malformed("png chunk length overflows"))?;
        if chunk_end > bytes.len() {
            return Err(ImageRefusal::Malformed("truncated png chunk"));
        }
        match &header[4..8] {
            b"IHDR" => {
                if offset != PNG_SIGNATURE.len() || length != 13 {
                    return Err(ImageRefusal::Malformed("png header chunk is not first"));
                }
                let data = &bytes[data_start..data_end];
                dimensions = Some((
                    u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
                    u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
                ));
            }
            b"acTL" | b"fcTL" | b"fdAT" => return Err(ImageRefusal::Animated),
            b"IDAT" => saw_data = true,
            b"IEND" => {
                if chunk_end != bytes.len() {
                    return Err(ImageRefusal::Malformed("bytes follow the png end chunk"));
                }
                break;
            }
            _ => {}
        }
        if dimensions.is_none() {
            return Err(ImageRefusal::Malformed("png header chunk is not first"));
        }
        offset = chunk_end;
    }
    if !saw_data {
        return Err(ImageRefusal::Malformed("png has no image data"));
    }
    dimensions.ok_or(ImageRefusal::Malformed("png has no header chunk"))
}

fn inspect_jpeg(bytes: &[u8]) -> Result<(u32, u32), ImageRefusal> {
    if !bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Err(ImageRefusal::MagicMismatch {
            declared: PictureMediaType::Jpeg.mime(),
        });
    }
    if !bytes.ends_with(&[0xFF, 0xD9]) {
        return Err(ImageRefusal::Malformed("jpeg has no end marker"));
    }
    let mut offset = 2usize;
    loop {
        if bytes.get(offset) != Some(&0xFF) {
            return Err(ImageRefusal::Malformed("jpeg marker expected"));
        }
        while bytes.get(offset + 1) == Some(&0xFF) {
            offset += 1;
        }
        let marker = *bytes
            .get(offset + 1)
            .ok_or(ImageRefusal::Malformed("truncated jpeg marker"))?;
        offset += 2;
        match marker {
            0x01 | 0xD0..=0xD7 => continue,
            0xD8 | 0xD9 | 0xDA => {
                return Err(ImageRefusal::Malformed("jpeg has no frame header"));
            }
            _ => {}
        }
        let length_bytes = bytes
            .get(offset..offset + 2)
            .ok_or(ImageRefusal::Malformed("truncated jpeg segment"))?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        let segment_end = offset + length;
        if length < 2 || segment_end > bytes.len() {
            return Err(ImageRefusal::Malformed("jpeg segment length is invalid"));
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            if length < 8 {
                return Err(ImageRefusal::Malformed("jpeg frame header is short"));
            }
            let height = u32::from(u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]));
            let width = u32::from(u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]));
            return Ok((width, height));
        }
        offset = segment_end;
    }
}

fn webp_le24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

fn inspect_webp(bytes: &[u8]) -> Result<(u32, u32), ImageRefusal> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(ImageRefusal::MagicMismatch {
            declared: PictureMediaType::Webp.mime(),
        });
    }
    let declared = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if declared.checked_add(8) != Some(bytes.len()) {
        return Err(ImageRefusal::Malformed("riff size differs from file size"));
    }
    let mut offset = 12usize;
    let mut dimensions: Option<(u32, u32)> = None;
    let mut set = |found: (u32, u32)| -> Result<(), ImageRefusal> {
        match dimensions {
            Some(previous) if previous != found => Err(ImageRefusal::Malformed(
                "webp chunks disagree on the image size",
            )),
            _ => {
                dimensions = Some(found);
                Ok(())
            }
        }
    };
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 8)
            .ok_or(ImageRefusal::Malformed("truncated webp chunk header"))?;
        let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let data_start = offset + 8;
        let data_end = data_start
            .checked_add(size)
            .ok_or(ImageRefusal::Malformed("webp chunk length overflows"))?;
        if data_end > bytes.len() {
            return Err(ImageRefusal::Malformed("truncated webp chunk"));
        }
        let data = &bytes[data_start..data_end];
        match &header[0..4] {
            b"ANIM" | b"ANMF" => return Err(ImageRefusal::Animated),
            b"VP8X" => {
                if size < 10 {
                    return Err(ImageRefusal::Malformed("webp extended header is short"));
                }
                if data[0] & 0x02 != 0 {
                    return Err(ImageRefusal::Animated);
                }
                set((1 + webp_le24(&data[4..7]), 1 + webp_le24(&data[7..10])))?;
            }
            b"VP8 " => {
                if size < 10 || data[3..6] != [0x9D, 0x01, 0x2A] {
                    return Err(ImageRefusal::Malformed(
                        "webp lossy frame header is invalid",
                    ));
                }
                set((
                    u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3FFF),
                    u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3FFF),
                ))?;
            }
            b"VP8L" => {
                if size < 5 || data[0] != 0x2F {
                    return Err(ImageRefusal::Malformed(
                        "webp lossless frame header is invalid",
                    ));
                }
                let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                set(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))?;
            }
            _ => {}
        }
        offset = data_end + (size & 1);
    }
    dimensions.ok_or(ImageRefusal::Malformed("webp has no image chunk"))
}

/// One image the record names: its bytes, by content hash.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PictureRendition {
    /// Use of this rendition (for example `hero` or `card`). Free text the
    /// consumer selects by; the mechanism attaches no meaning.
    pub role: String,
    pub sha256: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

/// The signed `picture` record body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PictureRecord {
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    pub source_url: String,
    pub licence: String,
    #[serde(default)]
    pub derived_from: Vec<String>,
    #[serde(default)]
    pub transform: Option<String>,
    pub renditions: Vec<PictureRendition>,
}

/// Whether `value` is a lowercase hex SHA-256.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl PictureRecord {
    /// Check every field the record must carry.
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() {
            bail!("title is empty");
        }
        if self.source_url.is_empty() {
            bail!("source_url is missing");
        }
        if !self.source_url.starts_with("https://")
            || self.source_url.len() > 2048
            || self
                .source_url
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            bail!("source_url must be an https URL");
        }
        if !ACCEPTED_LICENCES.contains(&self.licence.as_str()) {
            bail!(
                "licence `{}` is not accepted (accepted: {})",
                self.licence,
                ACCEPTED_LICENCES.join(", ")
            );
        }
        if self
            .derived_from
            .iter()
            .any(|entry| entry.trim().is_empty())
        {
            bail!("derived_from names an empty entry");
        }
        if self.renditions.is_empty() || self.renditions.len() > MAX_RENDITIONS_PER_RECORD {
            bail!("a picture names between 1 and {MAX_RENDITIONS_PER_RECORD} renditions");
        }
        let mut roles = BTreeSet::new();
        for rendition in &self.renditions {
            if rendition.role.trim().is_empty() || !roles.insert(rendition.role.as_str()) {
                bail!("rendition roles must be non-empty and unique");
            }
            if !is_sha256_hex(&rendition.sha256) {
                bail!("rendition `{}` sha256 is not lowercase hex", rendition.role);
            }
            if PictureMediaType::from_mime(&rendition.media_type).is_none() {
                bail!(
                    "rendition `{}` media_type `{}` is not image/png, image/jpeg or image/webp",
                    rendition.role,
                    rendition.media_type
                );
            }
            if rendition.bytes == 0 || rendition.bytes > MAX_PICTURE_BYTES {
                bail!(
                    "rendition `{}` byte size is outside the cap",
                    rendition.role
                );
            }
            if rendition.width == 0
                || rendition.height == 0
                || rendition.width > MAX_PICTURE_DIMENSION
                || rendition.height > MAX_PICTURE_DIMENSION
                || u64::from(rendition.width) * u64::from(rendition.height) > MAX_PICTURE_PIXELS
            {
                bail!(
                    "rendition `{}` dimensions are outside the cap",
                    rendition.role
                );
            }
        }
        Ok(())
    }
}

/// One verified rendition of an installed or source bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PictureEntry {
    pub sha256: String,
    pub media_type: PictureMediaType,
    pub role: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// Where the blob lives. Built from the validated hash and the declared
    /// type only; never from any name a caller supplied.
    pub blob_path: PathBuf,
    /// Path of the signed record naming it, relative to `.ai`.
    pub record: String,
}

/// Every rendition of one bundle, by content hash.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundlePictures {
    pub entries: BTreeMap<String, PictureEntry>,
}

/// Path of the signed picture-manifest ref under `.ai`.
pub fn picture_manifest_ref_path(ai_dir: &Path) -> PathBuf {
    ai_dir.join("refs").join("bundles").join("pictures")
}

/// Path of one blob, from its validated hash and declared type.
pub fn picture_blob_path(ai_dir: &Path, sha256: &str, media_type: PictureMediaType) -> PathBuf {
    ai_dir
        .join(PICTURES_DIR)
        .join(PICTURE_BLOBS_DIR)
        .join(format!("{sha256}.{}", media_type.extension()))
}

fn yaml_envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    }
}

fn walk_records(dir: &Path, relative: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read {}", dir.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("picture path in {} is not UTF-8", dir.display()))?;
        let child_relative = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        let path = entry.path();
        let file_type = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect {}", path.display()))?
            .file_type();
        if file_type.is_symlink() {
            bail!("pictures tree contains a symlink at {}", path.display());
        }
        if file_type.is_dir() {
            // The blobs directory is read separately; it holds no records.
            if relative.is_empty() && name == PICTURE_BLOBS_DIR {
                continue;
            }
            walk_records(&path, &child_relative, out)?;
        } else if file_type.is_file() {
            let is_record = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension == "yaml" || extension == "yml");
            if !is_record {
                bail!(
                    "{} is neither a picture record nor under {PICTURE_BLOBS_DIR}/",
                    path.display()
                );
            }
            out.push((child_relative, path));
        } else {
            bail!(
                "pictures tree contains a special file at {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn load_records(
    ai_dir: &Path,
    trust_store: Option<&TrustStore>,
) -> Result<Vec<(String, PictureRecord)>> {
    let pictures_dir = ai_dir.join(PICTURES_DIR);
    let mut files = Vec::new();
    walk_records(&pictures_dir, "", &mut files)?;
    let envelope = yaml_envelope();
    let mut records = Vec::with_capacity(files.len());
    for (relative, path) in files {
        let raw = lillux::read_regular_file_bounded_no_follow(&path, MAX_RECORD_BYTES)
            .with_context(|| format!("read picture record {relative}"))?;
        let raw = String::from_utf8(raw)
            .map_err(|_| anyhow!("picture record {relative} is not UTF-8"))?;
        if let Some(trust_store) = trust_store {
            let header = parse_signature_header(&raw, &envelope)
                .ok_or_else(|| anyhow!("picture record {relative} is unsigned"))?;
            let (class, _) = ryeos_engine::trust::verify_item_signature(
                &raw,
                &header,
                &envelope,
                trust_store,
                &SignerContext::bundle("picture"),
            )
            .map_err(|error| {
                anyhow!("picture record {relative} signature verification failed: {error}")
            })?;
            if class != TrustClass::Trusted {
                bail!(
                    "picture record {relative} signer {} is not trusted",
                    header.signer_fingerprint
                );
            }
        }
        let body = lillux::signature::strip_signature_lines(&raw);
        let record: PictureRecord = serde_yaml::from_str(&body)
            .with_context(|| format!("parse picture record {relative}"))?;
        record
            .validate()
            .with_context(|| format!("picture record {relative}"))?;
        records.push((relative, record));
    }
    Ok(records)
}

/// Blob files on disk by hash. Every name must be `<sha256>.<extension>`.
fn list_blobs(ai_dir: &Path) -> Result<BTreeMap<String, (PathBuf, PictureMediaType)>> {
    let blobs_dir = ai_dir.join(PICTURES_DIR).join(PICTURE_BLOBS_DIR);
    let mut blobs = BTreeMap::new();
    let read = match fs::read_dir(&blobs_dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(blobs),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", blobs_dir.display()));
        }
    };
    for entry in read {
        let entry = entry.with_context(|| format!("read {}", blobs_dir.display()))?;
        let path = entry.path();
        let file_type = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect {}", path.display()))?
            .file_type();
        if !file_type.is_file() {
            bail!("picture blob {} must be a regular file", path.display());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("picture blob name in {} is not UTF-8", blobs_dir.display()))?;
        let (stem, extension) = name
            .rsplit_once('.')
            .ok_or_else(|| anyhow!("picture blob `{name}` is not named <sha256>.<extension>"))?;
        let media_type = PictureMediaType::from_extension(extension)
            .filter(|_| is_sha256_hex(stem))
            .ok_or_else(|| anyhow!("picture blob `{name}` is not named <sha256>.<extension>"))?;
        if blobs.insert(stem.to_owned(), (path, media_type)).is_some() {
            bail!("picture blob {stem} is stored twice");
        }
    }
    Ok(blobs)
}

/// Cross-check records against blobs. `deep` hashes and inspects every blob;
/// otherwise only the presence and byte length are checked.
fn collect_entries(
    ai_dir: &Path,
    records: Vec<(String, PictureRecord)>,
    deep: bool,
) -> Result<BundlePictures> {
    let mut blobs = list_blobs(ai_dir)?;
    let mut entries = BTreeMap::new();
    for (relative, record) in records {
        for rendition in record.renditions {
            let media_type = PictureMediaType::from_mime(&rendition.media_type)
                .ok_or_else(|| anyhow!("picture record {relative}: unknown media type"))?;
            let (path, stored_type) = blobs.remove(&rendition.sha256).ok_or_else(|| {
                anyhow!(
                    "picture record {relative} rendition `{}` names {} but no blob has that hash \
                     (or two renditions share it)",
                    rendition.role,
                    rendition.sha256
                )
            })?;
            if stored_type != media_type {
                bail!(
                    "picture record {relative} rendition `{}` declares {} but its blob is stored \
                     as {}",
                    rendition.role,
                    media_type.mime(),
                    stored_type.mime()
                );
            }
            if deep {
                let bytes = lillux::read_regular_file_bounded_no_follow(&path, MAX_PICTURE_BYTES)
                    .with_context(|| format!("read picture blob {}", rendition.sha256))?;
                let actual = lillux::sha256_hex(&bytes);
                if actual != rendition.sha256 {
                    bail!(
                        "picture record {relative} rendition `{}`: blob hashes to {actual}, \
                         the record says {}",
                        rendition.role,
                        rendition.sha256
                    );
                }
                if bytes.len() as u64 != rendition.bytes {
                    bail!(
                        "picture record {relative} rendition `{}`: blob is {} bytes, the record \
                         says {}",
                        rendition.role,
                        bytes.len(),
                        rendition.bytes
                    );
                }
                let facts = inspect_image(&bytes, media_type).map_err(|refusal| {
                    anyhow!(
                        "picture record {relative} rendition `{}`: {refusal}",
                        rendition.role
                    )
                })?;
                if facts.width != rendition.width || facts.height != rendition.height {
                    bail!(
                        "picture record {relative} rendition `{}`: blob is {}x{}, the record says \
                         {}x{}",
                        rendition.role,
                        facts.width,
                        facts.height,
                        rendition.width,
                        rendition.height
                    );
                }
            } else {
                let metadata = fs::symlink_metadata(&path)
                    .with_context(|| format!("inspect picture blob {}", rendition.sha256))?;
                if !metadata.file_type().is_file() || metadata.len() != rendition.bytes {
                    bail!(
                        "picture record {relative} rendition `{}`: blob is not a regular file of \
                         {} bytes",
                        rendition.role,
                        rendition.bytes
                    );
                }
            }
            entries.insert(
                rendition.sha256.clone(),
                PictureEntry {
                    sha256: rendition.sha256,
                    media_type,
                    role: rendition.role,
                    width: rendition.width,
                    height: rendition.height,
                    bytes: rendition.bytes,
                    blob_path: path,
                    record: format!("{PICTURES_DIR}/{relative}"),
                },
            );
        }
    }
    if let Some(stray) = blobs.keys().next() {
        bail!("picture blob {stray} is named by no record");
    }
    Ok(BundlePictures { entries })
}

/// Publisher-side check of a bundle source tree: every record carries its
/// required fields, every blob has exactly one record and hashes to it, magic
/// bytes match the declared type, and the size caps hold. Records may be
/// unsigned: the publisher signs them afterwards. `None` when the bundle has
/// no `.ai/pictures` root.
pub fn check_picture_sources(bundle_root: &Path) -> Result<Option<BundlePictures>> {
    let ai_dir = bundle_root.join(ryeos_engine::AI_DIR);
    let pictures_dir = ai_dir.join(PICTURES_DIR);
    match fs::symlink_metadata(&pictures_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => bail!("{} must be a real directory", pictures_dir.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", pictures_dir.display()));
        }
    }
    let records = load_records(&ai_dir, None)?;
    collect_entries(&ai_dir, records, true).map(Some)
}

/// Verify an installed bundle's pictures through its signed picture manifest:
/// the signed ref, the manifest object, one item source per blob, and every
/// signed record. `deep` additionally hashes and inspects every blob (install
/// time); the serve path re-checks the one blob it reads. A bundle with no
/// pictures verifies as empty.
pub fn verify_bundle_pictures(
    bundle_root: &Path,
    trust_store: &TrustStore,
    deep: bool,
) -> Result<BundlePictures> {
    let ai_dir = bundle_root.join(ryeos_engine::AI_DIR);
    let pictures_dir = ai_dir.join(PICTURES_DIR);
    let ref_path = picture_manifest_ref_path(&ai_dir);
    let has_pictures = match fs::symlink_metadata(&pictures_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => true,
        Ok(_) => bail!("{} must be a real directory", pictures_dir.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", pictures_dir.display()));
        }
    };
    let has_ref = match fs::symlink_metadata(&ref_path) {
        Ok(metadata) if metadata.file_type().is_file() => true,
        Ok(_) => bail!("{} must be a regular file", ref_path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).with_context(|| format!("inspect {}", ref_path.display())),
    };
    match (has_pictures, has_ref) {
        (false, false) => return Ok(BundlePictures::default()),
        (true, false) => bail!("bundle ships pictures but no signed picture manifest ref"),
        (false, true) => bail!("bundle has a picture manifest ref but no pictures"),
        (true, true) => {}
    }

    let signed_ref =
        lillux::read_regular_file_bounded_no_follow(&ref_path, MAX_EXECUTOR_MANIFEST_REF_BYTES)?;
    let signed_ref =
        String::from_utf8(signed_ref).map_err(|_| anyhow!("picture manifest ref is not UTF-8"))?;
    let verified = verify_signed_manifest_ref_in_domain(
        &signed_ref,
        PICTURE_MANIFEST_REF_DOMAIN,
        |fingerprint| {
            trust_store
                .get(fingerprint)
                .map(|signer| signer.verifying_key)
        },
        ryeos_engine::resolution::TrustClass::TrustedBundle,
    )
    .map_err(|error| anyhow!("picture manifest ref: {error}"))?;
    if verified.trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle {
        bail!("picture manifest signer is not a trusted bundle signer");
    }

    let cas = lillux::cas::CasStore::new(ai_dir.join("objects"));
    let manifest_value = cas
        .get_object_bounded(&verified.manifest_hash, MAX_MANIFEST_OBJECT_BYTES)?
        .ok_or_else(|| anyhow!("picture manifest object is missing from the bundle CAS"))?;
    let item_sources = verify_executor_manifest_object(&manifest_value, &verified.manifest_hash)
        .map_err(|error| anyhow!("picture manifest: {error}"))?;
    let mut manifest_hashes = BTreeSet::new();
    for (item_ref, item_source_hash) in &item_sources {
        let sha256 = item_ref
            .strip_prefix(PICTURE_ITEM_REF_PREFIX)
            .filter(|candidate| is_sha256_hex(candidate))
            .ok_or_else(|| anyhow!("picture manifest names `{item_ref}`, not a picture"))?;
        let item_source = cas
            .get_object_bounded(item_source_hash, MAX_ITEM_SOURCE_OBJECT_BYTES)?
            .ok_or_else(|| anyhow!("picture item source for {sha256} is missing from CAS"))?;
        let blob_hash = verify_picture_item_source(&item_source, item_source_hash, item_ref)
            .map_err(|error| anyhow!("picture manifest: {error}"))?;
        if blob_hash != sha256 {
            bail!("picture manifest item {sha256} commits to a different blob hash");
        }
        manifest_hashes.insert(sha256.to_owned());
    }

    let records = load_records(&ai_dir, Some(trust_store))?;
    let pictures = collect_entries(&ai_dir, records, deep)?;
    let record_hashes: BTreeSet<String> = pictures.entries.keys().cloned().collect();
    if record_hashes != manifest_hashes {
        bail!("signed picture manifest and signed picture records name different blobs");
    }
    Ok(pictures)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 16x10 lossy WebP.
    pub(crate) const WEBP_16X10: [u8; 52] = [
        0x52, 0x49, 0x46, 0x46, 0x2c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x20, 0x20, 0x00, 0x00, 0x00, 0x70, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x10, 0x00, 0x0a, 0x00,
        0x01, 0x40, 0x26, 0x25, 0x94, 0x02, 0x74, 0x01, 0xa0, 0x00, 0x00, 0xf1, 0x3a, 0xbf, 0xbe,
        0xa7, 0x41, 0xf1, 0x5f, 0x00, 0x00, 0x00,
    ];

    /// A real 16x10 PNG.
    pub(crate) const PNG_16X10: [u8; 80] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x0a, 0x08, 0x02, 0x00, 0x00, 0x00, 0x32,
        0xdc, 0x49, 0xcb, 0x00, 0x00, 0x00, 0x17, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xac,
        0x68, 0xea, 0x61, 0x20, 0x05, 0x30, 0x91, 0xa4, 0x7a, 0x54, 0xc3, 0xa0, 0xd1, 0x00, 0x00,
        0xfc, 0x88, 0x01, 0x9a, 0x3b, 0x26, 0x8c, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
        0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// A JPEG header with one frame (16x10) and the end marker.
    pub(crate) const JPEG_16X10: [u8; 31] = [
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x0a,
        0x00, 0x10, 0x01, 0x01, 0x11, 0x00, 0xff, 0xda, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0xff,
        0xd9,
    ];

    #[test]
    fn real_images_report_their_dimensions() {
        assert_eq!(
            inspect_image(&WEBP_16X10, PictureMediaType::Webp),
            Ok(ImageFacts {
                width: 16,
                height: 10
            })
        );
        assert_eq!(
            inspect_image(&PNG_16X10, PictureMediaType::Png),
            Ok(ImageFacts {
                width: 16,
                height: 10
            })
        );
        assert_eq!(
            inspect_image(&JPEG_16X10, PictureMediaType::Jpeg),
            Ok(ImageFacts {
                width: 16,
                height: 10
            })
        );
    }

    #[test]
    fn bytes_of_another_format_than_declared_are_refused() {
        for (bytes, declared) in [
            (&WEBP_16X10[..], PictureMediaType::Png),
            (&PNG_16X10[..], PictureMediaType::Webp),
            (&PNG_16X10[..], PictureMediaType::Jpeg),
            (&JPEG_16X10[..], PictureMediaType::Png),
        ] {
            assert!(
                matches!(
                    inspect_image(bytes, declared),
                    Err(ImageRefusal::MagicMismatch { .. })
                ),
                "declared {}",
                declared.mime()
            );
        }
    }

    #[test]
    fn svg_and_gif_bytes_are_refused_under_every_declared_type() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"/>";
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
        for declared in [
            PictureMediaType::Png,
            PictureMediaType::Jpeg,
            PictureMediaType::Webp,
        ] {
            for bytes in [&svg[..], &gif[..]] {
                assert!(matches!(
                    inspect_image(bytes, declared),
                    Err(ImageRefusal::MagicMismatch { .. })
                ));
            }
        }
        assert_eq!(PictureMediaType::from_mime("image/svg+xml"), None);
        assert_eq!(PictureMediaType::from_mime("image/gif"), None);
    }

    #[test]
    fn animated_webp_and_png_are_refused() {
        // Extended header with the animation flag set.
        let mut animated = Vec::new();
        animated.extend_from_slice(b"RIFF");
        animated.extend_from_slice(&22u32.to_le_bytes());
        animated.extend_from_slice(b"WEBPVP8X");
        animated.extend_from_slice(&10u32.to_le_bytes());
        animated.extend_from_slice(&[0x02, 0, 0, 0, 15, 0, 0, 9, 0, 0]);
        assert_eq!(
            inspect_image(&animated, PictureMediaType::Webp),
            Err(ImageRefusal::Animated)
        );

        // Animation chunk without the flag.
        let mut chunked = Vec::new();
        chunked.extend_from_slice(b"RIFF");
        chunked.extend_from_slice(&18u32.to_le_bytes());
        chunked.extend_from_slice(b"WEBPANIM");
        chunked.extend_from_slice(&6u32.to_le_bytes());
        chunked.extend_from_slice(&[0; 6]);
        assert_eq!(
            inspect_image(&chunked, PictureMediaType::Webp),
            Err(ImageRefusal::Animated)
        );

        // An animation-control chunk before the image data.
        let mut apng = Vec::new();
        apng.extend_from_slice(&PNG_16X10[..33]);
        apng.extend_from_slice(&8u32.to_be_bytes());
        apng.extend_from_slice(b"acTL");
        apng.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 0]);
        apng.extend_from_slice(&[0, 0, 0, 0]);
        apng.extend_from_slice(&PNG_16X10[33..]);
        assert_eq!(
            inspect_image(&apng, PictureMediaType::Png),
            Err(ImageRefusal::Animated)
        );
    }

    #[test]
    fn oversize_images_and_dimensions_are_refused() {
        let big = vec![0u8; usize::try_from(MAX_PICTURE_BYTES).unwrap() + 1];
        assert!(matches!(
            inspect_image(&big, PictureMediaType::Png),
            Err(ImageRefusal::TooLarge { .. })
        ));
        assert_eq!(
            inspect_image(&[], PictureMediaType::Png),
            Err(ImageRefusal::Empty)
        );
        // Header of a 5000x10 PNG.
        let mut wide = PNG_16X10.to_vec();
        wide[16..20].copy_from_slice(&5000u32.to_be_bytes());
        assert!(matches!(
            inspect_image(&wide, PictureMediaType::Png),
            Err(ImageRefusal::DimensionsOutOfBounds { .. })
        ));
    }

    #[test]
    fn truncated_and_trailing_bytes_are_refused() {
        assert!(inspect_image(&PNG_16X10[..40], PictureMediaType::Png).is_err());
        let mut trailing = PNG_16X10.to_vec();
        trailing.push(0);
        assert!(inspect_image(&trailing, PictureMediaType::Png).is_err());
        let mut webp = WEBP_16X10.to_vec();
        webp.push(0);
        assert!(inspect_image(&webp, PictureMediaType::Webp).is_err());
        assert!(inspect_image(&JPEG_16X10[..20], PictureMediaType::Jpeg).is_err());
    }

    fn record(licence: &str, source_url: &str) -> PictureRecord {
        PictureRecord {
            title: "A picture".into(),
            artist: None,
            date: None,
            source_url: source_url.into(),
            licence: licence.into(),
            derived_from: Vec::new(),
            transform: None,
            renditions: vec![PictureRendition {
                role: "card".into(),
                sha256: "a".repeat(64),
                media_type: "image/webp".into(),
                width: 16,
                height: 10,
                bytes: 52,
            }],
        }
    }

    #[test]
    fn record_requires_source_url_and_an_accepted_licence() {
        record("cc0", "https://example.test/object/1")
            .validate()
            .unwrap();
        record("public-domain", "https://example.test/object/1")
            .validate()
            .unwrap();
        for bad in ["", "http://example.test/x", "ftp://example.test/x"] {
            assert!(record("cc0", bad).validate().is_err(), "{bad}");
        }
        for bad in ["", "cc-by", "all-rights-reserved", "CC0"] {
            assert!(
                record(bad, "https://example.test/x").validate().is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn record_refuses_unknown_fields_and_media_types() {
        let mut svg = record("cc0", "https://example.test/x");
        svg.renditions[0].media_type = "image/svg+xml".into();
        assert!(svg.validate().is_err());
        let parsed: Result<PictureRecord, _> = serde_yaml::from_str(
            "title: t\nsource_url: https://example.test/x\nlicence: cc0\nrenditions: []\nextra: 1\n",
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn licence_list_matches_the_kind_schema_enum() {
        let schema = include_str!(
            "../../../../bundles/ryeos-ui/.ai/node/engine/kinds/picture/picture.kind-schema.yaml"
        );
        let body = lillux::signature::strip_signature_lines(schema);
        let schema: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
        let listed = &schema["composed_value_contract"]["optional"]["licence"]["enum"];
        let required = &schema["composed_value_contract"]["required"]["licence"]["enum"];
        let listed = if listed.is_null() { required } else { listed };
        let names: Vec<&str> = listed
            .as_sequence()
            .expect("licence enum")
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(names, ACCEPTED_LICENCES);
    }

    fn write_blob(root: &Path, bytes: &[u8], extension: &str) -> String {
        let sha = lillux::sha256_hex(bytes);
        let dir = root.join(".ai/pictures/blobs");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{sha}.{extension}")), bytes).unwrap();
        sha
    }

    fn write_record(root: &Path, name: &str, yaml: &str) {
        let dir = root.join(".ai/pictures/ryeos");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.yaml")), yaml).unwrap();
    }

    fn record_yaml(sha: &str, extra: &str) -> String {
        format!(
            "title: Test\nsource_url: https://example.test/object/1\nlicence: cc0\n{extra}\
             renditions:\n  - role: card\n    sha256: \"{sha}\"\n    media_type: image/webp\n    \
             width: 16\n    height: 10\n    bytes: 52\n"
        )
    }

    #[test]
    fn publisher_check_accepts_a_matching_record_and_blob() {
        let root = tempfile::tempdir().unwrap();
        let sha = write_blob(root.path(), &WEBP_16X10, "webp");
        write_record(root.path(), "one", &record_yaml(&sha, ""));
        let pictures = check_picture_sources(root.path()).unwrap().unwrap();
        assert_eq!(pictures.entries.len(), 1);
        assert_eq!(pictures.entries[&sha].media_type, PictureMediaType::Webp);
        assert!(
            check_picture_sources(tempfile::tempdir().unwrap().path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn publisher_check_refuses_a_missing_source_url_a_bad_licence_and_a_hash_mismatch() {
        let root = tempfile::tempdir().unwrap();
        let sha = write_blob(root.path(), &WEBP_16X10, "webp");

        let no_source =
            record_yaml(&sha, "").replace("source_url: https://example.test/object/1\n", "");
        write_record(root.path(), "one", &no_source);
        assert!(check_picture_sources(root.path()).is_err());

        let bad_licence = record_yaml(&sha, "").replace("licence: cc0", "licence: cc-by");
        write_record(root.path(), "one", &bad_licence);
        let error = format!("{:#}", check_picture_sources(root.path()).unwrap_err());
        assert!(error.contains("licence"), "{error}");

        // A record naming a hash no blob has.
        write_record(root.path(), "one", &record_yaml(&"b".repeat(64), ""));
        assert!(check_picture_sources(root.path()).is_err());

        // A blob whose bytes no longer hash to its name.
        write_record(root.path(), "one", &record_yaml(&sha, ""));
        let path = root.path().join(format!(".ai/pictures/blobs/{sha}.webp"));
        let mut tampered = WEBP_16X10.to_vec();
        tampered[30] ^= 1;
        fs::write(path, tampered).unwrap();
        let error = format!("{:#}", check_picture_sources(root.path()).unwrap_err());
        assert!(error.contains("hashes to"), "{error}");
    }

    #[test]
    fn publisher_check_refuses_stray_blobs_wrong_type_and_extra_files() {
        let root = tempfile::tempdir().unwrap();
        let sha = write_blob(root.path(), &WEBP_16X10, "webp");
        write_record(root.path(), "one", &record_yaml(&sha, ""));
        let stray = write_blob(root.path(), &PNG_16X10, "png");
        let error = format!("{:#}", check_picture_sources(root.path()).unwrap_err());
        assert!(
            error.contains(&stray) && error.contains("no record"),
            "{error}"
        );
        fs::remove_file(root.path().join(format!(".ai/pictures/blobs/{stray}.png"))).unwrap();

        // A record that declares PNG for WebP bytes.
        let wrong = record_yaml(&sha, "").replace("image/webp", "image/png");
        write_record(root.path(), "one", &wrong);
        assert!(check_picture_sources(root.path()).is_err());

        write_record(root.path(), "one", &record_yaml(&sha, ""));
        fs::write(root.path().join(".ai/pictures/notes.txt"), "x").unwrap();
        assert!(check_picture_sources(root.path()).is_err());
    }
}
