//! Owner+name project identity and the project's own name declaration.
//!
//! A project is identified by the principal that owns it and a name:
//! `pid:v1:<64 lowercase hex>/<name>`. The owner is a principal id, never a
//! key fingerprint (see [`crate::principal_id`]). The name comes from the
//! project's `.ai/manifest.yaml` `name:`; that is a claim, never
//! trust. The owner is never read from the tree: it is the principal the
//! node's resolver says the authenticated launcher of a registered live
//! checkout speaks for, or the principal whose head key a pushed head was
//! found under. These types carry no authority on their own; nothing here decides
//! whether a signer is admitted.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::principal_id::PrincipalId;

/// Relative path of the file whose `name:` is a project's name: the
/// project's own manifest.
pub const PROJECT_DECLARATION_PATH: &str = ".ai/manifest.yaml";

/// Upper bound on a manifest read for its name.
pub const MAX_PROJECT_DECLARATION_BYTES: u64 = 64 * 1024;

const MAX_PROJECT_NAME_LEN: usize = 64;

/// A project name: `[a-z0-9][a-z0-9._-]{0,63}`.
///
/// No `/` or `:` can appear, so `pid:v1:<owner>/<name>` always splits at the
/// first `/` without ambiguity, and a name is always one path component.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectName(String);

impl ProjectName {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_project_name(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_project_name(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_PROJECT_NAME_LEN {
        bail!(
            "project name must be 1 to {MAX_PROJECT_NAME_LEN} characters: {:?}",
            value
        );
    }
    let mut bytes = value.bytes();
    let first = bytes.next().expect("non-empty name");
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        bail!("project name must start with a lowercase letter or digit: {value:?}");
    }
    if let Some(bad) = bytes.find(|byte| {
        !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-'))
    }) {
        bail!(
            "project name may contain only lowercase letters, digits, '.', '_' and '-': {value:?} has {:?}",
            char::from(bad)
        );
    }
    Ok(())
}

impl TryFrom<String> for ProjectName {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<ProjectName> for String {
    fn from(value: ProjectName) -> Self {
        value.0
    }
}

impl FromStr for ProjectName {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl fmt::Display for ProjectName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A project's identity: the owning principal and the project name.
/// Displayed and serialized as `pid:v1:<64 lowercase hex>/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectIdentity {
    owner: PrincipalId,
    name: ProjectName,
}

impl ProjectIdentity {
    pub fn new(owner: PrincipalId, name: ProjectName) -> Self {
        Self { owner, name }
    }

    /// The owning principal.
    pub fn owner(&self) -> &PrincipalId {
        &self.owner
    }

    pub fn name(&self) -> &ProjectName {
        &self.name
    }
}

impl FromStr for ProjectIdentity {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (owner, name) = value.split_once('/').with_context(|| {
            format!("project identity must be pid:v1:<owner>/<name>: {value:?}")
        })?;
        let owner: PrincipalId = owner
            .parse()
            .with_context(|| format!("project identity owner in {value:?}"))?;
        Ok(Self::new(owner, ProjectName::new(name)?))
    }
}

impl TryFrom<String> for ProjectIdentity {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl From<ProjectIdentity> for String {
    fn from(value: ProjectIdentity) -> Self {
        value.to_string()
    }
}

impl fmt::Display for ProjectIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

/// The project a plan, item or execution belongs to, for signer project
/// scopes. There is deliberately no default and no bare `None`: every site
/// names which case it is. Only `Stated` can match a scoped signer; nothing
/// here is a path, so a checkout or label can never be matched.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalProject {
    /// An owner+name identity this node has established: a checkout the
    /// launcher registered under its own owner, or a head this node holds
    /// for that owner and name at the exact snapshot.
    Stated(ProjectIdentity),
    /// No identity. A signer scoped to specific projects admits nothing.
    Absent(LogicalProjectAbsence),
}

/// Why a plan, item or execution states no project identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalProjectAbsence {
    /// The subject has no project at all.
    Projectless,
    /// A live checkout the launcher has not registered under its own owner,
    /// that declares another name, or a launcher without live write.
    NotRegistered,
    /// A pinned generation whose owner's head for its name is not held by
    /// this node at that snapshot.
    ForeignHeadNotHeld,
    /// A project tree that declares no name.
    NoProjectName,
    /// A caller that states no project (for example a pinned generation
    /// known only by snapshot).
    NotStated,
}

impl LogicalProject {
    /// The stated identity, if any.
    pub fn identity(&self) -> Option<&ProjectIdentity> {
        match self {
            Self::Stated(identity) => Some(identity),
            Self::Absent(_) => None,
        }
    }
}

impl fmt::Display for LogicalProjectAbsence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Projectless => "projectless",
            Self::NotRegistered => "no project identity for this session",
            Self::ForeignHeadNotHeld => "this node does not hold the owner's head for this project",
            Self::NoProjectName => "the checkout's manifest names no project",
            Self::NotStated => "no project stated",
        })
    }
}

impl fmt::Display for LogicalProject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stated(identity) => write!(formatter, "{identity}"),
            Self::Absent(reason) => write!(formatter, "(none: {reason})"),
        }
    }
}

/// The reserved head name of `--no-project` pushes. Not a valid
/// [`ProjectName`].
pub const NO_PROJECT_HEAD_NAME: &str = "__no_project__";

/// Which of the caller's heads a push, upload or head read targets.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PushedProject {
    /// A project head, keyed by the name its snapshot declares.
    Named { name: ProjectName },
    /// The per-principal `--no-project` head, whose snapshot is an empty tree.
    NoProject,
}

impl PushedProject {
    /// The head this target names in `owner`'s namespace. `owner` is the
    /// principal the node's resolver says the authenticated caller speaks
    /// for, never request input.
    pub fn head_key(&self, owner: &PrincipalId) -> ProjectHeadKey {
        match self {
            Self::Named { name } => ProjectHeadKey::owned_by(owner, name),
            Self::NoProject => ProjectHeadKey::no_project(owner),
        }
    }

    /// Refuse unless a snapshot tree that declares `declared` and holds
    /// `file_count` files is this target's content: a named head needs the
    /// same declared name; the no-project head needs an empty tree.
    pub fn require_content(&self, declared: Option<&ProjectName>, file_count: usize) -> Result<()> {
        match self {
            Self::Named { name } if declared == Some(name) => Ok(()),
            Self::Named { name } => bail!(
                "project_name_mismatch: the request names project {name} but the snapshot declares {}",
                declared.map_or_else(|| "no name".to_owned(), ToString::to_string)
            ),
            Self::NoProject if file_count == 0 => Ok(()),
            Self::NoProject => bail!("a no-project head must point at an empty project tree"),
        }
    }
}

impl PushedProject {
    /// A named target for fixtures; panics on an invalid name.
    pub fn test_named(name: &str) -> Self {
        Self::Named {
            name: ProjectName::new(name).expect("valid test project name"),
        }
    }
}

impl fmt::Display for PushedProject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named { name } => write!(formatter, "{name}"),
            Self::NoProject => formatter.write_str("(no project)"),
        }
    }
}

/// Where a pushed project head lives: the owner principal's storage
/// component and the sha256 of the project name,
/// `projects/pid-v1-<hex>/<sha256(name)>/head`.
///
/// The name hash is only ever computed from a [`ProjectName`], so a head
/// cannot be keyed by a path or any other caller string. The owner must come
/// from the authenticated principal or from a head key the node found, never
/// from request input; it is always a principal the node's resolver
/// produced. [`ProjectHeadKey::from_sealed`] exists only to carry
/// coordinates read back from a sealed record or the ref tree, which were
/// themselves built by one of the name constructors.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectHeadKey {
    owner: PrincipalId,
    name_hash: String,
}

impl ProjectHeadKey {
    /// The head of `identity`.
    pub fn for_project(identity: &ProjectIdentity) -> Self {
        Self::owned_by(identity.owner(), identity.name())
    }

    /// The head `owner` owns under `name`.
    pub fn owned_by(owner: &PrincipalId, name: &ProjectName) -> Self {
        Self {
            owner: owner.clone(),
            name_hash: lillux::cas::sha256_hex(name.as_str().as_bytes()),
        }
    }

    /// The caller's `--no-project` head: a per-principal head of an empty
    /// tree. Its name hash is of a reserved string no [`ProjectName`] can be
    /// (names cannot start with `_`), so it never collides with a project.
    pub fn no_project(owner: &PrincipalId) -> Self {
        Self {
            owner: owner.clone(),
            name_hash: lillux::cas::sha256_hex(NO_PROJECT_HEAD_NAME.as_bytes()),
        }
    }

    /// Coordinates read back from a sealed record or a found head key:
    /// `owner_component` is a principal storage component
    /// (`pid-v1-<64 hex>`). A bare fingerprint hex (the pre-cut layout)
    /// does not parse.
    pub fn from_sealed(owner_component: &str, name_hash: &str) -> Result<Self> {
        let owner =
            PrincipalId::from_storage_component(owner_component).context("project head owner")?;
        if name_hash.len() != 64
            || !name_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("project head name hash must be 64 lowercase hex characters: {name_hash:?}");
        }
        Ok(Self {
            owner,
            name_hash: name_hash.to_owned(),
        })
    }

    /// The owner's storage component, `pid-v1-<64 hex>`: the ref path
    /// segment and directory name.
    pub fn owner_component(&self) -> String {
        self.owner.storage_component()
    }

    /// The owning principal.
    pub fn owner(&self) -> &PrincipalId {
        &self.owner
    }

    /// sha256 of the project name, lowercase hex.
    pub fn name_hash(&self) -> &str {
        &self.name_hash
    }

    /// The key of a sealed ref path, which must be exactly
    /// `projects/pid-v1-<hex>/<name hash>/head`. The coordinates carry no
    /// path or name; nothing recovers either from them.
    pub fn parse_ref_path(ref_path: &str) -> Result<Self> {
        let mut components = ref_path.split('/');
        match (
            components.next(),
            components.next(),
            components.next(),
            components.next(),
            components.next(),
        ) {
            (Some("projects"), Some(owner), Some(name_hash), Some("head"), None) => {
                Self::from_sealed(owner, name_hash)
            }
            _ => bail!("not a project head ref path: {ref_path:?}"),
        }
    }

    /// The ref path, `projects/pid-v1-<hex>/<name hash>/head`.
    pub fn ref_path(&self) -> String {
        crate::refs::project_head_ref_path(&self.owner_component(), &self.name_hash)
            .expect("validated project head coordinates")
    }
}

/// The project name a tree claims: the top-level `name:` of its
/// `.ai/manifest.yaml`, the same name its items are namespaced under. It is
/// an unverified claim: the manifest's signature is not checked here, and a
/// signature would not be bound to a location anyway, so a copied tree would
/// carry a valid one. Trust comes from a checkout registration or a head
/// key, never from this file. A name outside the [`ProjectName`] grammar is
/// refused, never normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDeclaration {
    pub name: ProjectName,
}

impl ProjectDeclaration {
    /// Parse manifest bytes and take the project name from `name:`.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_PROJECT_DECLARATION_BYTES {
            bail!(
                "project_manifest_invalid: {PROJECT_DECLARATION_PATH} is larger than {MAX_PROJECT_DECLARATION_BYTES} bytes"
            );
        }
        let document: serde_yaml::Value = serde_yaml::from_slice(bytes).with_context(|| {
            format!("project_manifest_invalid: decode {PROJECT_DECLARATION_PATH}")
        })?;
        let Some(name) = document.get("name") else {
            bail!(
                "project_manifest_name_missing: {PROJECT_DECLARATION_PATH} has no top-level `name:`; a project's name is its manifest name"
            );
        };
        let Some(name) = name.as_str() else {
            bail!(
                "project_manifest_name_invalid: {PROJECT_DECLARATION_PATH} `name:` is not a string"
            );
        };
        // Exactly one top-level `name:` line, written as a plain scalar: no
        // quotes, anchors, aliases, tags or trailing comment. Anything the
        // parser could have resolved into this value some other way refuses.
        let text = std::str::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!("project_manifest_invalid: {PROJECT_DECLARATION_PATH} is not UTF-8")
        })?;
        let name_lines: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("name:"))
            .collect();
        if name_lines.len() != 1 || name_lines[0].trim() != name {
            bail!(
                "project_manifest_name_invalid: {PROJECT_DECLARATION_PATH} must hold exactly one top-level `name: <name>` written as a plain value"
            );
        }
        let name = ProjectName::new(name).map_err(|error| {
            anyhow::anyhow!(
                "project_manifest_name_invalid: {PROJECT_DECLARATION_PATH} `name: {name}` is not a valid project name ({error:#})"
            )
        })?;
        Ok(Self { name })
    }

    /// Read the claim under a pinned project root without following
    /// symlinks. `Ok(None)` when the root has no manifest.
    pub fn read(root: &lillux::PinnedDirectory) -> Result<Option<Self>> {
        let Some(file) = root
            .open_pinned_regular_descendant(Path::new(PROJECT_DECLARATION_PATH), false)
            .with_context(|| format!("open {PROJECT_DECLARATION_PATH}"))?
        else {
            return Ok(None);
        };
        let observation = file.observation()?;
        let bytes = file.read_stable_bounded(&observation, MAX_PROJECT_DECLARATION_BYTES)?;
        Self::parse(&bytes).map(Some)
    }

    /// The name a stored project tree claims, read from its CAS objects.
    /// `Ok(None)` when the tree holds no manifest.
    pub fn declared_name_in_tree(
        cas: &lillux::CasStore,
        tree: &crate::objects::ProjectTree,
    ) -> Result<Option<ProjectName>> {
        let Some(file_hash) = tree.files.get(PROJECT_DECLARATION_PATH) else {
            return Ok(None);
        };
        let file = crate::objects::ProjectFile::from_value(
            &cas.get_object(file_hash)?
                .with_context(|| format!("{PROJECT_DECLARATION_PATH} file object is missing"))?,
        )?;
        let bytes = cas
            .get_blob_bounded(&file.blob_hash, MAX_PROJECT_DECLARATION_BYTES)?
            .with_context(|| format!("{PROJECT_DECLARATION_PATH} blob is missing"))?;
        Ok(Some(Self::parse(&bytes)?.name))
    }

    /// The name claimed by the project tree at `root`, an existing
    /// directory. `Ok(None)` when it has no manifest.
    pub fn declared_name(root: &Path) -> Result<Option<ProjectName>> {
        let pinned = lillux::PinnedDirectory::open(root)?
            .with_context(|| format!("project root {} is not a directory", root.display()))?;
        Ok(Self::read(&pinned)
            .with_context(|| format!("read the project name of {}", root.display()))?
            .map(|declaration| declaration.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_key_is_the_owner_and_the_name_hash_and_round_trips_its_ref_path() {
        let name = ProjectName::new("demo").unwrap();
        let owner: PrincipalId = OWNER.parse().unwrap();
        let key = ProjectHeadKey::owned_by(&owner, &name);
        assert_eq!(key.owner(), &owner);
        assert_eq!(key.name_hash(), lillux::cas::sha256_hex(b"demo"));
        assert_eq!(
            key.ref_path(),
            format!("projects/pid-v1-{}/{}/head", &OWNER[7..], key.name_hash())
        );
        assert_eq!(
            ProjectHeadKey::parse_ref_path(&key.ref_path()).unwrap(),
            key
        );
        let identity = ProjectIdentity::new(owner, name);
        assert_eq!(ProjectHeadKey::for_project(&identity), key);
        for bad in [
            "projects/a/b/head".to_owned(),
            format!("{}/extra", key.ref_path()),
            format!("chains/pid-v1-{}/{}/head", &OWNER[7..], key.name_hash()),
            // The pre-cut layout, a bare fingerprint hex owner segment.
            format!("projects/{}/{}/head", &OWNER[7..], key.name_hash()),
        ] {
            assert!(ProjectHeadKey::parse_ref_path(&bad).is_err(), "{bad}");
        }
    }

    const OWNER: &str = "pid:v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn project_names_follow_the_rule() {
        for good in ["a", "ryeos-next", "demo.v2", "a_b", "0", &"a".repeat(64)] {
            ProjectName::new(good).unwrap_or_else(|error| panic!("{good:?}: {error}"));
        }
        for bad in [
            "",
            &"a".repeat(65),
            "Ryeos",
            "-lead",
            ".hidden",
            "_x",
            "a/b",
            "a:b",
            "a b",
            "ü",
            "a\nb",
        ] {
            assert!(ProjectName::new(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn identity_round_trips_through_its_string_form() {
        let identity = ProjectIdentity::new(
            OWNER.parse().unwrap(),
            ProjectName::new("ryeos-next").unwrap(),
        );
        let text = identity.to_string();
        assert_eq!(text, format!("{OWNER}/ryeos-next"));
        assert_eq!(text.parse::<ProjectIdentity>().unwrap(), identity);
        let json = serde_json::to_string(&identity).unwrap();
        assert_eq!(json, format!("\"{OWNER}/ryeos-next\""));
        assert_eq!(
            serde_json::from_str::<ProjectIdentity>(&json).unwrap(),
            identity
        );
        assert_eq!(identity.owner().to_string(), OWNER);
        assert_eq!(identity.name().as_str(), "ryeos-next");
    }

    #[test]
    fn identity_refuses_malformed_owners_and_names() {
        for bad in [
            "ryeos-next",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/x",
            "fp:0123/x",
            "fp:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/x",
            "pid:v1:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef/x",
            &format!("{OWNER}/"),
            &format!("{OWNER}/a/b"),
            &format!("{OWNER}/Bad"),
            OWNER,
        ] {
            assert!(
                bad.parse::<ProjectIdentity>().is_err(),
                "{bad:?} must be refused"
            );
            assert!(serde_json::from_str::<ProjectIdentity>(&format!("{bad:?}")).is_err());
        }
    }

    #[test]
    fn the_project_name_is_the_manifest_name() {
        // A real signed manifest: the signature comment and every other field
        // are ignored; only `name:` is read.
        let manifest = b"# ryeos:signed:2026-10-26T23:17:07Z:abc:sig:741a\nname: ryeos\nversion: 0.1.0\ndescription: x\nrequires_kinds:\n- config\n";
        assert_eq!(
            ProjectDeclaration::parse(manifest).unwrap().name.as_str(),
            "ryeos"
        );
        for (bad, code) in [
            (&b"version: 0.1.0\n"[..], "project_manifest_name_missing"),
            (b"name: Farm\n", "project_manifest_name_invalid"),
            (b"name: a/b\n", "project_manifest_name_invalid"),
            (b"name: _x\n", "project_manifest_name_invalid"),
            (b"name: [x]\n", "project_manifest_name_invalid"),
            (b"- not a map\n", "project_manifest_name_missing"),
            (b"name: x\nname: y\n", "project_manifest"),
            (b"name: \"x\"\n", "project_manifest_name_invalid"),
            (b"other: &a x\nname: *a\n", "project_manifest_name_invalid"),
            (b"name: !!str x\n", "project_manifest_name_invalid"),
            (b"name: x # comment\n", "project_manifest_name_invalid"),
            (b"name: x\n---\nname: y\n", "project_manifest"),
        ] {
            let error = ProjectDeclaration::parse(bad).unwrap_err();
            assert!(
                format!("{error:#}").contains(code),
                "{:?}: {error:#}",
                String::from_utf8_lossy(bad)
            );
        }
        let oversized = format!("name: x\n# {}\n", "a".repeat(64 * 1024));
        assert!(ProjectDeclaration::parse(oversized.as_bytes()).is_err());
    }

    #[test]
    fn declarations_are_read_from_a_pinned_root_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert_eq!(ProjectDeclaration::read(&pinned).unwrap(), None);

        std::fs::create_dir(root.path().join(".ai")).unwrap();
        assert_eq!(ProjectDeclaration::read(&pinned).unwrap(), None);

        std::fs::write(root.path().join(PROJECT_DECLARATION_PATH), "name: demo\n").unwrap();
        let read = ProjectDeclaration::read(&pinned).unwrap().unwrap();
        assert_eq!(read.name.as_str(), "demo");

        // A declaration reached through a symlink is never read.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("manifest.yaml"), "name: other\n").unwrap();
        std::fs::remove_file(root.path().join(PROJECT_DECLARATION_PATH)).unwrap();
        let target = elsewhere.path().join("manifest.yaml");
        lillux::PinnedDirectory::open(&root.path().join(".ai"))
            .unwrap()
            .unwrap()
            .create_symlink(
                std::ffi::OsStr::new("manifest.yaml"),
                target.to_str().unwrap().as_bytes(),
            )
            .unwrap();
        assert!(
            !matches!(ProjectDeclaration::read(&pinned), Ok(Some(_))),
            "a symlinked manifest must never be read"
        );
    }
}
