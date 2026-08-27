//! Signed declarative managed external-content activation contract.
//!
//! Workload bundles own exact acquisition and component data. The node owns
//! permission, resource ceilings, acquisition, persistence, and publication.
//! This module deliberately accepts no executable hook or caller-supplied host
//! path.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::sections::external_content::ManagedExternalContentActivationPolicy;

pub const MANAGED_ACTIVATION_SCHEMA: &str = "ryeos.external_content_activation.v1";
pub const MANAGED_ACTIVATION_OPERATION: &str = "external_content_activation";
pub const MANAGED_ACTIVATION_ARCHIVE_FORMAT: &str = "tar_gzip";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionMode {
    Online,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedMemberDisposition {
    Import,
    VerifyOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedComponentShape {
    File,
    Tree,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationComponent {
    pub id: String,
    pub source: String,
    pub member: String,
    pub shape: ManagedComponentShape,
    pub storage: ManagedComponentStorage,
    pub expected_manifest_schema: String,
    pub expected_manifest_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedPersistentSessionRequirements {
    pub max_processes: usize,
    pub max_address_space_bytes: u64,
    pub max_cpu_seconds: u64,
    pub max_open_streams: usize,
    pub max_active_streams: usize,
    pub max_active_streams_per_subject: usize,
    pub max_stream_backlog_bytes: u64,
    pub max_total_backlog_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationRequirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_session: Option<ManagedPersistentSessionRequirements>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedExternalContentActivation {
    pub category: String,
    pub schema: String,
    pub consumer_ref: String,
    pub sources: Vec<ManagedActivationSource>,
    pub components: Vec<ManagedActivationComponent>,
    #[serde(default)]
    pub requirements: ManagedActivationRequirements,
}

#[derive(Debug, Clone)]
pub struct ResolvedManagedExternalContentActivation {
    pub activation_ref: String,
    pub activation_program_digest: String,
    pub publisher_fingerprint: String,
    pub document: ManagedExternalContentActivation,
}

impl ManagedExternalContentActivation {
    pub fn from_value(
        value: &Value,
        policy: &ManagedExternalContentActivationPolicy,
    ) -> anyhow::Result<Self> {
        let document: Self = serde_json::from_value(value.clone())
            .context("parse managed external-content activation config")?;
        document.validate(policy)?;
        Ok(document)
    }

    pub fn validate(&self, policy: &ManagedExternalContentActivationPolicy) -> anyhow::Result<()> {
        policy.validate()?;
        if self.schema != MANAGED_ACTIVATION_SCHEMA {
            bail!("managed external-content activation schema is not current");
        }
        validate_identity("activation category", &self.category, 128)?;
        validate_canonical_ref("activation consumer ref", &self.consumer_ref)?;
        if self.sources.is_empty()
            || self.sources.len() > policy.max_archives
            || self.sources.len() > ryeos_state::objects::MAX_EXTERNAL_CONTENT_ACTIVATION_SOURCES
        {
            bail!("managed activation source count exceeds policy");
        }
        if self.components.is_empty()
            || self.components.len()
                > ryeos_state::objects::MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS
        {
            bail!("managed activation component count is outside the supported range");
        }

        let mut source_ids = BTreeSet::new();
        let mut source_members = BTreeMap::<(&str, &str), ManagedMemberDisposition>::new();
        let mut imported_members = BTreeSet::new();
        let mut total_compressed = 0u64;
        let mut total_expanded = 0u64;
        let mut total_members = 0usize;
        for source in &self.sources {
            validate_id("activation source id", &source.id)?;
            if !source_ids.insert(source.id.as_str()) {
                bail!("managed activation repeats a source id");
            }
            validate_source_url(&source.url, policy)?;
            if source.archive_format != MANAGED_ACTIVATION_ARCHIVE_FORMAT {
                bail!("managed activation source archive format is unsupported");
            }
            validate_hash("activation archive digest", &source.sha256)?;
            if source.maximum_compressed_bytes == 0
                || source.maximum_compressed_bytes > policy.max_compressed_bytes
                || source.maximum_expanded_bytes == 0
                || source.maximum_expanded_bytes > policy.max_expanded_bytes
                || source.maximum_compressed_bytes > source.maximum_expanded_bytes
            {
                bail!("managed activation archive bounds exceed node policy");
            }
            total_compressed = total_compressed
                .checked_add(source.maximum_compressed_bytes)
                .ok_or_else(|| anyhow::anyhow!("activation compressed byte ceiling overflow"))?;
            total_expanded = total_expanded
                .checked_add(source.maximum_expanded_bytes)
                .ok_or_else(|| anyhow::anyhow!("activation expanded byte ceiling overflow"))?;
            if source.members.is_empty() {
                bail!("managed activation source declares no members");
            }
            total_members = total_members
                .checked_add(source.members.len())
                .ok_or_else(|| anyhow::anyhow!("activation member ceiling overflow"))?;
            for member in &source.members {
                validate_member_path(&member.path)?;
                validate_hash("activation member digest", &member.sha256)?;
                if member.maximum_bytes == 0 || member.maximum_bytes > policy.max_member_bytes {
                    bail!("managed activation member bound exceeds node policy");
                }
                if source_members
                    .insert(
                        (source.id.as_str(), member.path.as_str()),
                        member.disposition,
                    )
                    .is_some()
                {
                    bail!("managed activation repeats a source member");
                }
                if member.disposition == ManagedMemberDisposition::Import {
                    imported_members.insert((source.id.as_str(), member.path.as_str()));
                }
            }
        }
        if total_compressed > policy.max_compressed_bytes
            || total_expanded > policy.max_expanded_bytes
            || total_members > policy.max_members
        {
            bail!("managed activation aggregate archive bounds exceed node policy");
        }

        let mut component_ids = BTreeSet::new();
        let mut consumed_imports = BTreeSet::new();
        for component in &self.components {
            validate_id("activation component id", &component.id)?;
            validate_id("activation component source", &component.source)?;
            validate_member_path(&component.member)?;
            validate_hash(
                "activation expected manifest hash",
                &component.expected_manifest_hash,
            )?;
            if !component_ids.insert(component.id.as_str()) {
                bail!("managed activation repeats a component id");
            }
            if source_members.get(&(component.source.as_str(), component.member.as_str()))
                != Some(&ManagedMemberDisposition::Import)
            {
                bail!("activation component does not name an imported source member");
            }
            if !consumed_imports.insert((component.source.as_str(), component.member.as_str())) {
                bail!("an imported activation member is consumed more than once");
            }
            match component.storage {
                ManagedComponentStorage::Content
                    if component.expected_manifest_schema
                        != ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA =>
                {
                    bail!("content activation component names the wrong manifest schema")
                }
                ManagedComponentStorage::LargeContent
                    if component.expected_manifest_schema
                        != ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA =>
                {
                    bail!("large-content activation component names the wrong manifest schema")
                }
                _ => {}
            }
            if component.shape != ManagedComponentShape::File {
                bail!("managed activation v1 supports regular-file components only");
            }
        }
        if consumed_imports != imported_members {
            bail!("every imported activation member must map to exactly one component");
        }
        if let Some(requirements) = self.requirements.persistent_session.as_ref() {
            requirements.validate()?;
        }
        Ok(())
    }
}

impl ManagedPersistentSessionRequirements {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_processes == 0
            || self.max_address_space_bytes == 0
            || self.max_cpu_seconds == 0
            || self.max_open_streams == 0
            || self.max_active_streams == 0
            || self.max_active_streams_per_subject == 0
            || self.max_stream_backlog_bytes == 0
            || self.max_total_backlog_bytes == 0
            || self.max_active_streams > self.max_open_streams
            || self.max_active_streams_per_subject > self.max_active_streams
            || self.max_stream_backlog_bytes > self.max_total_backlog_bytes
        {
            bail!("managed activation persistent-session requirements are incoherent");
        }
        Ok(())
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
    if !effective.trusted
        || effective.trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle
        || effective.source.bundle_root.is_none()
    {
        bail!("managed activation config must be a trusted installed-bundle item");
    }
    let publisher_fingerprint = effective
        .provenance
        .root
        .signer_fingerprint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("managed activation config has no publisher"))?;
    let document = ManagedExternalContentActivation::from_value(&effective.composed_value, policy)?;
    let activation_program_digest =
        ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "canonical_ref": effective.canonical_ref,
            "kind": effective.kind,
            "publisher_fingerprint": publisher_fingerprint,
            "trust_class": effective.trust_class,
            "composed_value": effective.composed_value,
        }))?;
    Ok(ResolvedManagedExternalContentActivation {
        activation_ref: effective.canonical_ref,
        activation_program_digest,
        publisher_fingerprint,
        document,
    })
}

fn validate_source_url(
    value: &str,
    policy: &ManagedExternalContentActivationPolicy,
) -> anyhow::Result<()> {
    let parsed = url::Url::parse(value).context("parse managed activation source URL")?;
    if parsed.scheme() != "https"
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
    if host != host.to_ascii_lowercase()
        || !policy
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

fn validate_identity(label: &str, value: &str, maximum: usize) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        bail!("{label} is empty, unbounded, or non-canonical");
    }
    Ok(())
}

fn validate_canonical_ref(label: &str, value: &str) -> anyhow::Result<()> {
    validate_identity(label, value, 512)?;
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
            minimum_free_bytes: 4096,
            max_attempts: 3,
        }
    }

    fn document_value() -> Value {
        serde_json::json!({
            "category":"fixture",
            "schema":MANAGED_ACTIVATION_SCHEMA,
            "consumer_ref":"worker:fixture/hosted",
            "sources":[{
                "id":"package",
                "url":"https://releases.example.test/fixture.tar.gz",
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
                "shape":"file",
                "storage":"large_content",
                "expected_manifest_schema":ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
                "expected_manifest_hash":"c".repeat(64)
            }],
            "requirements":{}
        })
    }

    #[test]
    fn closed_activation_contract_accepts_exact_signed_data() {
        ManagedExternalContentActivation::from_value(&document_value(), &policy()).unwrap();
    }

    #[test]
    fn activation_contract_rejects_unknown_data_and_unadmitted_hosts() {
        let mut unknown = document_value();
        unknown["script"] = Value::String("run-me".to_owned());
        assert!(ManagedExternalContentActivation::from_value(&unknown, &policy()).is_err());

        let mut foreign = document_value();
        foreign["sources"][0]["url"] =
            Value::String("https://foreign.example.test/fixture.tar.gz".to_owned());
        assert!(ManagedExternalContentActivation::from_value(&foreign, &policy()).is_err());
    }

    #[test]
    fn every_imported_member_maps_to_exactly_one_component() {
        let mut orphan = document_value();
        orphan["components"] = Value::Array(Vec::new());
        assert!(ManagedExternalContentActivation::from_value(&orphan, &policy()).is_err());

        let mut duplicate = document_value();
        let repeated = duplicate["components"][0].clone();
        duplicate["components"]
            .as_array_mut()
            .unwrap()
            .push(repeated);
        assert!(ManagedExternalContentActivation::from_value(&duplicate, &policy()).is_err());
    }
}
