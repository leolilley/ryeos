//! Operator grants of host-direct execution.
//!
//! A tool that declares `host_authority: required` asks to run on the host,
//! unconfined, as the daemon user. That is operator-equivalent authority: the
//! process can read the operator key, the node key, the vault and every bundle
//! root. Declaring it grants nothing. A launch runs on the host only when all
//! three owners agree:
//!
//! 1. the publisher's isolation lane offers it (`host_direct:
//!    operator_granted` on the node's host class);
//! 2. the operator granted that exact item here, pinned to its signer and the
//!    exact bytes of its source closure and runtime;
//! 3. the caller is the configured local operator, through an origin the
//!    grant names.
//!
//! The section is operator-owned and changed only by the operator-signed
//! `node.policy-apply` operation. Every profile ships it empty: default deny.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "host_access";
pub const POLICY_SCHEMA: u32 = 1;
/// Most grants one node may hold.
pub const MAX_GRANTS: usize = 64;
/// Most daemon boot-environment names one grant may deliver.
pub const MAX_HOST_ENV: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeHostAccessPolicy {
    pub schema: u32,
    pub grants: Vec<HostAccessGrant>,
}

/// One operator grant of host-direct execution to one exact item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAccessGrant {
    /// Policy-name grammar; unique in the section.
    pub id: String,
    /// Canonical item reference; unique in the section.
    pub item: String,
    /// Signer fingerprint of the item (64 lowercase hex).
    pub signer: String,
    /// Effective source-closure content-manifest digest (64 lowercase hex),
    /// or null for an item with no source closure. Any change needs a new
    /// grant.
    pub source_closure: Option<String>,
    /// The executor chain's runtime: its canonical ref and content hash.
    pub runtime: HostAccessRuntimePin,
    /// Launch roots that may reach this grant. `schedule` is never implied.
    pub origins: Vec<HostAccessOrigin>,
    /// Names delivered from the daemon's own boot environment, not the
    /// caller's shell. Sorted and unique. Nothing else is inherited.
    pub host_env: Vec<String>,
    pub containment: HostAccessContainment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAccessRuntimePin {
    #[serde(rename = "ref")]
    pub runtime_ref: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAccessOrigin {
    /// The operator runs the item itself.
    Direct,
    /// A node of an operator-rooted graph runs it, with the operator still the
    /// acting principal and no delegation on the path.
    GraphChild,
    /// A schedule fire whose principal is the operator.
    Schedule,
}

impl HostAccessOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::GraphChild => "graph_child",
            Self::Schedule => "schedule",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAccessContainment {
    /// The run is placed in a cgroup process scope, so cancel and recovery
    /// reach every descendant. Needs a lane whose process scopes are required.
    ScopeRequired,
    /// Process-group containment only: a descendant that leaves the group
    /// survives cancel.
    GroupAllowed,
}

impl HostAccessContainment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScopeRequired => "scope_required",
            Self::GroupAllowed => "group_allowed",
        }
    }
}

/// The exact subject a host-direct launch presents for admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAccessSubject<'a> {
    pub item: &'a str,
    pub signer: Option<&'a str>,
    pub source_closure: Option<&'a str>,
    /// `None` at admission, before the executor chain is resolved; launch
    /// always presents it.
    pub runtime: Option<(&'a str, &'a str)>,
}

/// A typed refusal. `code` is stable public vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAccessRefusal {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for HostAccessRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HostAccessRefusal {}

impl HostAccessRefusal {
    pub const LANE_UNAVAILABLE: &'static str = "host_access_lane_unavailable";
    pub const NOT_GRANTED: &'static str = "host_access_not_granted";
    pub const GRANT_STALE: &'static str = "host_access_grant_stale";
    pub const PRINCIPAL_REFUSED: &'static str = "host_access_principal_refused";
    pub const DELEGATION_REFUSED: &'static str = "host_access_delegation_refused";
    pub const ORIGIN_NOT_GRANTED: &'static str = "host_access_origin_not_granted";
    pub const CONTAINMENT_UNAVAILABLE: &'static str = "host_access_containment_unavailable";
    pub const SIGNER_REFUSED: &'static str = "host_access_signer_refused";
    pub const PROJECT_REFUSED: &'static str = "host_access_project_refused";

    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl NodeHostAccessPolicy {
    /// The shipped default: no grants.
    pub fn granting_nothing() -> Self {
        Self {
            schema: POLICY_SCHEMA,
            grants: Vec::new(),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != POLICY_SCHEMA {
            bail!("node host_access policy schema is not current");
        }
        if self.grants.len() > MAX_GRANTS {
            bail!("node host_access policy holds more than {MAX_GRANTS} grants");
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut items = std::collections::BTreeSet::new();
        for grant in &self.grants {
            grant.validate()?;
            if !ids.insert(grant.id.as_str()) {
                bail!("node host_access policy grants id `{}` twice", grant.id);
            }
            if !items.insert(grant.item.as_str()) {
                bail!("node host_access policy grants item `{}` twice", grant.item);
            }
        }
        Ok(())
    }

    pub fn grant_for(&self, item: &str) -> Option<&HostAccessGrant> {
        self.grants.iter().find(|grant| grant.item == item)
    }

    pub fn grant_by_id(&self, id: &str) -> Option<&HostAccessGrant> {
        self.grants.iter().find(|grant| grant.id == id)
    }

    /// Refuse grants the node's isolation lane cannot honour. Run as a
    /// cross-section check of every node policy generation.
    pub fn check_against_lane(
        &self,
        lane: &ryeos_engine::isolation::IsolationPolicy,
    ) -> anyhow::Result<()> {
        if self.grants.is_empty() {
            return Ok(());
        }
        if lane.host_direct != ryeos_engine::isolation::IsolationHostDirect::OperatorGranted {
            bail!(
                "{}: node host_access policy holds {} grant(s), but host class `{}` says host_direct: {}",
                HostAccessRefusal::LANE_UNAVAILABLE,
                self.grants.len(),
                lane.host_class,
                lane.host_direct.as_str()
            );
        }
        let scopes = matches!(
            lane.process_scopes,
            ryeos_engine::isolation::IsolationProcessScopePolicy::Required { .. }
        );
        for grant in &self.grants {
            if grant.containment == HostAccessContainment::ScopeRequired && !scopes {
                bail!(
                    "{}: grant `{}` says containment: scope_required, but host class `{}` has process_scopes: unconfigured",
                    HostAccessRefusal::CONTAINMENT_UNAVAILABLE,
                    grant.id,
                    lane.host_class
                );
            }
        }
        Ok(())
    }

    /// Refuse a grant whose item kind cannot declare host authority. Run at
    /// boot once the kind registry is loaded, so a grant that could never
    /// match is an error, not a silent no-op.
    pub fn check_against_kinds(
        &self,
        kinds: &ryeos_engine::kind_registry::KindRegistry,
    ) -> anyhow::Result<()> {
        for grant in &self.grants {
            let item = ryeos_engine::canonical_ref::CanonicalRef::parse(&grant.item)
                .with_context(|| format!("host_access grant `{}` item", grant.id))?;
            let declares = kinds
                .get(&item.kind)
                .and_then(|schema| schema.execution.as_ref())
                .is_some_and(|execution| execution.host_authority.is_some());
            if !declares {
                bail!(
                    "host_access grant `{}` names kind `{}`, whose kind schema has no host_authority projection",
                    grant.id,
                    item.kind
                );
            }
        }
        Ok(())
    }

    /// Admit one host-direct subject against the grants: the exact grant for
    /// its item, with matching signer and digests, reached through an origin
    /// the grant names. Principal and lane checks are the caller's; this
    /// owns only what the operator granted.
    pub fn admit(
        &self,
        subject: &HostAccessSubject<'_>,
        origin: HostAccessOrigin,
    ) -> Result<&HostAccessGrant, HostAccessRefusal> {
        let Some(grant) = self.grant_for(subject.item) else {
            return Err(HostAccessRefusal::new(
                HostAccessRefusal::NOT_GRANTED,
                format!(
                    "no host_access grant names `{}` (signer {}, source closure {}); the operator grants it with `ryeos node host-access grant --project <path> {}`",
                    subject.item,
                    subject.signer.unwrap_or("none"),
                    subject.source_closure.unwrap_or("none"),
                    subject.item
                ),
            ));
        };
        let mut stale = Vec::new();
        if subject.signer != Some(grant.signer.as_str()) {
            stale.push(format!(
                "signer granted {} seen {}",
                grant.signer,
                subject.signer.unwrap_or("none")
            ));
        }
        if subject.source_closure != grant.source_closure.as_deref() {
            stale.push(format!(
                "source closure granted {} seen {}",
                grant.source_closure.as_deref().unwrap_or("none"),
                subject.source_closure.unwrap_or("none")
            ));
        }
        if let Some((runtime_ref, content_hash)) = subject.runtime
            && (runtime_ref != grant.runtime.runtime_ref
                || content_hash != grant.runtime.content_hash)
        {
            stale.push(format!(
                "runtime granted {}@{} seen {runtime_ref}@{content_hash}",
                grant.runtime.runtime_ref, grant.runtime.content_hash
            ));
        }
        if !stale.is_empty() {
            return Err(HostAccessRefusal::new(
                HostAccessRefusal::GRANT_STALE,
                format!(
                    "host_access grant `{}` for `{}` no longer matches its bytes: {}; re-grant it with `ryeos node host-access grant`",
                    grant.id,
                    subject.item,
                    stale.join("; ")
                ),
            ));
        }
        if !grant.origins.contains(&origin) {
            return Err(HostAccessRefusal::new(
                HostAccessRefusal::ORIGIN_NOT_GRANTED,
                format!(
                    "host_access grant `{}` admits origins [{}]; this launch's origin is {}",
                    grant.id,
                    grant
                        .origins
                        .iter()
                        .map(|origin| origin.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    origin.as_str()
                ),
            ));
        }
        Ok(grant)
    }
}

impl HostAccessGrant {
    pub fn validate(&self) -> anyhow::Result<()> {
        crate::node_policy::generation::validate_policy_name("host_access grant", &self.id)?;
        let item = ryeos_engine::canonical_ref::CanonicalRef::parse(&self.item)
            .with_context(|| format!("host_access grant `{}` item is not canonical", self.id))?;
        if item.to_string() != self.item {
            bail!(
                "host_access grant `{}` item `{}` is not in canonical form",
                self.id,
                self.item
            );
        }
        if !canonical_digest(&self.signer) {
            bail!(
                "host_access grant `{}` signer is not a 64-hex fingerprint",
                self.id
            );
        }
        if let Some(digest) = &self.source_closure
            && !canonical_digest(digest)
        {
            bail!(
                "host_access grant `{}` source_closure is not a 64-hex digest",
                self.id
            );
        }
        ryeos_engine::canonical_ref::CanonicalRef::parse(&self.runtime.runtime_ref).with_context(
            || {
                format!(
                    "host_access grant `{}` runtime ref is not canonical",
                    self.id
                )
            },
        )?;
        if !canonical_digest(&self.runtime.content_hash) {
            bail!(
                "host_access grant `{}` runtime content_hash is not a 64-hex digest",
                self.id
            );
        }
        if self.origins.is_empty() {
            bail!("host_access grant `{}` names no origin", self.id);
        }
        if self.origins.windows(2).any(|pair| pair[0] >= pair[1]) {
            bail!(
                "host_access grant `{}` origins must be sorted and unique",
                self.id
            );
        }
        if self.host_env.len() > MAX_HOST_ENV {
            bail!(
                "host_access grant `{}` names more than {MAX_HOST_ENV} host_env names",
                self.id
            );
        }
        if self.host_env.windows(2).any(|pair| pair[0] >= pair[1]) {
            bail!(
                "host_access grant `{}` host_env must be sorted and unique",
                self.id
            );
        }
        for name in &self.host_env {
            let canonical = name
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_')
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
            if !canonical {
                bail!(
                    "host_access grant `{}` host_env name `{name}` is not an environment name",
                    self.id
                );
            }
            if ryeos_engine::runtime::is_reserved_env_name(name) {
                bail!(
                    "host_access grant `{}` host_env name `{name}` is reserved for RyeOS",
                    self.id
                );
            }
        }
        Ok(())
    }
}

/// 64 lowercase hex: the form every RyeOS content digest and signer
/// fingerprint takes.
pub fn canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl TypedNodePolicy for NodeHostAccessPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeHostAccessPolicySection;

impl NodePolicySection for NodeHostAccessPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: NodeHostAccessPolicy =
            serde_json::from_value(body.clone()).context("parse node host_access policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

/// The cross-section check of one complete node generation: grants only on a
/// lane that offers host-direct execution, and scope containment only where
/// the lane has process scopes.
pub fn check_generation(
    policies: &std::collections::BTreeMap<String, Value>,
) -> anyhow::Result<()> {
    let (Some(host_access), Some(isolation)) = (
        policies.get(SECTION_NAME),
        policies.get(super::isolation::SECTION_NAME),
    ) else {
        return Ok(());
    };
    let host_access: NodeHostAccessPolicy =
        serde_json::from_value(host_access.clone()).context("parse node host_access policy")?;
    host_access.validate()?;
    if host_access.grants.is_empty() {
        return Ok(());
    }
    let lane = super::isolation::selected_policy(isolation)?;
    host_access.check_against_lane(&lane)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: format!("/node/policies/{SECTION_NAME}.yaml").into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn grant() -> HostAccessGrant {
        HostAccessGrant {
            id: "guarded-cargo".into(),
            item: "tool:ryeos/development/guarded-cargo".into(),
            signer: "a".repeat(64),
            source_closure: Some("b".repeat(64)),
            runtime: HostAccessRuntimePin {
                runtime_ref: "tool:ryeos/core/runtimes/python/script".into(),
                content_hash: "c".repeat(64),
            },
            origins: vec![HostAccessOrigin::Direct, HostAccessOrigin::GraphChild],
            host_env: vec!["HOME".into(), "PATH".into()],
            containment: HostAccessContainment::ScopeRequired,
        }
    }

    fn policy(grants: Vec<HostAccessGrant>) -> NodeHostAccessPolicy {
        NodeHostAccessPolicy {
            schema: POLICY_SCHEMA,
            grants,
        }
    }

    fn subject(grant: &HostAccessGrant) -> HostAccessSubject<'_> {
        HostAccessSubject {
            item: &grant.item,
            signer: Some(&grant.signer),
            source_closure: grant.source_closure.as_deref(),
            runtime: Some((&grant.runtime.runtime_ref, &grant.runtime.content_hash)),
        }
    }

    #[test]
    fn default_body_grants_nothing() {
        let section = NodeHostAccessPolicySection;
        let body = serde_json::json!({"schema": 1, "grants": []});
        assert!(section.parse(&context(), &body).is_ok());
        let empty = NodeHostAccessPolicy::granting_nothing();
        let granted = grant();
        let refusal = empty
            .admit(&subject(&granted), HostAccessOrigin::Direct)
            .unwrap_err();
        assert_eq!(refusal.code, HostAccessRefusal::NOT_GRANTED);
        assert!(refusal.message.contains(&granted.item));
        assert!(refusal.message.contains("ryeos node host-access grant"));
    }

    #[test]
    fn grant_requires_exact_digests_and_known_origins() {
        let mut bad = grant();
        bad.signer = "short".into();
        assert!(policy(vec![bad]).validate().is_err());
        let mut bad = grant();
        bad.source_closure = Some(format!("sha256:{}", "b".repeat(64)));
        assert!(policy(vec![bad]).validate().is_err());
        let mut bad = grant();
        bad.runtime.content_hash = "C".repeat(64);
        assert!(policy(vec![bad]).validate().is_err());
        let mut bad = grant();
        bad.origins.clear();
        assert!(policy(vec![bad]).validate().is_err());
        let mut bad = grant();
        bad.origins = vec![HostAccessOrigin::GraphChild, HostAccessOrigin::Direct];
        assert!(policy(vec![bad]).validate().is_err());
        let mut value = serde_json::to_value(policy(vec![grant()])).unwrap();
        value["grants"][0]["origins"] = serde_json::json!(["direct", "webhook"]);
        assert!(serde_json::from_value::<NodeHostAccessPolicy>(value).is_err());
        let mut none = grant();
        none.source_closure = None;
        policy(vec![none]).validate().unwrap();
        policy(vec![grant()]).validate().unwrap();
    }

    #[test]
    fn duplicate_grant_id_or_item_is_refused() {
        let mut other = grant();
        other.item = "tool:ryeos/development/featured-build-check".into();
        assert!(
            policy(vec![grant(), other])
                .validate()
                .unwrap_err()
                .to_string()
                .contains("id `guarded-cargo` twice")
        );
        let mut other = grant();
        other.id = "other".into();
        assert!(
            policy(vec![grant(), other])
                .validate()
                .unwrap_err()
                .to_string()
                .contains("twice")
        );
    }

    #[test]
    fn grant_env_names_follow_the_host_env_grammar() {
        for name in ["path", "1PATH", "PATH-X", "RYEOS_TOKEN"] {
            let mut bad = grant();
            bad.host_env = vec![name.into()];
            assert!(policy(vec![bad]).validate().is_err(), "{name}");
        }
        let mut unsorted = grant();
        unsorted.host_env = vec!["PATH".into(), "HOME".into()];
        assert!(policy(vec![unsorted]).validate().is_err());
    }

    #[test]
    fn unknown_fields_and_schema_two_are_refused() {
        let section = NodeHostAccessPolicySection;
        let mut value = serde_json::to_value(policy(vec![grant()])).unwrap();
        value["grants"][0]["project_access"] = "read_only".into();
        assert!(section.parse(&context(), &value).is_err());
        let mut value = serde_json::to_value(policy(vec![grant()])).unwrap();
        value["schema"] = 2.into();
        assert!(section.parse(&context(), &value).is_err());
        assert!(
            section
                .parse(&context(), &serde_json::json!({"schema": 1}))
                .is_err()
        );
    }

    #[test]
    fn admit_names_each_stale_value_and_the_origin() {
        let granted = grant();
        let policy = policy(vec![granted.clone()]);
        assert_eq!(
            policy
                .admit(&subject(&granted), HostAccessOrigin::Direct)
                .unwrap()
                .id,
            "guarded-cargo"
        );
        let edited = "e".repeat(64);
        let mut seen = subject(&granted);
        seen.source_closure = Some(&edited);
        let refusal = policy.admit(&seen, HostAccessOrigin::Direct).unwrap_err();
        assert_eq!(refusal.code, HostAccessRefusal::GRANT_STALE);
        assert!(refusal.message.contains(&edited));
        assert!(refusal.message.contains(&"b".repeat(64)));
        let refusal = policy
            .admit(&subject(&granted), HostAccessOrigin::Schedule)
            .unwrap_err();
        assert_eq!(refusal.code, HostAccessRefusal::ORIGIN_NOT_GRANTED);
        assert!(refusal.message.contains("direct, graph_child"));
        let mut admission = subject(&granted);
        admission.runtime = None;
        assert!(policy.admit(&admission, HostAccessOrigin::Direct).is_ok());
    }

    #[test]
    fn grants_on_an_unavailable_lane_or_without_scopes_are_refused() {
        let mut lane = ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring();
        let granted = policy(vec![grant()]);
        NodeHostAccessPolicy::granting_nothing()
            .check_against_lane(&lane)
            .unwrap();
        assert!(
            granted
                .check_against_lane(&lane)
                .unwrap_err()
                .to_string()
                .contains(HostAccessRefusal::LANE_UNAVAILABLE)
        );
        lane.host_direct = ryeos_engine::isolation::IsolationHostDirect::OperatorGranted;
        assert!(
            granted
                .check_against_lane(&lane)
                .unwrap_err()
                .to_string()
                .contains(HostAccessRefusal::CONTAINMENT_UNAVAILABLE)
        );
        let mut group = grant();
        group.containment = HostAccessContainment::GroupAllowed;
        policy(vec![group]).check_against_lane(&lane).unwrap();
        lane.process_scopes = ryeos_engine::isolation::IsolationProcessScopePolicy::Required {
            control_timeout_ms: 5000,
        };
        granted.check_against_lane(&lane).unwrap();
    }
}
