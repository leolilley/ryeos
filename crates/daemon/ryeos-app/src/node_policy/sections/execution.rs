//! Node-owned execution admission and host-environment authority.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "execution";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExecutionAdmissionPolicy {
    pub schema: u32,
    pub max_live_fanout: u32,
    pub max_private_materialization_copy_bytes: u64,
    pub host_env_passthrough: Vec<String>,
    /// Required-nullable target ceiling for a boot-local workload client.
    /// Project data may request less; absence disables admission completely.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub workload_client: Option<ryeos_runtime::workload_client::WorkloadClientNodePolicy>,
    /// Required-nullable node authority for constrained execution resources.
    /// `null` is deny-all; a populated policy is still only a ceiling and does
    /// not assert that matching devices were observed or allocated.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub resource_authority: Option<NodeExecutionResourcePolicy>,
    /// Required-nullable node ceiling for signed product producer recipes.
    /// `null` denies producer admission; a recipe may only narrow these bounds.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub producer_resource_ceiling:
        Option<ryeos_state::external_content::products::producer_recipe::ProducerResourceBounds>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExecutionResourcePolicy {
    pub admission: ryeos_engine::contracts::ExecutionResourceAdmissionPolicy,
    /// Node-owned portion of every finite occupancy maximum reserved for
    /// termination, reap and terminal meter capture. It is captured into the
    /// exact process identity; project content cannot narrow it away.
    pub cleanup_allowance_ms: u64,
    pub resources: Vec<NodeExecutionResourceDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExecutionResourceDescriptor {
    pub stable_id: String,
    pub class: String,
    pub observation_contract_digest: String,
    /// Reserved for semantic facts emitted by a qualified admitted host
    /// observer. The initial resource adapter accepts only an empty map; node
    /// policy authorship alone is not hardware observation.
    pub facts:
        std::collections::BTreeMap<String, ryeos_engine::contracts::ExecutionResourceFactValue>,
    pub character_devices: Vec<lillux::CharacterDeviceSpec>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub accounting: Option<ryeos_accounting::ResourceAccountingAuthority>,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl NodeExecutionAdmissionPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 4 {
            bail!("node execution policy schema is not current");
        }
        if self.max_live_fanout == 0 {
            bail!("node execution max_live_fanout must be greater than zero");
        }
        if self.max_private_materialization_copy_bytes == 0 {
            bail!("node execution private materialization copy limit must be greater than zero");
        }
        let canonical = self
            .host_env_passthrough
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if canonical != self.host_env_passthrough {
            bail!("node execution host-env allowlist must be sorted and unique");
        }
        ryeos_engine::runtime::HostEnvBindings::from_allowlist(
            self.host_env_passthrough.iter().cloned(),
        )
        .map_err(anyhow::Error::from)
        .context("validate node execution host-env allowlist")?;
        if let Some(policy) = &self.workload_client {
            policy
                .validate()
                .context("validate node workload-client ceiling")?;
        }
        if let Some(policy) = &self.resource_authority {
            policy
                .validate()
                .context("validate node execution resource authority")?;
        }
        if let Some(ceiling) = &self.producer_resource_ceiling {
            ceiling
                .validate()
                .context("validate node producer ceiling")?;
        }
        Ok(())
    }

    pub fn admit_producer_bounds(
        &self,
        bounds: &ryeos_state::external_content::products::producer_recipe::ProducerResourceBounds,
    ) -> anyhow::Result<()> {
        bounds.validate()?;
        let ceiling = self
            .producer_resource_ceiling
            .as_ref()
            .context("node producer admission is disabled")?;
        ceiling.validate()?;
        if bounds.maximum_wall_time_ms > ceiling.maximum_wall_time_ms
            || bounds.maximum_stdout_bytes > ceiling.maximum_stdout_bytes
            || bounds.maximum_stderr_bytes > ceiling.maximum_stderr_bytes
            || bounds.maximum_memory_bytes > ceiling.maximum_memory_bytes
            || bounds.maximum_processes > ceiling.maximum_processes
        {
            bail!("producer resource bounds exceed the node ceiling");
        }
        Ok(())
    }

    pub fn admit_execution_target(
        &self,
        target: Option<&ryeos_engine::contracts::ExecutionTargetRequirement>,
    ) -> anyhow::Result<()> {
        let Some(target) = target else {
            return Ok(());
        };
        target.validate()?;
        if !target.requests_resources() {
            return Ok(());
        }
        let policy = self.resource_authority.as_ref().context(
            "node execution resource authority is disabled for a resource-bearing target",
        )?;
        policy.admission.admit(target)
    }

    pub fn host_env_bindings(&self) -> anyhow::Result<ryeos_engine::runtime::HostEnvBindings> {
        ryeos_engine::runtime::HostEnvBindings::from_allowlist(
            self.host_env_passthrough.iter().cloned(),
        )
        .map_err(anyhow::Error::from)
        .context("resolve node execution host-env bindings")
    }
}

impl NodeExecutionResourcePolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.admission.validate()?;
        if self.cleanup_allowance_ms == 0 || self.cleanup_allowance_ms > 10 * 60 * 1000 {
            bail!("node resource cleanup allowance is outside its operational bound");
        }
        if self.resources.len() > ryeos_engine::contracts::MAX_EXECUTION_TARGET_REQUIREMENTS {
            bail!("node resource catalog exceeds the portable resource bound");
        }
        let mut previous: Option<&str> = None;
        for resource in &self.resources {
            resource.validate()?;
            if let Some(ryeos_accounting::ResourceAccountingAuthority {
                spend:
                    ryeos_accounting::ResourceSpendAuthority::Bounded {
                        maximum_occupancy_milliseconds,
                        ..
                    },
                ..
            }) = resource.accounting.as_ref()
                && self.cleanup_allowance_ms >= *maximum_occupancy_milliseconds
            {
                bail!(
                    "node resource `{}` cleanup allowance consumes its finite occupancy maximum",
                    resource.stable_id
                );
            }
            if previous.is_some_and(|candidate| candidate >= resource.stable_id.as_str()) {
                bail!("node resources must be stable-id sorted and unique");
            }
            if self
                .admission
                .allowed_classes
                .binary_search(&resource.class)
                .is_err()
            {
                bail!(
                    "node resource `{}` uses class `{}` outside its admission allowlist",
                    resource.stable_id,
                    resource.class
                );
            }
            previous = Some(&resource.stable_id);
        }
        Ok(())
    }
}

impl NodeExecutionResourceDescriptor {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_resource_token("node resource stable_id", &self.stable_id)?;
        validate_resource_token("node resource class", &self.class)?;
        validate_digest(
            "node resource observation contract digest",
            &self.observation_contract_digest,
        )?;
        if self.facts.len() > ryeos_engine::contracts::MAX_EXECUTION_RESOURCE_FACTS {
            bail!("node resource `{}` has too many facts", self.stable_id);
        }
        for (name, value) in &self.facts {
            validate_resource_fact_key(name)?;
            if let ryeos_engine::contracts::ExecutionResourceFactValue::Text(value) = value {
                validate_resource_token("node resource fact value", value)?;
            }
        }
        let mut previous: Option<&str> = None;
        for device in &self.character_devices {
            device.validate().map_err(anyhow::Error::msg)?;
            if previous.is_some_and(|candidate| candidate >= device.role.as_str()) {
                bail!(
                    "node resource `{}` character-device roles must be sorted and unique",
                    self.stable_id
                );
            }
            previous = Some(&device.role);
        }
        if let Some(accounting) = &self.accounting {
            accounting.validate().map_err(anyhow::Error::msg)?;
            if accounting.stable_resource_id != self.stable_id
                || accounting.resource_class != self.class
                || accounting.observation_contract_digest.as_str()
                    != self.observation_contract_digest
            {
                bail!(
                    "node resource `{}` accounting authority contradicts its descriptor",
                    self.stable_id
                );
            }
        }
        Ok(())
    }
}

fn validate_digest(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("{label} must be a lowercase sha256 digest");
    }
    Ok(())
}

fn validate_resource_token(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > ryeos_engine::contracts::MAX_EXECUTION_RESOURCE_FACT_VALUE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("{label} is not a bounded canonical token");
    }
    Ok(())
}

fn validate_resource_fact_key(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > ryeos_engine::contracts::MAX_EXECUTION_RESOURCE_FACT_KEY_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("node resource fact is not a bounded canonical key");
    }
    Ok(())
}

impl TypedNodePolicy for NodeExecutionAdmissionPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeExecutionPolicySection;

impl NodePolicySection for NodeExecutionPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: NodeExecutionAdmissionPolicy =
            serde_json::from_value(body.clone()).context("parse node execution policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::external_content::products::producer_recipe::ProducerResourceBounds;

    fn producer_bounds() -> ProducerResourceBounds {
        ProducerResourceBounds {
            maximum_wall_time_ms: 1_000,
            maximum_stdout_bytes: 2_000,
            maximum_stderr_bytes: 3_000,
            maximum_memory_bytes: 4_000,
            maximum_processes: 5,
        }
    }

    fn valid_policy() -> NodeExecutionAdmissionPolicy {
        NodeExecutionAdmissionPolicy {
            schema: 4,
            max_live_fanout: 8,
            max_private_materialization_copy_bytes: 17_179_869_184,
            host_env_passthrough: Vec::new(),
            workload_client: None,
            resource_authority: None,
            producer_resource_ceiling: None,
        }
    }

    #[test]
    fn limits_are_explicit_and_positive() {
        let mut policy = valid_policy();
        assert!(policy.validate().is_ok());
        policy.max_live_fanout = 0;
        assert!(policy.validate().is_err());
        policy = valid_policy();
        policy.max_private_materialization_copy_bytes = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn host_environment_allowlist_is_canonical() {
        let mut policy = valid_policy();
        policy.host_env_passthrough = vec!["PATH".into(), "PATH".into()];
        assert!(policy.validate().is_err());
        policy.host_env_passthrough = vec!["Z_VALUE".into(), "A_VALUE".into()];
        assert!(policy.validate().is_err());
    }

    #[test]
    fn absent_resource_authority_is_deny_all() {
        let policy = valid_policy();
        let target = ryeos_engine::contracts::ExecutionTargetRequirement {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            resources: vec![ryeos_engine::contracts::ExecutionResourceRequirement {
                class: "accelerator".to_owned(),
                count: 1,
                allocation: ryeos_engine::contracts::ExecutionResourceAllocation::Exclusive,
                access: ryeos_engine::contracts::ExecutionResourceAccess::ExecutionRestricted,
                exact_facts: Default::default(),
                minimum_facts: Default::default(),
            }],
        };
        assert!(policy.admit_execution_target(Some(&target)).is_err());
    }

    #[test]
    fn producer_ceiling_is_required_nullable_and_positive() {
        let mut value = serde_json::to_value(valid_policy()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("producer_resource_ceiling");
        assert!(serde_json::from_value::<NodeExecutionAdmissionPolicy>(value).is_err());

        let mut policy = valid_policy();
        assert!(policy.admit_producer_bounds(&producer_bounds()).is_err());
        policy.producer_resource_ceiling = Some(producer_bounds());
        assert!(policy.validate().is_ok());
        policy
            .producer_resource_ceiling
            .as_mut()
            .unwrap()
            .maximum_processes = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn producer_bounds_may_narrow_but_never_widen_any_node_ceiling() {
        let mut policy = valid_policy();
        let ceiling = producer_bounds();
        policy.producer_resource_ceiling = Some(ceiling.clone());
        assert!(policy.admit_producer_bounds(&ceiling).is_ok());

        let mut narrower = ceiling.clone();
        narrower.maximum_wall_time_ms -= 1;
        narrower.maximum_stdout_bytes -= 1;
        narrower.maximum_stderr_bytes -= 1;
        narrower.maximum_memory_bytes -= 1;
        narrower.maximum_processes -= 1;
        assert!(policy.admit_producer_bounds(&narrower).is_ok());

        let mut wider = ceiling.clone();
        wider.maximum_wall_time_ms += 1;
        assert!(policy.admit_producer_bounds(&wider).is_err());
        wider = ceiling.clone();
        wider.maximum_stdout_bytes += 1;
        assert!(policy.admit_producer_bounds(&wider).is_err());
        wider = ceiling.clone();
        wider.maximum_stderr_bytes += 1;
        assert!(policy.admit_producer_bounds(&wider).is_err());
        wider = ceiling.clone();
        wider.maximum_memory_bytes += 1;
        assert!(policy.admit_producer_bounds(&wider).is_err());
        wider = ceiling;
        wider.maximum_processes += 1;
        assert!(policy.admit_producer_bounds(&wider).is_err());
    }

    #[test]
    fn authored_source_profiles_use_the_current_execution_policy_contract() {
        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        for profile in [
            "standard",
            "hosted-workflow",
            "central-host",
            "hosted-node",
            "local-inference",
            "contained-workflow",
            "full",
            "development",
            "bundle-source",
            "release-authority",
        ] {
            let path = repository.join(format!("bundles/.ai/node/init/profiles/{profile}.yaml"));
            let source = std::fs::read_to_string(&path).unwrap();
            let document: serde_json::Value = serde_yaml::from_str(&source).unwrap();
            let policy: NodeExecutionAdmissionPolicy =
                serde_json::from_value(document["policies"]["execution"].clone()).unwrap();
            policy.validate().unwrap();
            assert_eq!(policy.schema, 4, "{}", path.display());
            if [
                "development",
                "full",
                "contained-workflow",
                "release-authority",
            ]
            .contains(&profile)
            {
                assert!(policy.producer_resource_ceiling.is_some(), "{profile}");
            } else {
                assert!(policy.producer_resource_ceiling.is_none(), "{profile}");
            }
        }
    }
}
