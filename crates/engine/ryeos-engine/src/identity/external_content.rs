//! Signed external-content declarations and pure admission rules.
//!
//! This module deliberately performs no filesystem access, capture, CAS
//! writes, materialization, or node-policy lookup. Kinds own declaration and
//! composition semantics; state owns meaning-blind capture/storage mechanics;
//! daemon/executor orchestration supplies admitted named roots and policy.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::contracts::ItemSpace;

pub use ryeos_state::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_CONTENT_TREE_SCHEMA,
    EXTERNAL_REALIZATIONS_DERIVED_KEY, ExternalContentKind,
    ExternalContentManifestEntry as ManifestEntry, ExternalContentManifestEntryKind,
    ExternalContentManifestObject, ExternalContentMode, FILE_REALIZATION_ENTRY_PATH,
};

pub const MAX_DECLARATIONS_PER_ITEM: usize = 8;
pub const MAX_EXCLUDES_PER_DECLARATION: usize = 32;
pub const MAX_ENTRY_PATH_BYTES: usize = ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaringAuthority<'a> {
    Project,
    Node,
    Bundle(&'a str),
}

impl DeclaringAuthority<'_> {
    pub fn label(&self) -> String {
        match self {
            Self::Project => "project".to_owned(),
            Self::Node => "node".to_owned(),
            Self::Bundle(name) => format!("bundle:{name}"),
        }
    }
}

/// Signed locator classes. These are authority names, never host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalContentRoot {
    ProjectFiles,
    NodeFiles,
    Bundle(String),
}

impl Serialize for ExternalContentRoot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.label())
    }
}

impl<'de> Deserialize<'de> for ExternalContentRoot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl ExternalContentRoot {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "project_files" => Ok(Self::ProjectFiles),
            "node_files" => Ok(Self::NodeFiles),
            other => match other.strip_prefix("bundle:") {
                Some(name) if valid_bundle_name(name) => Ok(Self::Bundle(name.to_owned())),
                _ => Err(format!("unsupported external content root: {other}")),
            },
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::ProjectFiles => "project_files".to_owned(),
            Self::NodeFiles => "node_files".to_owned(),
            Self::Bundle(name) => format!("bundle:{name}"),
        }
    }

    pub fn contract_class(&self) -> &'static str {
        match self {
            Self::ProjectFiles => "project_files",
            Self::NodeFiles => "node_files",
            Self::Bundle(_) => "bundle:own",
        }
    }

    pub fn declarable_from(&self, declarer: DeclaringAuthority<'_>) -> bool {
        match (self, declarer) {
            (Self::ProjectFiles, DeclaringAuthority::Project | DeclaringAuthority::Node) => true,
            (Self::NodeFiles, DeclaringAuthority::Node) => true,
            (Self::Bundle(named), DeclaringAuthority::Bundle(own)) => named == own,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentLocator {
    pub root: ExternalContentRoot,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentDeclaration {
    pub id: String,
    pub kind: ExternalContentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<ExternalContentLocator>,
    pub mode: ExternalContentMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_hint: Option<String>,
    pub mount: String,
}

impl ExternalContentDeclaration {
    pub fn validate(&self, declarer: DeclaringAuthority<'_>) -> anyhow::Result<()> {
        self.validate_with_pending_pin(declarer, false)
    }

    pub fn validate_for_static_preview(
        &self,
        declarer: DeclaringAuthority<'_>,
    ) -> anyhow::Result<()> {
        self.validate_with_pending_pin(declarer, true)
    }

    fn validate_with_pending_pin(
        &self,
        declarer: DeclaringAuthority<'_>,
        allow_pending_pin: bool,
    ) -> anyhow::Result<()> {
        validate_declaration_id(&self.id)?;
        validate_relative_path("external content mount target", &self.mount)?;
        match &self.locator {
            Some(locator) => {
                validate_relative_path("external content locator path", &locator.path)?;
                if !locator.root.declarable_from(declarer) {
                    anyhow::bail!(
                        "an item admitted from {} may not declare external content under {}",
                        declarer.label(),
                        locator.root.label()
                    );
                }
            }
            None => {
                if self.mode != ExternalContentMode::Pinned || self.digest.is_none() {
                    anyhow::bail!(
                        "external content `{}` may omit its locator only when pinned to a digest",
                        self.id
                    );
                }
                if !self.exclude.is_empty() {
                    anyhow::bail!("locator-free external content cannot declare exclusions");
                }
            }
        }
        match (self.mode, self.digest.as_deref()) {
            (ExternalContentMode::Pinned, Some(digest)) if lillux::cas::valid_hash(digest) => {}
            (ExternalContentMode::Pinned, Some(digest))
                if allow_pending_pin && self.locator.is_some() && is_pending_pin_token(digest) => {}
            (ExternalContentMode::Pinned, Some(_)) => {
                anyhow::bail!("pinned external content digest is not canonical")
            }
            (ExternalContentMode::Pinned, None) => {
                anyhow::bail!("pinned external content must carry its expected digest")
            }
            (ExternalContentMode::Captured, None) => {}
            (ExternalContentMode::Captured, Some(_)) => {
                anyhow::bail!("captured external content must not carry a digest")
            }
        }
        if self.exclude.len() > MAX_EXCLUDES_PER_DECLARATION {
            anyhow::bail!("external content has too many authored exclusions");
        }
        for exclusion in &self.exclude {
            validate_exclude_pattern(exclusion)?;
        }
        if let Some(hint) = &self.metadata_hint {
            if hint.is_empty() || hint.len() > 255 || hint.chars().any(char::is_control) {
                anyhow::bail!("external content metadata hint is not canonical");
            }
        }
        Ok(())
    }
}

pub fn validate_declarations(
    declarations: &[ExternalContentDeclaration],
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<()> {
    validate_declaration_collection(declarations, declarer, false)
}

pub fn validate_declarations_for_static_preview(
    declarations: &[ExternalContentDeclaration],
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<()> {
    validate_declaration_collection(declarations, declarer, true)
}

fn validate_declaration_collection(
    declarations: &[ExternalContentDeclaration],
    declarer: DeclaringAuthority<'_>,
    allow_pending_pin: bool,
) -> anyhow::Result<()> {
    if declarations.len() > MAX_DECLARATIONS_PER_ITEM {
        anyhow::bail!("item declares too many external content entries");
    }
    let mut ids = BTreeSet::new();
    let mut mounts = BTreeSet::new();
    for declaration in declarations {
        if allow_pending_pin {
            declaration.validate_for_static_preview(declarer)?;
        } else {
            declaration.validate(declarer)?;
        }
        if !ids.insert(declaration.id.as_str()) {
            anyhow::bail!("external content id `{}` is duplicated", declaration.id);
        }
        if !mounts.insert(declaration.mount.as_str()) {
            anyhow::bail!(
                "external content mount `{}` is duplicated",
                declaration.mount
            );
        }
    }
    let mounts = mounts.into_iter().collect::<Vec<_>>();
    for (index, left) in mounts.iter().enumerate() {
        for right in mounts.iter().skip(index + 1) {
            if path_contains(left, right) || path_contains(right, left) {
                anyhow::bail!("external content mounts `{left}` and `{right}` overlap");
            }
        }
    }
    Ok(())
}

pub fn declarations_from_composed(
    composed: &serde_json::Value,
    contract: Option<&crate::kind_registry::ExecutionExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<Option<Vec<ExternalContentDeclaration>>> {
    declarations_from_composed_with_pending(composed, contract, declarer, false)
}

pub fn declarations_from_composed_for_static_preview(
    composed: &serde_json::Value,
    contract: Option<&crate::kind_registry::ExecutionExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<Option<Vec<ExternalContentDeclaration>>> {
    declarations_from_composed_with_pending(composed, contract, declarer, true)
}

fn declarations_from_composed_with_pending(
    composed: &serde_json::Value,
    contract: Option<&crate::kind_registry::ExecutionExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
    allow_pending_pin: bool,
) -> anyhow::Result<Option<Vec<ExternalContentDeclaration>>> {
    let authored = composed.get("external_content");
    let Some(contract) = contract else {
        if authored.is_some() {
            anyhow::bail!(
                "item declares `external_content` but its signed kind has no execution.external_content contract"
            );
        }
        return Ok(None);
    };
    let Some(value) = authored else {
        return Ok(None);
    };
    if value.is_null() {
        anyhow::bail!("external_content must be an array");
    }
    let declarations: Vec<ExternalContentDeclaration> = serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("invalid external_content declaration: {error}"))?;
    if declarations.len() > contract.max_declarations {
        anyhow::bail!(
            "item declares {} external content entries; its signed kind permits {}",
            declarations.len(),
            contract.max_declarations
        );
    }
    if allow_pending_pin {
        validate_declarations_for_static_preview(&declarations, declarer)?;
    } else {
        validate_declarations(&declarations, declarer)?;
    }
    for declaration in &declarations {
        if let Some(locator) = &declaration.locator
            && !contract
                .allowed_roots
                .iter()
                .any(|allowed| allowed == locator.root.contract_class())
        {
            anyhow::bail!(
                "external content `{}` names root `{}` which its signed kind does not permit",
                declaration.id,
                locator.root.label()
            );
        }
    }
    Ok(Some(declarations))
}

/// Derive declaration authority from verified resolution provenance. This is
/// pure admission logic: it does not resolve or open a host path.
pub fn declaring_authority(
    resolution: &crate::resolution::ResolutionOutput,
) -> anyhow::Result<DeclaringAuthority<'_>> {
    use crate::contracts::ItemSourceRoot;

    match (&resolution.root.source_root, resolution.root.source_space) {
        (ItemSourceRoot::Project, ItemSpace::Project) => Ok(DeclaringAuthority::Project),
        (ItemSourceRoot::Node, ItemSpace::Node) => Ok(DeclaringAuthority::Node),
        (ItemSourceRoot::Bundle { name }, ItemSpace::Bundle) => {
            Ok(DeclaringAuthority::Bundle(name))
        }
        (identity, space) => anyhow::bail!(
            "external-content declarer has non-authoritative or incoherent source root {identity:?} for {} space",
            space.as_str()
        ),
    }
}

fn validate_relative_path(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() > MAX_ENTRY_PATH_BYTES {
        anyhow::bail!("{label} exceeds {MAX_ENTRY_PATH_BYTES} bytes");
    }
    ryeos_state::objects::validate_canonical_project_relative_path(value)
        .map_err(|error| anyhow::anyhow!("{label}: {error}"))
}

fn validate_declaration_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        anyhow::bail!("external content id has a non-canonical value: {id:?}");
    }
    Ok(())
}

fn is_pending_pin_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !lillux::cas::valid_hash(value)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn validate_exclude_pattern(pattern: &str) -> anyhow::Result<()> {
    if pattern.is_empty() || pattern.len() > 128 || pattern.contains('/') {
        anyhow::bail!("external content exclusion is not a canonical basename pattern");
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        if !suffix.starts_with('.') || suffix.len() < 2 || suffix.contains('*') {
            anyhow::bail!("external content suffix exclusion must use `*.ext`");
        }
    } else if pattern.contains('*') {
        anyhow::bail!("external content exclusion supports one leading suffix wildcard only");
    }
    Ok(())
}

fn valid_bundle_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn path_contains(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(roots: &[&str], max: usize) -> crate::kind_registry::ExecutionExternalContentDecl {
        crate::kind_registry::ExecutionExternalContentDecl {
            realization_derived: EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
            allowed_roots: roots.iter().map(|value| (*value).to_owned()).collect(),
            max_declarations: max,
            large_content: None,
        }
    }

    #[test]
    fn absence_empty_and_null_are_distinct() {
        let contract = contract(&["project_files"], 2);
        assert!(
            declarations_from_composed(
                &serde_json::json!({}),
                Some(&contract),
                DeclaringAuthority::Project
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(
            declarations_from_composed(
                &serde_json::json!({"external_content": []}),
                Some(&contract),
                DeclaringAuthority::Project
            )
            .unwrap(),
            Some(Vec::new())
        );
        assert!(
            declarations_from_composed(
                &serde_json::json!({"external_content": null}),
                Some(&contract),
                DeclaringAuthority::Project
            )
            .is_err()
        );
    }

    #[test]
    fn declarer_and_kind_contract_both_constrain_roots() {
        let value = serde_json::json!({"external_content": [{
            "id": "fixture",
            "kind": "tree",
            "locator": {"root": "node_files", "path": "fixture"},
            "mode": "captured",
            "mount": "fixture"
        }]});
        assert!(
            declarations_from_composed(
                &value,
                Some(&contract(&["node_files"], 1)),
                DeclaringAuthority::Project
            )
            .is_err()
        );
    }

    #[test]
    fn pending_pin_tokens_exist_only_in_static_preview() {
        let value = serde_json::json!({"external_content": [{
            "id": "fixture",
            "kind": "tree",
            "locator": {"root": "project_files", "path": "vendor/fixture"},
            "mode": "pinned",
            "digest": "PENDING_FIXTURE_DIGEST",
            "mount": "vendor/fixture"
        }]});
        assert!(
            declarations_from_composed_for_static_preview(
                &value,
                Some(&contract(&["project_files"], 1)),
                DeclaringAuthority::Project
            )
            .unwrap()
            .is_some()
        );
        assert!(
            declarations_from_composed(
                &value,
                Some(&contract(&["project_files"], 1)),
                DeclaringAuthority::Project
            )
            .is_err()
        );
    }
}
