//! Owner+name project identity and the project's own name declaration.
//!
//! A project is identified by the principal that owns it and a name:
//! `fp:<64 lowercase hex>/<name>`. The name comes from the project tree's
//! `.ai/project.yaml`; that declaration is a claim, never trust. The owner is
//! never read from the tree: it is the authenticated launcher of a registered
//! live checkout, or the principal whose head key a pushed head was found
//! under. These types carry no authority on their own; nothing here decides
//! whether a signer is admitted.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Relative path of a project's name declaration inside its root.
pub const PROJECT_DECLARATION_PATH: &str = ".ai/project.yaml";

/// The only declaration schema this node reads.
pub const PROJECT_DECLARATION_SCHEMA: u32 = 1;

/// Upper bound on a declaration's size; the document holds two short fields.
pub const MAX_PROJECT_DECLARATION_BYTES: u64 = 4 * 1024;

const MAX_PROJECT_NAME_LEN: usize = 64;

/// A project name: `[a-z0-9][a-z0-9._-]{0,63}`.
///
/// No `/` or `:` can appear, so `fp:<owner>/<name>` always splits at the
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
/// Displayed and serialized as `fp:<64 lowercase hex>/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectIdentity {
    owner: String,
    name: ProjectName,
}

impl ProjectIdentity {
    /// `owner` must be exactly `fp:<64 lowercase hex>`; no normalization.
    pub fn new(owner: impl Into<String>, name: ProjectName) -> Result<Self> {
        let owner = owner.into();
        crate::refs::principal_storage_key(&owner)
            .with_context(|| format!("project owner {owner:?}"))?;
        Ok(Self { owner, name })
    }

    /// The owning principal, `fp:<64 lowercase hex>`.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &ProjectName {
        &self.name
    }
}

impl FromStr for ProjectIdentity {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (owner, name) = value
            .split_once('/')
            .with_context(|| format!("project identity must be fp:<owner>/<name>: {value:?}"))?;
        Self::new(owner, ProjectName::new(name)?)
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
    /// The head this target names for `principal_id`, the authenticated
    /// caller.
    pub fn head_key(&self, principal_id: &str) -> Result<ProjectHeadKey> {
        match self {
            Self::Named { name } => ProjectHeadKey::owned_by(principal_id, name),
            Self::NoProject => ProjectHeadKey::no_project(principal_id),
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

/// Where a pushed project head lives: the owner's storage key and the
/// sha256 of the project name, `projects/<owner hex>/<sha256(name)>/head`.
///
/// The name hash is only ever computed from a [`ProjectName`], so a head
/// cannot be keyed by a path or any other caller string. The owner must come
/// from the authenticated principal or from a head key the node found, never
/// from request input. [`ProjectHeadKey::from_sealed`] exists only to carry
/// coordinates read back from a sealed record or the ref tree, which were
/// themselves built by one of the name constructors.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectHeadKey {
    owner_key: String,
    name_hash: String,
}

impl ProjectHeadKey {
    /// The head of `identity`.
    pub fn for_project(identity: &ProjectIdentity) -> Self {
        Self::owned_by(identity.owner(), identity.name())
            .expect("a project identity's owner is a valid principal")
    }

    /// The head `principal_id` owns under `name`. `principal_id` must be
    /// exactly `fp:<64 lowercase hex>`.
    pub fn owned_by(principal_id: &str, name: &ProjectName) -> Result<Self> {
        let owner_key = crate::refs::principal_storage_key(principal_id)?.to_owned();
        Ok(Self {
            owner_key,
            name_hash: lillux::cas::sha256_hex(name.as_str().as_bytes()),
        })
    }

    /// The caller's `--no-project` head: a per-principal head of an empty
    /// tree. Its name hash is of a reserved string no [`ProjectName`] can be
    /// (names cannot start with `_`), so it never collides with a project.
    pub fn no_project(principal_id: &str) -> Result<Self> {
        let owner_key = crate::refs::principal_storage_key(principal_id)?.to_owned();
        Ok(Self {
            owner_key,
            name_hash: lillux::cas::sha256_hex(NO_PROJECT_HEAD_NAME.as_bytes()),
        })
    }

    /// Coordinates read back from a sealed record or a found head key.
    pub fn from_sealed(owner_key: &str, name_hash: &str) -> Result<Self> {
        for (label, value) in [("owner key", owner_key), ("project name hash", name_hash)] {
            if value.len() != 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                bail!("project head {label} must be 64 lowercase hex characters: {value:?}");
            }
        }
        Ok(Self {
            owner_key: owner_key.to_owned(),
            name_hash: name_hash.to_owned(),
        })
    }

    /// The owner's raw fingerprint hex (no `fp:` prefix).
    pub fn owner_key(&self) -> &str {
        &self.owner_key
    }

    /// The owning principal, `fp:<64 lowercase hex>`.
    pub fn owner_principal(&self) -> String {
        format!("fp:{}", self.owner_key)
    }

    /// sha256 of the project name, lowercase hex.
    pub fn name_hash(&self) -> &str {
        &self.name_hash
    }

    /// The key of a sealed ref path, which must be exactly
    /// `projects/<owner hex>/<name hash>/head`. The coordinates carry no
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

    /// The ref path, `projects/<owner hex>/<name hash>/head`.
    pub fn ref_path(&self) -> String {
        crate::refs::project_head_ref_path(&self.owner_key, &self.name_hash)
            .expect("validated project head coordinates")
    }
}

/// A project tree's `.ai/project.yaml`: the name the tree claims. Unsigned on
/// purpose: a signature is not bound to a location, so a copied tree would
/// carry a valid one. Trust comes from registration or a head key, never from
/// this file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDeclaration {
    pub schema: u32,
    pub name: ProjectName,
}

impl ProjectDeclaration {
    /// Parse and validate declaration bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_PROJECT_DECLARATION_BYTES {
            bail!(
                "{PROJECT_DECLARATION_PATH} is larger than {MAX_PROJECT_DECLARATION_BYTES} bytes"
            );
        }
        let declaration: Self = serde_yaml::from_slice(bytes)
            .with_context(|| format!("decode {PROJECT_DECLARATION_PATH}"))?;
        if declaration.schema != PROJECT_DECLARATION_SCHEMA {
            bail!(
                "{PROJECT_DECLARATION_PATH} schema {} is not supported (expected {PROJECT_DECLARATION_SCHEMA})",
                declaration.schema
            );
        }
        Ok(declaration)
    }

    /// Read the declaration under a pinned project root without following
    /// symlinks. `Ok(None)` when the root declares no name.
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

    /// The name a stored project tree declares, read from its CAS objects.
    /// `Ok(None)` when the tree holds no declaration.
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

    /// The name declared by the project tree at `root`, an existing
    /// directory. `Ok(None)` when it declares none.
    pub fn declared_name(root: &Path) -> Result<Option<ProjectName>> {
        let pinned = lillux::PinnedDirectory::open(root)?
            .with_context(|| format!("project root {} is not a directory", root.display()))?;
        Ok(Self::read(&pinned)
            .with_context(|| format!("read the project declaration of {}", root.display()))?
            .map(|declaration| declaration.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_key_is_the_owner_and_the_name_hash_and_round_trips_its_ref_path() {
        let name = ProjectName::new("demo").unwrap();
        let key = ProjectHeadKey::owned_by(OWNER, &name).unwrap();
        assert_eq!(key.owner_principal(), OWNER);
        assert_eq!(key.name_hash(), lillux::cas::sha256_hex(b"demo"));
        assert_eq!(
            key.ref_path(),
            format!("projects/{}/{}/head", &OWNER[3..], key.name_hash())
        );
        assert_eq!(ProjectHeadKey::parse_ref_path(&key.ref_path()).unwrap(), key);
        let identity = ProjectIdentity::new(OWNER, name).unwrap();
        assert_eq!(ProjectHeadKey::for_project(&identity), key);
        for bad in [
            "projects/a/b/head".to_owned(),
            format!("{}/extra", key.ref_path()),
            format!("chains/{}/{}/head", &OWNER[3..], key.name_hash()),
        ] {
            assert!(ProjectHeadKey::parse_ref_path(&bad).is_err(), "{bad}");
        }
        assert!(ProjectHeadKey::owned_by("fp:short", &ProjectName::new("demo").unwrap()).is_err());
    }

    const OWNER: &str = "fp:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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
        let identity =
            ProjectIdentity::new(OWNER, ProjectName::new("ryeos-next").unwrap()).unwrap();
        let text = identity.to_string();
        assert_eq!(text, format!("{OWNER}/ryeos-next"));
        assert_eq!(text.parse::<ProjectIdentity>().unwrap(), identity);
        let json = serde_json::to_string(&identity).unwrap();
        assert_eq!(json, format!("\"{OWNER}/ryeos-next\""));
        assert_eq!(
            serde_json::from_str::<ProjectIdentity>(&json).unwrap(),
            identity
        );
        assert_eq!(identity.owner(), OWNER);
        assert_eq!(identity.name().as_str(), "ryeos-next");
    }

    #[test]
    fn identity_refuses_malformed_owners_and_names() {
        for bad in [
            "ryeos-next",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/x",
            "fp:0123/x",
            "fp:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef/x",
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
    fn declarations_parse_strictly() {
        let declaration = ProjectDeclaration::parse(b"schema: 1\nname: ryeos-next\n").unwrap();
        assert_eq!(declaration.name.as_str(), "ryeos-next");
        for bad in [
            &b"schema: 2\nname: x\n"[..],
            b"name: x\n",
            b"schema: 1\n",
            b"schema: 1\nname: X\n",
            b"schema: 1\nname: x\nowner: fp:abc\n",
            b"- not a map\n",
        ] {
            assert!(
                ProjectDeclaration::parse(bad).is_err(),
                "{:?} must be refused",
                String::from_utf8_lossy(bad)
            );
        }
        let oversized = format!("schema: 1\nname: x\n# {}\n", "a".repeat(4096));
        assert!(ProjectDeclaration::parse(oversized.as_bytes()).is_err());
    }

    #[test]
    fn declarations_are_read_from_a_pinned_root_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert_eq!(ProjectDeclaration::read(&pinned).unwrap(), None);

        std::fs::create_dir(root.path().join(".ai")).unwrap();
        assert_eq!(ProjectDeclaration::read(&pinned).unwrap(), None);

        std::fs::write(
            root.path().join(PROJECT_DECLARATION_PATH),
            "schema: 1\nname: demo\n",
        )
        .unwrap();
        let read = ProjectDeclaration::read(&pinned).unwrap().unwrap();
        assert_eq!(read.name.as_str(), "demo");

        // A declaration reached through a symlink is never read.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(
            elsewhere.path().join("project.yaml"),
            "schema: 1\nname: other\n",
        )
        .unwrap();
        std::fs::remove_file(root.path().join(PROJECT_DECLARATION_PATH)).unwrap();
        let target = elsewhere.path().join("project.yaml");
        lillux::PinnedDirectory::open(&root.path().join(".ai"))
            .unwrap()
            .unwrap()
            .create_symlink(
                std::ffi::OsStr::new("project.yaml"),
                target.to_str().unwrap().as_bytes(),
            )
            .unwrap();
        assert!(
            !matches!(ProjectDeclaration::read(&pinned), Ok(Some(_))),
            "a symlinked declaration must never be read"
        );
    }
}
