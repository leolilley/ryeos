//! Trusted acquisition recipes for external content that is intentionally not
//! shipped in an installed bundle.
//!
//! The portable recipe says only how to obtain exact bytes and which existing
//! consumer declaration each member supplies. The consumer remains authority
//! for realization ID, file/tree kind, pinned manifest digest, and mount. Node
//! policy separately controls whether and within what limits acquisition may
//! run on this site.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::sections::external_content::ManagedExternalContentActivationPolicy;

pub const MANAGED_ACTIVATION_SCHEMA: &str = "ryeos.external_content_activation.v1";
pub const MANAGED_ACTIVATION_ARCHIVE_FORMAT: &str = "tar_gzip";
const MAX_PORTABLE_ARCHIVES: usize = 8;
const MAX_PORTABLE_MEMBERS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedMemberDisposition {
    Import,
    VerifyOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedComponentStorage {
    Content,
    LargeContent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationMember {
    pub path: String,
    pub disposition: ManagedMemberDisposition,
    pub sha256: String,
    pub maximum_bytes: u64,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationSource {
    pub id: String,
    pub url: String,
    pub archive_format: String,
    pub sha256: String,
    pub maximum_compressed_bytes: u64,
    pub maximum_expanded_bytes: u64,
    pub members: Vec<ManagedActivationMember>,
}

/// One acquisition member mapped to an existing consumer external-content ID.
/// Kind, pinned manifest digest, schema, and mount are deliberately absent:
/// admission derives them from the resolved consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationComponent {
    pub id: String,
    pub source: String,
    pub member: String,
    pub storage: ManagedComponentStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedExternalContentActivation {
    pub schema: String,
    pub consumer_ref: String,
    pub sources: Vec<ManagedActivationSource>,
    pub components: Vec<ManagedActivationComponent>,
}

#[derive(Debug, Clone)]
pub struct ResolvedManagedActivationComponent {
    pub recipe: ManagedActivationComponent,
    pub expected_manifest_hash: String,
    pub expected_manifest_kind: String,
    pub declaration_kind: ryeos_engine::external_content::ExternalContentKind,
}

#[derive(Debug, Clone)]
pub struct ResolvedManagedExternalContentActivation {
    pub activation_ref: String,
    pub activation_program_digest: String,
    pub publisher_fingerprint: String,
    pub document: ManagedExternalContentActivation,
    pub components: Vec<ResolvedManagedActivationComponent>,
}

impl ManagedExternalContentActivation {
    /// Compile the portable signed recipe without consulting this node.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let document: Self = serde_json::from_value(value.clone())
            .context("parse managed external-content acquisition config")?;
        document.validate_portable()?;
        Ok(document)
    }

    pub fn validate_portable(&self) -> anyhow::Result<()> {
        if self.schema != MANAGED_ACTIVATION_SCHEMA {
            bail!("managed external-content activation schema is not current");
        }
        validate_canonical_ref("activation consumer ref", &self.consumer_ref)?;
        if self.sources.is_empty() || self.sources.len() > MAX_PORTABLE_ARCHIVES {
            bail!("managed activation source count exceeds the portable contract");
        }
        if self.components.is_empty()
            || self.components.len()
                > ryeos_state::objects::MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS
        {
            bail!("managed activation component count is outside the supported range");
        }

        let mut source_ids = BTreeSet::new();
        let mut source_members = BTreeMap::<(&str, &str), &ManagedActivationMember>::new();
        let mut imported_members = BTreeSet::new();
        let mut total_members = 0usize;
        for source in &self.sources {
            validate_id("activation source id", &source.id)?;
            if !source_ids.insert(source.id.as_str()) {
                bail!("managed activation repeats a source id");
            }
            validate_portable_source_url(&source.url)?;
            if source.archive_format != MANAGED_ACTIVATION_ARCHIVE_FORMAT {
                bail!("managed activation source archive format is unsupported");
            }
            validate_hash("activation archive digest", &source.sha256)?;
            if source.maximum_compressed_bytes == 0
                || source.maximum_expanded_bytes == 0
                || source.maximum_compressed_bytes > source.maximum_expanded_bytes
                || source.maximum_expanded_bytes
                    > ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES
            {
                bail!("managed activation archive bounds exceed the portable contract");
            }
            if source.members.is_empty() {
                bail!("managed activation source declares no selected members");
            }
            total_members = total_members
                .checked_add(source.members.len())
                .ok_or_else(|| anyhow::anyhow!("activation member ceiling overflow"))?;
            for member in &source.members {
                validate_member_path(&member.path)?;
                validate_hash("activation member digest", &member.sha256)?;
                if member.maximum_bytes == 0
                    || member.maximum_bytes > ryeos_state::objects::MAX_LARGE_CONTENT_FILE_BYTES
                {
                    bail!("managed activation member bound exceeds the portable contract");
                }
                if source_members
                    .insert((source.id.as_str(), member.path.as_str()), member)
                    .is_some()
                {
                    bail!("managed activation repeats a source member");
                }
                if member.disposition == ManagedMemberDisposition::Import {
                    imported_members.insert((source.id.as_str(), member.path.as_str()));
                }
            }
        }
        if total_members > MAX_PORTABLE_MEMBERS {
            bail!("managed activation selected-member count exceeds the portable contract");
        }

        let mut component_ids = BTreeSet::new();
        let mut consumed_imports = BTreeSet::new();
        for component in &self.components {
            validate_id("activation component id", &component.id)?;
            validate_id("activation component source", &component.source)?;
            validate_member_path(&component.member)?;
            if !component_ids.insert(component.id.as_str()) {
                bail!("managed activation repeats a component id");
            }
            let Some(member) =
                source_members.get(&(component.source.as_str(), component.member.as_str()))
            else {
                bail!("activation component names an absent source member");
            };
            if member.disposition != ManagedMemberDisposition::Import {
                bail!("activation component does not name an imported source member");
            }
            if component.storage == ManagedComponentStorage::Content
                && member.maximum_bytes > ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES
            {
                bail!("ordinary-content activation component has a large-content byte bound");
            }
            if !consumed_imports.insert((component.source.as_str(), component.member.as_str())) {
                bail!("an imported activation member is consumed more than once");
            }
        }
        if consumed_imports != imported_members {
            bail!("every imported activation member must map to exactly one component");
        }
        Ok(())
    }

    /// Admit the portable recipe against this node and the already-resolved
    /// consumer. Repeated facts are derived here and retained only as compiled
    /// assertions for the import/bind operation.
    pub fn admit(
        &self,
        policy: &ManagedExternalContentActivationPolicy,
        declarations: &[ryeos_engine::external_content::ExternalContentDeclaration],
        large_content_supported: bool,
    ) -> anyhow::Result<Vec<ResolvedManagedActivationComponent>> {
        self.validate_portable()?;
        policy.validate()?;
        if self.sources.len() > policy.max_archives {
            bail!("managed activation archive count exceeds node policy");
        }
        let mut total_compressed = 0u64;
        let mut total_expanded = 0u64;
        let mut total_members = 0usize;
        for source in &self.sources {
            admit_source_url(&source.url, policy)?;
            if source.maximum_compressed_bytes > policy.max_compressed_bytes
                || source.maximum_expanded_bytes > policy.max_expanded_bytes
            {
                bail!("managed activation archive bounds exceed node policy");
            }
            total_compressed = total_compressed
                .checked_add(source.maximum_compressed_bytes)
                .ok_or_else(|| anyhow::anyhow!("activation compressed byte ceiling overflow"))?;
            total_expanded = total_expanded
                .checked_add(source.maximum_expanded_bytes)
                .ok_or_else(|| anyhow::anyhow!("activation expanded byte ceiling overflow"))?;
            total_members = total_members
                .checked_add(source.members.len())
                .ok_or_else(|| anyhow::anyhow!("activation member ceiling overflow"))?;
            if source
                .members
                .iter()
                .any(|member| member.maximum_bytes > policy.max_member_bytes)
            {
                bail!("managed activation member bound exceeds node policy");
            }
        }
        if total_compressed > policy.max_compressed_bytes
            || total_expanded > policy.max_expanded_bytes
            || total_members > policy.max_members
        {
            bail!("managed activation aggregate bounds exceed node policy");
        }

        let required = declarations
            .iter()
            .filter(|declaration| {
                declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
                    && declaration.locator.is_none()
            })
            .map(|declaration| (declaration.id.as_str(), declaration))
            .collect::<BTreeMap<_, _>>();
        if required.len() != self.components.len() {
            bail!(
                "managed activation must supply every locator-free pinned consumer realization exactly once"
            );
        }
        let mut resolved = Vec::with_capacity(self.components.len());
        for component in &self.components {
            let declaration = required.get(component.id.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "managed activation component {} is not a pinned consumer realization",
                    component.id
                )
            })?;
            if declaration.kind != ryeos_engine::external_content::ExternalContentKind::File {
                bail!("managed activation v1 can supply only consumer file realizations");
            }
            let expected_manifest_hash = declaration
                .digest
                .clone()
                .ok_or_else(|| anyhow::anyhow!("pinned consumer realization has no digest"))?;
            let expected_manifest_kind = match component.storage {
                ManagedComponentStorage::Content => {
                    ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
                }
                ManagedComponentStorage::LargeContent if large_content_supported => {
                    ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                }
                ManagedComponentStorage::LargeContent => {
                    bail!("consumer kind has no signed large-content grant")
                }
            };
            resolved.push(ResolvedManagedActivationComponent {
                recipe: component.clone(),
                expected_manifest_hash,
                expected_manifest_kind: expected_manifest_kind.to_owned(),
                declaration_kind: declaration.kind,
            });
        }
        resolved.sort_by(|left, right| left.recipe.id.cmp(&right.recipe.id));
        Ok(resolved)
    }
}

impl ResolvedManagedExternalContentActivation {
    pub fn component(&self, id: &str) -> anyhow::Result<&ResolvedManagedActivationComponent> {
        self.components
            .iter()
            .find(|component| component.recipe.id == id)
            .ok_or_else(|| anyhow::anyhow!("managed activation component {id} is absent"))
    }

    pub fn source(&self, id: &str) -> anyhow::Result<&ManagedActivationSource> {
        self.document
            .sources
            .iter()
            .find(|source| source.id == id)
            .ok_or_else(|| anyhow::anyhow!("managed activation source {id} is absent"))
    }

    pub fn member(
        &self,
        component: &ResolvedManagedActivationComponent,
    ) -> anyhow::Result<&ManagedActivationMember> {
        self.source(&component.recipe.source)?
            .members
            .iter()
            .find(|member| member.path == component.recipe.member)
            .ok_or_else(|| anyhow::anyhow!("managed activation component member is absent"))
    }
}

pub fn resolve_activation(
    state: &crate::state::AppState,
    activation_ref: &str,
) -> anyhow::Result<ResolvedManagedExternalContentActivation> {
    let policy = state
        .node_config
        .external_content_import_policy
        .as_ref()
        .and_then(|policy| policy.managed_activation.as_ref())
        .ok_or_else(|| anyhow::anyhow!("node has no managed external-content activation policy"))?;
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(activation_ref)
        .map_err(|error| anyhow::anyhow!("invalid activation ref: {error}"))?;
    if canonical.to_string() != activation_ref || canonical.kind != "config" {
        bail!("managed activation requires one canonical config ref");
    }
    let effective = state.engine.with_checked_bundle_generation(|generation| {
        generation.effective_item(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: canonical,
            expected_kind: Some("config".to_owned()),
            project_root: None,
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        })
    })?;
    require_trusted_bundle_item(&effective, "managed activation config")?;
    let publisher_fingerprint = item_publisher(&effective, "managed activation config")?;
    let document = ManagedExternalContentActivation::from_value(&effective.composed_value)?;

    let consumer_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(&document.consumer_ref)
        .map_err(|error| anyhow::anyhow!("invalid activation consumer ref: {error}"))?;
    let consumer = state.engine.with_checked_bundle_generation(|generation| {
        generation.effective_item(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: consumer_ref,
            expected_kind: None,
            project_root: None,
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        })
    })?;
    require_trusted_bundle_item(&consumer, "managed activation consumer")?;
    let consumer_publisher = item_publisher(&consumer, "managed activation consumer")?;
    if consumer_publisher != publisher_fingerprint {
        bail!("managed activation and consumer must share one trusted bundle publisher");
    }
    let external_contract = state
        .engine
        .kinds
        .get(&consumer.kind)
        .and_then(|kind| kind.execution.as_ref())
        .and_then(|execution| execution.external_content.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!("managed activation consumer kind has no external-content contract")
        })?;
    let declarations: Vec<ryeos_engine::external_content::ExternalContentDeclaration> =
        serde_json::from_value(
            consumer
                .composed_value
                .get("external_content")
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("managed activation consumer declares no external content")
                })?,
        )
        .context("parse resolved consumer external-content declarations")?;
    let components = document.admit(
        policy,
        &declarations,
        external_contract.large_content.is_some(),
    )?;
    let activation_program_digest =
        ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "activation": {
                "canonical_ref": effective.canonical_ref,
                "kind": effective.kind,
                "publisher_fingerprint": publisher_fingerprint,
                "trust_class": effective.trust_class,
                "composed_value": effective.composed_value,
            },
            "consumer": {
                "canonical_ref": consumer.canonical_ref,
                "kind": consumer.kind,
                "publisher_fingerprint": consumer_publisher,
                "trust_class": consumer.trust_class,
                "external_content": declarations,
                "large_content_supported": external_contract.large_content.is_some(),
            }
        }))?;
    Ok(ResolvedManagedExternalContentActivation {
        activation_ref: effective.canonical_ref,
        activation_program_digest,
        publisher_fingerprint,
        document,
        components,
    })
}

fn require_trusted_bundle_item(
    item: &ryeos_engine::engine::EffectiveItem,
    label: &str,
) -> anyhow::Result<()> {
    if !item.trusted
        || item.trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle
        || item.source.bundle_root.is_none()
    {
        bail!("{label} must be a trusted installed-bundle item");
    }
    Ok(())
}

fn item_publisher(
    item: &ryeos_engine::engine::EffectiveItem,
    label: &str,
) -> anyhow::Result<String> {
    item.provenance
        .root
        .signer_fingerprint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{label} has no publisher"))
}

fn validate_portable_source_url(value: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(value).context("parse managed activation source URL")?;
    if parsed.as_str() != value
        || parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.port().is_some()
    {
        bail!("managed activation source must be a canonical HTTPS URL");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("managed activation source has no HTTPS host"))?;
    if host != host.to_ascii_lowercase() {
        bail!("managed activation source host is not canonical");
    }
    Ok(())
}

fn admit_source_url(
    value: &str,
    policy: &ManagedExternalContentActivationPolicy,
) -> anyhow::Result<()> {
    validate_portable_source_url(value)?;
    let parsed = url::Url::parse(value)?;
    let host = parsed
        .host_str()
        .expect("portable URL validation checked host");
    if !policy
        .allowed_https_hosts
        .iter()
        .any(|allowed| allowed == host)
    {
        bail!("managed activation source host is not admitted by node policy");
    }
    Ok(())
}

fn validate_member_path(value: &str) -> anyhow::Result<()> {
    ryeos_state::objects::validate_canonical_project_relative_path(value)
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(value) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("{label} is not a canonical sha256 digest");
    }
    Ok(())
}

fn validate_id(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        bail!("{label} is not canonical");
    }
    Ok(())
}

fn validate_canonical_ref(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 512
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        bail!("{label} is empty, unbounded, or non-canonical");
    }
    let parsed = ryeos_engine::canonical_ref::CanonicalRef::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid {label}: {error}"))?;
    if parsed.to_string() != value {
        bail!("{label} is not canonical");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ManagedExternalContentActivationPolicy {
        ManagedExternalContentActivationPolicy {
            allow_online: true,
            allowed_https_hosts: vec!["releases.example.test".to_owned()],
            max_archives: 2,
            max_compressed_bytes: 4096,
            max_expanded_bytes: 8192,
            max_members: 8,
            max_member_bytes: 4096,
            max_concurrent_activations: 1,
            cache_budget_bytes: 16384,
            store_budget_bytes: 32768,
            minimum_free_bytes: 4096,
            max_attempts: 3,
        }
    }

    fn document_value(host: &str) -> Value {
        serde_json::json!({
            "schema":MANAGED_ACTIVATION_SCHEMA,
            "consumer_ref":"worker:fixture/hosted",
            "sources":[{
                "id":"package",
                "url":format!("https://{host}/fixture.tar.gz"),
                "archive_format":MANAGED_ACTIVATION_ARCHIVE_FORMAT,
                "sha256":"a".repeat(64),
                "maximum_compressed_bytes":4096,
                "maximum_expanded_bytes":8192,
                "members":[{
                    "path":"bin/runtime",
                    "disposition":"import",
                    "sha256":"b".repeat(64),
                    "maximum_bytes":4096,
                    "executable":true
                }]
            }],
            "components":[{
                "id":"runtime",
                "source":"package",
                "member":"bin/runtime",
                "storage":"large_content"
            }]
        })
    }

    fn declarations() -> Vec<ryeos_engine::external_content::ExternalContentDeclaration> {
        vec![ryeos_engine::external_content::ExternalContentDeclaration {
            id: "runtime".to_owned(),
            kind: ryeos_engine::external_content::ExternalContentKind::File,
            locator: None,
            mode: ryeos_engine::external_content::ExternalContentMode::Pinned,
            digest: Some("c".repeat(64)),
            exclude: Vec::new(),
            metadata_hint: None,
            mount: "bin/runtime".to_owned(),
        }]
    }

    #[test]
    fn portable_compilation_does_not_depend_on_this_node_host_policy() {
        let document =
            ManagedExternalContentActivation::from_value(&document_value("foreign.example.test"))
                .unwrap();
        assert!(document.admit(&policy(), &declarations(), true).is_err());
    }

    #[test]
    fn admission_derives_consumer_manifest_authority() {
        let document =
            ManagedExternalContentActivation::from_value(&document_value("releases.example.test"))
                .unwrap();
        let resolved = document.admit(&policy(), &declarations(), true).unwrap();
        assert_eq!(resolved[0].expected_manifest_hash, "c".repeat(64));
        assert_eq!(
            resolved[0].expected_manifest_kind,
            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
        );
    }

    #[test]
    fn recipe_cannot_establish_a_second_consumer_realization() {
        let mut value = document_value("releases.example.test");
        value["components"][0]["id"] = Value::String("other".to_owned());
        let document = ManagedExternalContentActivation::from_value(&value).unwrap();
        assert!(document.admit(&policy(), &declarations(), true).is_err());
        value["expected_manifest_hash"] = Value::String("d".repeat(64));
        assert!(ManagedExternalContentActivation::from_value(&value).is_err());
    }
}
