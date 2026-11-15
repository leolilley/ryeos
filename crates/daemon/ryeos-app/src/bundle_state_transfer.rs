//! Moving a bundle's state between nodes while both are stopped: signed export,
//! verified import, and removal.
//!
//! An archive is a tar holding a node-signed `MANIFEST.json` (bundle id,
//! source node, creation time, every namespace's file list with the sha256 and
//! size of each file) and the files themselves under `files/<namespace>/`.
//! Import refuses anything the manifest does not describe exactly, so a
//! tampered or truncated archive cannot install. The node never interprets the
//! bytes a bundle stores; it carries regular files and refuses links and
//! special entries rather than guess what they mean.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use lillux::crypto::{Signature, Signer as _, Verifier as _, VerifyingKey};
use ryeos_bundle::runtime_authority::validate_state_home_segment;
use ryeos_node_runtime::identity::NodeIdentity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bundle_state_home::{HOME_RECORD_NAME, HomeRecord, root_path};

/// Format of the archive manifest.
pub const ARCHIVE_SCHEMA: u32 = 1;
const MANIFEST_ENTRY: &str = "MANIFEST.json";
const FILES_PREFIX: &str = "files";
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
/// Largest single file a state archive carries.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Most files and deepest nesting one namespace may hold for transfer.
const TREE_BUDGET: lillux::DirectoryTraversalBudget =
    lillux::DirectoryTraversalBudget::new(100_000, 32);
/// Largest archive an import reads.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

/// One file of one namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveFile {
    /// Path relative to the namespace, `/`-separated.
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// One namespace and the files it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveNamespace {
    pub name: String,
    pub files: Vec<ArchiveFile>,
}

/// The node-signed description of an archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveManifest {
    pub schema: u32,
    pub bundle_id: String,
    /// `fp:<fingerprint>` of the node that exported and signed.
    pub source_node: String,
    pub created_at: String,
    pub namespaces: Vec<ArchiveNamespace>,
    /// Base64 ed25519 signature over the canonical JSON of every other field.
    pub signature: String,
}

impl ArchiveManifest {
    fn unsigned(&self) -> Value {
        json!({
            "schema": self.schema,
            "bundle_id": self.bundle_id,
            "source_node": self.source_node,
            "created_at": self.created_at,
            "namespaces": self.namespaces,
        })
    }

    fn payload(&self) -> Result<Vec<u8>> {
        Ok(lillux::canonical_json(&self.unsigned())?.into_bytes())
    }

    /// Verify the signature with `key`, the verifying key of `source_node`.
    pub fn verify(&self, key: &VerifyingKey) -> Result<()> {
        ensure!(
            self.schema == ARCHIVE_SCHEMA,
            "archive schema {} is not the supported schema {ARCHIVE_SCHEMA}",
            self.schema
        );
        let expected = format!("fp:{}", lillux::sha256_hex(key.as_bytes()));
        ensure!(
            self.source_node == expected,
            "archive was signed by `{}`, not by the key offered (`{expected}`)",
            self.source_node
        );
        let signature = Signature::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(&self.signature)
                .context("archive signature is not base64")?,
        )
        .context("archive signature is malformed")?;
        key.verify(&self.payload()?, &signature)
            .context("archive manifest signature does not verify")
    }
}

/// The relative `/`-separated path of a file, validated to contain only
/// ordinary path components.
fn validated_relative_path(path: &str) -> Result<PathBuf> {
    ensure!(!path.is_empty() && !path.contains('\0'), "empty file path");
    let candidate = Path::new(path);
    ensure!(
        candidate
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "file path `{path}` is not a plain relative path"
    );
    ensure!(
        !path.contains('\\')
            && path
                .split('/')
                .all(|segment| !segment.is_empty() && segment != "." && segment != ".."),
        "file path `{path}` is not canonical"
    );
    Ok(candidate.to_path_buf())
}

/// Read every regular file below `directory`, refusing anything else.
fn read_tree(
    directory: &lillux::PinnedDirectory,
    prefix: &Path,
    depth: usize,
    remaining: &mut usize,
    out: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    ensure!(depth <= TREE_BUDGET.max_depth, "namespace nests too deeply");
    let mut entries = directory.entries_no_follow()?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        *remaining = remaining
            .checked_sub(1)
            .context("namespace holds more entries than a transfer carries")?;
        let relative = prefix.join(&entry.name);
        match entry.entry_type {
            lillux::PinnedEntryType::Regular => {
                let file = directory
                    .open_regular(&entry.name, false)?
                    .with_context(|| {
                        format!("{} vanished while it was read", relative.display())
                    })?;
                let bytes = lillux::read_open_regular_file_bounded(file, MAX_FILE_BYTES)
                    .with_context(|| format!("read {}", relative.display()))?;
                let key = relative
                    .to_str()
                    .with_context(|| format!("{} is not UTF-8", relative.display()))?
                    .to_owned();
                out.insert(key, bytes);
            }
            lillux::PinnedEntryType::Directory => {
                let child = directory
                    .open_child_directory(&entry.name)?
                    .with_context(|| {
                        format!("{} vanished while it was read", relative.display())
                    })?;
                read_tree(&child, &relative, depth + 1, remaining, out)?;
            }
            other => bail!(
                "{} is a {other:?}; a state archive carries only regular files and directories",
                relative.display()
            ),
        }
    }
    Ok(())
}

fn open_bundle_home(
    app_root: &Path,
    bundle_id: &str,
    node: &NodeIdentity,
) -> Result<lillux::PinnedDirectory> {
    validate_state_home_segment_or_bundle(bundle_id)?;
    let root = lillux::PinnedDirectory::open(&root_path(app_root))?
        .context("the node has no bundle state")?;
    let home = root
        .open_child_directory(OsStr::new(bundle_id))?
        .with_context(|| format!("bundle `{bundle_id}` has no state home"))?;
    let bytes =
        lillux::read_regular_file_bounded_no_follow(&home.path().join(HOME_RECORD_NAME), 16 * 1024)
            .context("read the home record")?;
    let record: HomeRecord = serde_json::from_slice(&bytes).context("parse the home record")?;
    record
        .verify(bundle_id, node.verifying_key())
        .with_context(|| format!("the home of `{bundle_id}` does not verify"))?;
    Ok(home)
}

fn validate_state_home_segment_or_bundle(bundle_id: &str) -> Result<()> {
    ryeos_state::objects::validate_bundle_identifier("bundle_id", bundle_id)
        .map_err(|error| anyhow::anyhow!("{error:#}"))
}

fn namespaces_to_move(
    home: &lillux::PinnedDirectory,
    namespace: Option<&str>,
) -> Result<Vec<String>> {
    if let Some(namespace) = namespace {
        validate_state_home_segment("namespace", namespace).map_err(anyhow::Error::msg)?;
        ensure!(
            home.open_child_directory(OsStr::new(namespace))?.is_some(),
            "the home has no namespace `{namespace}`"
        );
        return Ok(vec![namespace.to_owned()]);
    }
    let mut names = Vec::new();
    for name in home.entry_names()? {
        if name == OsStr::new(HOME_RECORD_NAME) {
            continue;
        }
        let name = name.to_string_lossy().into_owned();
        validate_state_home_segment("namespace", &name).map_err(anyhow::Error::msg)?;
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// What an export wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSummary {
    pub bundle_id: String,
    pub namespaces: Vec<String>,
    pub files: usize,
    pub bytes: u64,
    pub archive_sha256: String,
}

/// Build the signed archive of `bundle_id`'s state (one namespace, or all).
/// The home must carry a record that verifies under this node's key.
pub fn export_archive(
    app_root: &Path,
    node: &NodeIdentity,
    bundle_id: &str,
    namespace: Option<&str>,
) -> Result<(Vec<u8>, ExportSummary)> {
    let home = open_bundle_home(app_root, bundle_id, node)?;
    let names = namespaces_to_move(&home, namespace)?;
    let mut manifest_namespaces = Vec::new();
    let mut contents: Vec<(String, BTreeMap<String, Vec<u8>>)> = Vec::new();
    for name in &names {
        let directory = home
            .open_child_directory(OsStr::new(name))?
            .with_context(|| format!("namespace `{name}` vanished"))?;
        let mut files = BTreeMap::new();
        let mut remaining = TREE_BUDGET.max_entries;
        read_tree(&directory, Path::new(""), 0, &mut remaining, &mut files)?;
        manifest_namespaces.push(ArchiveNamespace {
            name: name.clone(),
            files: files
                .iter()
                .map(|(path, bytes)| ArchiveFile {
                    path: path.clone(),
                    sha256: lillux::sha256_hex(bytes),
                    size: bytes.len() as u64,
                })
                .collect(),
        });
        contents.push((name.clone(), files));
    }
    let mut manifest = ArchiveManifest {
        schema: ARCHIVE_SCHEMA,
        bundle_id: bundle_id.to_owned(),
        source_node: node.principal_id(),
        created_at: lillux::time::iso8601_now(),
        namespaces: manifest_namespaces,
        signature: String::new(),
    };
    let signature: Signature = node.signing_key().sign(&manifest.payload()?);
    manifest.signature = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());

    let mut builder = tar::Builder::new(Vec::new());
    append_entry(
        &mut builder,
        MANIFEST_ENTRY,
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    let mut file_count = 0usize;
    let mut byte_count = 0u64;
    for (name, files) in &contents {
        for (path, bytes) in files {
            append_entry(
                &mut builder,
                &format!("{FILES_PREFIX}/{name}/{path}"),
                bytes,
            )?;
            file_count += 1;
            byte_count += bytes.len() as u64;
        }
    }
    let archive = builder.into_inner()?;
    let summary = ExportSummary {
        bundle_id: bundle_id.to_owned(),
        namespaces: names,
        files: file_count,
        bytes: byte_count,
        archive_sha256: lillux::sha256_hex(&archive),
    };
    Ok((archive, summary))
}

fn append_entry(builder: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Regular);
    builder
        .append_data(&mut header, path, bytes)
        .with_context(|| format!("append {path} to the archive"))
}

/// An archive's manifest and the bytes of every file it lists.
#[derive(Debug)]
pub struct ParsedArchive {
    pub manifest: ArchiveManifest,
    /// `(namespace, relative path)` to bytes.
    pub files: BTreeMap<(String, String), Vec<u8>>,
}

/// Parse an archive and prove it matches its manifest exactly: every listed
/// file present with the listed size and sha256, and nothing else. The
/// manifest's signature is *not* judged here; the caller decides which keys it
/// trusts (see [`ArchiveManifest::verify`]).
pub fn parse_archive(bytes: &[u8]) -> Result<ParsedArchive> {
    ensure!(
        bytes.len() as u64 <= MAX_ARCHIVE_BYTES,
        "archive exceeds {MAX_ARCHIVE_BYTES} bytes"
    );
    let mut archive = tar::Archive::new(bytes);
    let mut manifest: Option<ArchiveManifest> = None;
    let mut found: BTreeMap<(String, String), Vec<u8>> = BTreeMap::new();
    for entry in archive.entries().context("read the archive")? {
        let mut entry = entry.context("read an archive entry")?;
        ensure!(
            entry.header().entry_type() == tar::EntryType::Regular,
            "archive holds an entry that is not a regular file"
        );
        let path = entry
            .path()
            .context("archive entry path")?
            .to_str()
            .context("archive entry path is not UTF-8")?
            .to_owned();
        let size = entry.header().size()?;
        ensure!(
            size <= MAX_FILE_BYTES.max(MAX_MANIFEST_BYTES),
            "archive entry `{path}` is too large"
        );
        let mut body = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut body)?;
        if path == MANIFEST_ENTRY {
            ensure!(manifest.is_none(), "archive holds two manifests");
            ensure!(size <= MAX_MANIFEST_BYTES, "archive manifest is too large");
            manifest = Some(serde_json::from_slice(&body).context("parse the archive manifest")?);
            continue;
        }
        let rest = path
            .strip_prefix(&format!("{FILES_PREFIX}/"))
            .with_context(|| format!("archive entry `{path}` is outside `{FILES_PREFIX}/`"))?;
        let (namespace, relative) = rest
            .split_once('/')
            .with_context(|| format!("archive entry `{path}` names no file"))?;
        validate_state_home_segment("namespace", namespace).map_err(anyhow::Error::msg)?;
        validated_relative_path(relative)?;
        ensure!(
            found
                .insert((namespace.to_owned(), relative.to_owned()), body)
                .is_none(),
            "archive entry `{path}` appears twice"
        );
    }
    let manifest = manifest.context("archive has no manifest")?;
    validate_state_home_segment_or_bundle(&manifest.bundle_id)?;
    let mut listed = BTreeSet::new();
    let mut namespaces_seen = BTreeSet::new();
    for namespace in &manifest.namespaces {
        validate_state_home_segment("namespace", &namespace.name).map_err(anyhow::Error::msg)?;
        ensure!(
            namespaces_seen.insert(namespace.name.as_str()),
            "manifest lists namespace `{}` twice",
            namespace.name
        );
        for file in &namespace.files {
            validated_relative_path(&file.path)?;
            let key = (namespace.name.clone(), file.path.clone());
            let body = found
                .get(&key)
                .with_context(|| format!("archive lacks listed file {}/{}", key.0, key.1))?;
            ensure!(
                body.len() as u64 == file.size && lillux::sha256_hex(body) == file.sha256,
                "file {}/{} does not match its manifest entry",
                key.0,
                key.1
            );
            ensure!(
                listed.insert(key.clone()),
                "manifest lists {}/{} twice",
                key.0,
                key.1
            );
        }
    }
    ensure!(
        found.len() == listed.len(),
        "archive holds files its manifest does not list"
    );
    Ok(ParsedArchive {
        manifest,
        files: found,
    })
}

/// What a caller must supply to judge an import. Kept as data and closures so
/// the stopped-node handler, not this module, owns where trust and bundle
/// facts come from.
pub struct ImportPolicy<'a> {
    /// The verifying key for a `fp:<fingerprint>` signer this node trusts, or
    /// `None`. This node's own key and every pinned peer node key belong here.
    pub signer_key: &'a dyn Fn(&str) -> Option<VerifyingKey>,
    /// Whether `bundle_id` is installed and its signed manifest declares a
    /// state home backing `namespace`; the error names what is missing.
    pub namespace_backed: &'a dyn Fn(&str, &str) -> std::result::Result<(), String>,
    /// Namespaces the operator explicitly allows to be replaced.
    pub replace: &'a [String],
}

/// What an import did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub bundle_id: String,
    pub created: Vec<String>,
    pub replaced: Vec<String>,
    /// Namespaces already holding exactly the archive's bytes.
    pub unchanged: Vec<String>,
    pub files: usize,
}

/// Verify and install an archive into the stopped node at `app_root`.
///
/// Refuses unless the manifest verifies under a trusted key, the bundle is
/// installed and declares a state home backing every namespace, and every
/// target namespace is absent (or already identical, or named in
/// `policy.replace`). Nothing is written before every check passes.
pub fn import_archive(
    app_root: &Path,
    node: &NodeIdentity,
    archive: &[u8],
    policy: &ImportPolicy<'_>,
) -> Result<ImportSummary> {
    let parsed = parse_archive(archive)?;
    let manifest = &parsed.manifest;
    let key = (policy.signer_key)(&manifest.source_node).with_context(|| {
        format!(
            "archive was signed by `{}`, which this node neither is nor has pinned as a peer \
             node; pin its key with `ryeos trust pin` or export the archive from this node",
            manifest.source_node
        )
    })?;
    manifest.verify(&key)?;
    for namespace in &manifest.namespaces {
        (policy.namespace_backed)(&manifest.bundle_id, &namespace.name).map_err(|reason| {
            anyhow::anyhow!(
                "bundle `{}` cannot take namespace `{}`: {reason}",
                manifest.bundle_id,
                namespace.name
            )
        })?;
    }
    for requested in policy.replace {
        ensure!(
            manifest
                .namespaces
                .iter()
                .any(|namespace| &namespace.name == requested),
            "--replace-namespace `{requested}` names no namespace of the archive"
        );
    }

    // Judge every target before writing any.
    let existing_home = lillux::PinnedDirectory::open(&root_path(app_root))?
        .map(|root| root.open_child_directory(OsStr::new(&manifest.bundle_id)))
        .transpose()?
        .flatten();
    if existing_home.is_some() {
        open_bundle_home(app_root, &manifest.bundle_id, node)?;
    }
    let mut plan: Vec<(&ArchiveNamespace, Disposition)> = Vec::new();
    for namespace in &manifest.namespaces {
        let current = existing_home
            .as_ref()
            .map(|home| home.open_child_directory(OsStr::new(&namespace.name)))
            .transpose()?
            .flatten();
        let disposition = match current {
            None => Disposition::Create,
            Some(directory) => {
                let mut tree = BTreeMap::new();
                let mut remaining = TREE_BUDGET.max_entries;
                read_tree(&directory, Path::new(""), 0, &mut remaining, &mut tree)?;
                let identical = tree.len()
                    == parsed
                        .files
                        .keys()
                        .filter(|(ns, _)| ns == &namespace.name)
                        .count()
                    && tree.iter().all(|(path, bytes)| {
                        parsed.files.get(&(namespace.name.clone(), path.clone())) == Some(bytes)
                    });
                if identical {
                    Disposition::Unchanged
                } else if policy.replace.contains(&namespace.name) {
                    Disposition::Replace
                } else {
                    bail!(
                        "namespace `{}` of bundle `{}` already exists on this node and differs \
                         from the archive; pass --replace-namespace {} to replace it",
                        namespace.name,
                        manifest.bundle_id,
                        namespace.name
                    );
                }
            }
        };
        plan.push((namespace, disposition));
    }

    let mut summary = ImportSummary {
        bundle_id: manifest.bundle_id.clone(),
        created: Vec::new(),
        replaced: Vec::new(),
        unchanged: Vec::new(),
        files: 0,
    };
    for (namespace, disposition) in plan {
        match disposition {
            Disposition::Unchanged => {
                summary.unchanged.push(namespace.name.clone());
                continue;
            }
            Disposition::Create => summary.created.push(namespace.name.clone()),
            Disposition::Replace => summary.replaced.push(namespace.name.clone()),
        }
        let opened = crate::bundle_state_home::open_or_create(
            app_root,
            &manifest.bundle_id,
            &namespace.name,
            node,
            "offline-import",
        )
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
        let directory = lillux::PinnedDirectory::open(&opened.path)?
            .context("the namespace vanished after it was opened")?;
        if disposition == Disposition::Replace {
            directory.remove_contents_recursive_bounded(TREE_BUDGET)?;
        }
        for file in &namespace.files {
            let bytes = &parsed.files[&(namespace.name.clone(), file.path.clone())];
            write_file(&directory, &file.path, bytes)?;
            summary.files += 1;
        }
        directory.sync()?;
    }
    Ok(summary)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Create,
    Replace,
    Unchanged,
}

fn write_file(root: &lillux::PinnedDirectory, relative: &str, bytes: &[u8]) -> Result<()> {
    let relative = validated_relative_path(relative)?;
    let mut directory = root.try_clone()?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let name = component.as_os_str();
        if components.peek().is_some() {
            directory = directory.open_or_create_child(name, 0o700)?;
        } else {
            let mut file = directory.open_regular_create(name, true, true, 0o600)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
    }
    Ok(())
}

/// The digest an operator can compare before and after a removal: the sha256
/// of the canonical JSON listing of every `(namespace, path, sha256)` the
/// removal covers. A namespace that is absent contributes nothing.
pub fn listing_digest(
    app_root: &Path,
    node: &NodeIdentity,
    bundle_id: &str,
    namespace: Option<&str>,
) -> Result<String> {
    let Some(root) = lillux::PinnedDirectory::open(&root_path(app_root))? else {
        return Ok(lillux::sha256_hex(b"[]"));
    };
    if root.open_child_directory(OsStr::new(bundle_id))?.is_none() {
        return Ok(lillux::sha256_hex(b"[]"));
    }
    let home = open_bundle_home(app_root, bundle_id, node)?;
    let names = match namespace {
        Some(name) => {
            validate_state_home_segment("namespace", name).map_err(anyhow::Error::msg)?;
            if home.open_child_directory(OsStr::new(name))?.is_none() {
                Vec::new()
            } else {
                vec![name.to_owned()]
            }
        }
        None => namespaces_to_move(&home, None)?,
    };
    let mut listing = Vec::new();
    for name in names {
        let directory = home
            .open_child_directory(OsStr::new(&name))?
            .with_context(|| format!("namespace `{name}` vanished"))?;
        let mut files = BTreeMap::new();
        let mut remaining = TREE_BUDGET.max_entries;
        read_tree(&directory, Path::new(""), 0, &mut remaining, &mut files)?;
        for (path, bytes) in files {
            listing.push(json!({
                "namespace": name,
                "path": path,
                "sha256": lillux::sha256_hex(&bytes),
            }));
        }
    }
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&Value::Array(listing))?.as_bytes(),
    ))
}

/// What a removal deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveSummary {
    pub bundle_id: String,
    pub namespaces: Vec<String>,
    pub bundle_home_removed: bool,
}

/// Delete one namespace, or a bundle's whole home. Idempotent: what is already
/// gone is not an error. A whole-home removal also deletes the signed record;
/// a namespace removal keeps it so the remaining namespaces stay verified.
pub fn remove_state(
    app_root: &Path,
    node: &NodeIdentity,
    bundle_id: &str,
    namespace: Option<&str>,
) -> Result<RemoveSummary> {
    validate_state_home_segment_or_bundle(bundle_id)?;
    let mut summary = RemoveSummary {
        bundle_id: bundle_id.to_owned(),
        namespaces: Vec::new(),
        bundle_home_removed: false,
    };
    let Some(root) = lillux::PinnedDirectory::open(&root_path(app_root))? else {
        return Ok(summary);
    };
    let bundle_name = OsStr::new(bundle_id);
    let Some(home) = root.open_child_directory(bundle_name)? else {
        return Ok(summary);
    };
    // A home this node cannot vouch for is not deleted on its say-so.
    open_bundle_home(app_root, bundle_id, node)?;
    let names = match namespace {
        Some(name) => {
            validate_state_home_segment("namespace", name).map_err(anyhow::Error::msg)?;
            vec![name.to_owned()]
        }
        None => namespaces_to_move(&home, None)?,
    };
    for name in names {
        let name_os = OsStr::new(&name);
        let Some(directory) = home.open_child_directory(name_os)? else {
            continue;
        };
        directory.remove_contents_recursive_bounded(TREE_BUDGET)?;
        ensure!(
            home.remove_empty_child_if_same(name_os, &directory)?
                .is_some(),
            "namespace `{name}` changed while it was removed"
        );
        summary.namespaces.push(name);
    }
    if namespace.is_none() {
        let record = home.path().join(HOME_RECORD_NAME);
        if home
            .entry_no_follow(OsStr::new(HOME_RECORD_NAME))?
            .is_some()
        {
            std::fs::remove_file(&record).context("remove the home record")?;
        }
        ensure!(
            root.remove_empty_child_if_same(bundle_name, &home)?
                .is_some(),
            "the home of `{bundle_id}` changed while it was removed"
        );
        summary.bundle_home_removed = true;
    }
    home.sync().ok();
    root.sync()?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle_state_home::open_or_create;

    struct Node {
        app: tempfile::TempDir,
        _keys: tempfile::TempDir,
        identity: NodeIdentity,
    }

    fn node() -> Node {
        let app = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(app.path().join(ryeos_engine::AI_DIR).join("state")).unwrap();
        let keys = tempfile::tempdir().unwrap();
        let identity = NodeIdentity::create(&keys.path().join("key.pem")).unwrap();
        Node {
            app,
            _keys: keys,
            identity,
        }
    }

    fn seed(node: &Node, bundle: &str, namespace: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let opened =
            open_or_create(node.app.path(), bundle, namespace, &node.identity, "gen").unwrap();
        let directory = lillux::PinnedDirectory::open(&opened.path)
            .unwrap()
            .unwrap();
        for (path, bytes) in files {
            write_file(&directory, path, bytes).unwrap();
        }
        opened.path
    }

    fn everything_backed(_: &str, _: &str) -> std::result::Result<(), String> {
        Ok(())
    }

    fn policy<'a>(
        key: &'a dyn Fn(&str) -> Option<VerifyingKey>,
        backed: &'a dyn Fn(&str, &str) -> std::result::Result<(), String>,
        replace: &'a [String],
    ) -> ImportPolicy<'a> {
        ImportPolicy {
            signer_key: key,
            namespace_backed: backed,
            replace,
        }
    }

    #[test]
    fn export_import_round_trip_preserves_bytes() {
        let source = node();
        seed(
            &source,
            "central-auth",
            "realm-a",
            &[
                ("sessions/one.json", b"{\"a\":1}"),
                ("top.bin", &[0, 1, 2, 255]),
            ],
        );
        seed(&source, "central-auth", "realm-b", &[("x", b"b")]);
        let (archive, summary) =
            export_archive(source.app.path(), &source.identity, "central-auth", None).unwrap();
        assert_eq!(summary.namespaces, ["realm-a", "realm-b"]);
        assert_eq!(summary.files, 3);

        // A fresh node that pinned the source node's key installs it.
        let target = node();
        let source_key = *source.identity.verifying_key();
        let source_fp = source.identity.principal_id();
        let trust = move |fp: &str| (fp == source_fp).then_some(source_key);
        let imported = import_archive(
            target.app.path(),
            &target.identity,
            &archive,
            &policy(&trust, &everything_backed, &[]),
        )
        .unwrap();
        assert_eq!(imported.created, ["realm-a", "realm-b"]);
        assert_eq!(imported.files, 3);
        let landed = namespace_path_for(&target, "central-auth", "realm-a");
        assert_eq!(
            std::fs::read(landed.join("sessions/one.json")).unwrap(),
            b"{\"a\":1}"
        );
        assert_eq!(
            std::fs::read(landed.join("top.bin")).unwrap(),
            [0, 1, 2, 255]
        );

        // The imported home is the target node's own, and re-exports identically.
        let (again, _) = export_archive(
            target.app.path(),
            &target.identity,
            "central-auth",
            Some("realm-a"),
        )
        .unwrap();
        let reparsed = parse_archive(&again).unwrap();
        assert_eq!(
            reparsed.manifest.source_node,
            target.identity.principal_id()
        );
        assert_eq!(
            reparsed.files[&("realm-a".to_owned(), "top.bin".to_owned())],
            [0, 1, 2, 255]
        );

        // Running the same import again converges: nothing differs.
        let rerun = import_archive(
            target.app.path(),
            &target.identity,
            &archive,
            &policy(&trust, &everything_backed, &[]),
        )
        .unwrap();
        assert_eq!(rerun.unchanged, ["realm-a", "realm-b"]);
        assert!(rerun.created.is_empty());
    }

    fn namespace_path_for(node: &Node, bundle: &str, namespace: &str) -> PathBuf {
        crate::bundle_state_home::namespace_path(node.app.path(), bundle, namespace)
    }

    #[test]
    fn import_refuses_unsigned_or_foreign_manifest() {
        let source = node();
        seed(&source, "central-auth", "realm-a", &[("x", b"1")]);
        let (archive, _) =
            export_archive(source.app.path(), &source.identity, "central-auth", None).unwrap();
        let target = node();

        // A signer this node has not pinned is refused.
        let nobody = |_: &str| -> Option<VerifyingKey> { None };
        let error = import_archive(
            target.app.path(),
            &target.identity,
            &archive,
            &policy(&nobody, &everything_backed, &[]),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("neither is nor has pinned"),
            "{error:#}"
        );

        // Trusting the wrong key for the claimed signer fails the signature
        // binding, as does tampering with the manifest body.
        let other = node();
        let other_key = *other.identity.verifying_key();
        let claimed = source.identity.principal_id();
        let wrong_key = move |fp: &str| (fp == claimed).then_some(other_key);
        assert!(
            import_archive(
                target.app.path(),
                &target.identity,
                &archive,
                &policy(&wrong_key, &everything_backed, &[]),
            )
            .is_err()
        );

        // Rewriting the manifest (here, the bundle id) without the source's
        // key breaks the signature; so does corrupting a file's bytes.
        let source_key = *source.identity.verifying_key();
        let source_fp = source.identity.principal_id();
        let trust = move |fp: &str| (fp == source_fp).then_some(source_key);
        let parsed = parse_archive(&archive).unwrap();
        let mut forged = parsed.manifest.clone();
        forged.bundle_id = "other-bundle".to_owned();
        let mut builder = tar::Builder::new(Vec::new());
        append_entry(
            &mut builder,
            MANIFEST_ENTRY,
            &serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        append_entry(&mut builder, "files/realm-a/x", b"1").unwrap();
        let forged_archive = builder.into_inner().unwrap();
        let error = import_archive(
            target.app.path(),
            &target.identity,
            &forged_archive,
            &policy(&trust, &everything_backed, &[]),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("signature"), "{error:#}");

        let mut builder = tar::Builder::new(Vec::new());
        append_entry(
            &mut builder,
            MANIFEST_ENTRY,
            &serde_json::to_vec(&parsed.manifest).unwrap(),
        )
        .unwrap();
        append_entry(&mut builder, "files/realm-a/x", b"tampered").unwrap();
        let corrupted = builder.into_inner().unwrap();
        let error = import_archive(
            target.app.path(),
            &target.identity,
            &corrupted,
            &policy(&trust, &everything_backed, &[]),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("does not match"), "{error:#}");

        // A bundle that does not declare the namespace refuses before writing.
        let unbacked =
            |_: &str, _: &str| -> std::result::Result<(), String> { Err("not installed".into()) };
        let error = import_archive(
            target.app.path(),
            &target.identity,
            &archive,
            &policy(&trust, &unbacked, &[]),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("not installed"), "{error:#}");
        assert!(!root_path(target.app.path()).join("central-auth").exists());
    }

    #[test]
    fn import_refuses_existing_namespace_without_replace() {
        let source = node();
        seed(&source, "central-auth", "realm-a", &[("x", b"new")]);
        let (archive, _) =
            export_archive(source.app.path(), &source.identity, "central-auth", None).unwrap();
        let key = *source.identity.verifying_key();
        let fp = source.identity.principal_id();
        let trust = move |candidate: &str| (candidate == fp).then_some(key);

        let target = node();
        let landed = seed(
            &target,
            "central-auth",
            "realm-a",
            &[("x", b"old"), ("y", b"keep")],
        );
        let error = import_archive(
            target.app.path(),
            &target.identity,
            &archive,
            &policy(&trust, &everything_backed, &[]),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("--replace-namespace realm-a"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(landed.join("x")).unwrap(), b"old");

        // Naming a namespace the archive lacks is refused outright.
        let stray = ["realm-z".to_owned()];
        assert!(
            import_archive(
                target.app.path(),
                &target.identity,
                &archive,
                &policy(&trust, &everything_backed, &stray),
            )
            .is_err()
        );

        // With the explicit replace the old content is gone and the new is in.
        let replace = ["realm-a".to_owned()];
        let done = import_archive(
            target.app.path(),
            &target.identity,
            &archive,
            &policy(&trust, &everything_backed, &replace),
        )
        .unwrap();
        assert_eq!(done.replaced, ["realm-a"]);
        assert_eq!(std::fs::read(landed.join("x")).unwrap(), b"new");
        assert!(!landed.join("y").exists());
    }

    #[test]
    fn remove_deletes_after_listing_and_keeps_the_rest() {
        let n = node();
        seed(&n, "central-auth", "realm-a", &[("a/b", b"1")]);
        let kept = seed(&n, "central-auth", "realm-b", &[("c", b"2")]);
        let empty = listing_digest(n.app.path(), &n.identity, "nobody", None).unwrap();
        assert_eq!(empty, lillux::sha256_hex(b"[]"));
        let listed =
            listing_digest(n.app.path(), &n.identity, "central-auth", Some("realm-a")).unwrap();
        assert_ne!(listed, empty);
        // The digest covers content, so a changed file changes it.
        let before = listing_digest(n.app.path(), &n.identity, "central-auth", None).unwrap();
        let removed =
            remove_state(n.app.path(), &n.identity, "central-auth", Some("realm-a")).unwrap();
        assert_eq!(removed.namespaces, ["realm-a"]);
        assert!(!removed.bundle_home_removed);
        assert!(kept.join("c").exists());
        assert_ne!(
            listing_digest(n.app.path(), &n.identity, "central-auth", None).unwrap(),
            before
        );
        // Removing again finds nothing and succeeds.
        let again =
            remove_state(n.app.path(), &n.identity, "central-auth", Some("realm-a")).unwrap();
        assert!(again.namespaces.is_empty());

        let whole = remove_state(n.app.path(), &n.identity, "central-auth", None).unwrap();
        assert!(whole.bundle_home_removed);
        assert!(!root_path(n.app.path()).join("central-auth").exists());
        // A home the node cannot vouch for is not deleted on its say-so.
        seed(&n, "other", "ns", &[("f", b"1")]);
        std::fs::remove_file(root_path(n.app.path()).join("other").join(HOME_RECORD_NAME)).unwrap();
        assert!(remove_state(n.app.path(), &n.identity, "other", None).is_err());
    }

    #[test]
    fn export_refuses_links_and_specials() {
        let n = node();
        let dir = seed(&n, "central-auth", "realm-a", &[("x", b"1")]);
        lillux::PinnedDirectory::open(&dir)
            .unwrap()
            .unwrap()
            .create_symlink(OsStr::new("link"), b"x")
            .unwrap();
        let error = export_archive(n.app.path(), &n.identity, "central-auth", None).unwrap_err();
        assert!(format!("{error:#}").contains("regular files"), "{error:#}");
    }

    #[test]
    fn archive_paths_cannot_escape() {
        for bad in ["../x", "/abs", "a//b", "a/./b", "a\\b", "", "a/"] {
            assert!(validated_relative_path(bad).is_err(), "{bad:?}");
        }
        assert!(validated_relative_path("a/b.c").is_ok());
        let mut builder = tar::Builder::new(Vec::new());
        append_entry(&mut builder, MANIFEST_ENTRY, b"{}").unwrap();
        assert!(parse_archive(&builder.into_inner().unwrap()).is_err());
    }
}
