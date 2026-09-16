use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Absolute portable decode bounds. These are not operational admission
/// policy: signed kind schemas and the node's policy generation impose the
/// materially smaller ceilings used by a launch.
pub const MAX_EXECUTION_TARGET_REQUIREMENTS: usize = 64;
pub const MAX_EXECUTION_RESOURCE_COUNT: u16 = 1024;
pub const MAX_EXECUTION_RESOURCE_FACTS: usize = 64;
pub const MAX_EXECUTION_RESOURCE_FACT_KEY_BYTES: usize = 64;
pub const MAX_EXECUTION_RESOURCE_FACT_VALUE_BYTES: usize = 256;

/// Exact signed suitability requirement for the process created by one
/// execution. This is not allocation or descendant authority: it describes
/// only the target on which this process may run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTargetRequirement {
    pub os: String,
    pub arch: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ExecutionResourceRequirement>,
}

/// Authored complexity ceiling used by kind schemas and node policy. It is
/// deliberately independent of resource discovery and allocation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTargetLimits {
    pub max_requirements: u16,
    pub max_resource_count: u16,
    pub max_facts_per_requirement: u16,
}

/// Meaning-blind resource requirement. Fact vocabulary belongs to the
/// admitted observation contract; RyeOS implements only bounded equality and
/// integer-minimum matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionResourceRequirement {
    pub class: String,
    pub count: u16,
    pub allocation: ExecutionResourceAllocation,
    pub access: ExecutionResourceAccess,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exact_facts: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub minimum_facts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExecutionResourceFactValue {
    Text(String),
    Integer(u64),
}

/// Exact launch-selected resource evidence retained in the admitted
/// realization and process owner. It contains only facts used by the signed
/// requirement plus the node observation and device-binding identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionResourceSelection {
    pub stable_id: String,
    pub class: String,
    pub matched_facts: BTreeMap<String, ExecutionResourceFactValue>,
    pub observation_contract_digest: String,
    pub device_binding_digest: String,
    pub access: ExecutionResourceAccess,
    pub enforcement: ExecutionResourceEnforcement,
    pub character_devices: Vec<lillux::CharacterDeviceIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionResourceEnforcement {
    DeploymentVisible,
    CharacterDeviceGrant,
}

impl ExecutionResourceSelection {
    pub const REALIZATION_PROPERTY: &str = "execution_resource_selections";

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_token("selected resource id", &self.stable_id)?;
        validate_token("selected resource class", &self.class)?;
        validate_digest(
            "selected resource observation contract",
            &self.observation_contract_digest,
        )?;
        validate_digest(
            "selected resource device binding",
            &self.device_binding_digest,
        )?;
        if self.matched_facts.len() > MAX_EXECUTION_RESOURCE_FACTS {
            anyhow::bail!("selected resource retains too many matched facts");
        }
        for (key, value) in &self.matched_facts {
            validate_fact_key(key)?;
            if let ExecutionResourceFactValue::Text(value) = value
                && (value.is_empty()
                    || value.len() > MAX_EXECUTION_RESOURCE_FACT_VALUE_BYTES
                    || value.chars().any(char::is_control))
            {
                anyhow::bail!("selected resource fact `{key}` is invalid");
            }
        }
        if self.character_devices.len() > MAX_EXECUTION_RESOURCE_FACTS {
            anyhow::bail!("selected resource retains too many character devices");
        }
        match self.enforcement {
            ExecutionResourceEnforcement::DeploymentVisible
                if self.access != ExecutionResourceAccess::DeploymentVisible =>
            {
                anyhow::bail!("deployment-visible evidence contradicts requested access")
            }
            ExecutionResourceEnforcement::CharacterDeviceGrant
                if self.access != ExecutionResourceAccess::ExecutionRestricted
                    || self.character_devices.is_empty() =>
            {
                anyhow::bail!("character-device evidence contradicts requested access")
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionResourceAllocation {
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionResourceAccess {
    DeploymentVisible,
    ExecutionRestricted,
}

/// Node-owned admission ceiling for constrained execution resources. Device
/// discovery is a separate observation boundary, and allocation state is a
/// separate lifecycle boundary; neither is encoded in this policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionResourceAdmissionPolicy {
    pub limits: ExecutionTargetLimits,
    pub max_total_resource_count: u16,
    pub max_concurrent_exclusive_allocations: u32,
    pub allowed_classes: Vec<String>,
    pub allowed_allocations: Vec<ExecutionResourceAllocation>,
    pub allowed_access: Vec<ExecutionResourceAccess>,
}

/// Inheritable ceiling over resource selection. The initial authority surface
/// mirrors the existing binary network ceiling: a subject may preserve the
/// independently configured node policy or irreversibly deny resource use to
/// itself and every descendant. Exact suitability remains in the subject's
/// target requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionResourceAuthorityCeiling {
    NodePolicy,
    Denied,
}

impl ExecutionResourceAuthorityCeiling {
    pub const REALIZATION_PROPERTY: &str = "execution_resource_authority_ceiling";

    pub fn intersect(self, other: Self) -> Self {
        if matches!(self, Self::Denied) || matches!(other, Self::Denied) {
            Self::Denied
        } else {
            Self::NodePolicy
        }
    }

    pub fn admits(self, target: Option<&ExecutionTargetRequirement>) -> anyhow::Result<()> {
        if matches!(self, Self::Denied) && target.is_some_and(Self::target_requests_resources) {
            anyhow::bail!("execution resource authority is denied for a resource-bearing target");
        }
        Ok(())
    }

    fn target_requests_resources(target: &ExecutionTargetRequirement) -> bool {
        target.requests_resources()
    }
}

impl ExecutionTargetRequirement {
    pub const REALIZATION_PROPERTY: &str = "execution_target_requirement";

    pub fn requests_resources(&self) -> bool {
        !self.resources.is_empty()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_token("target os", &self.os)?;
        validate_token("target arch", &self.arch)?;
        if self.resources.len() > MAX_EXECUTION_TARGET_REQUIREMENTS {
            anyhow::bail!(
                "execution target exceeds the portable requirement bound of {MAX_EXECUTION_TARGET_REQUIREMENTS}"
            );
        }
        let mut classes = BTreeSet::new();
        for resource in &self.resources {
            resource.validate()?;
            if !classes.insert(resource.class.as_str()) {
                anyhow::bail!(
                    "execution target contains duplicate resource class `{}`",
                    resource.class
                );
            }
        }
        Ok(())
    }

    pub fn validate_current_platform(&self) -> anyhow::Result<()> {
        self.validate()?;
        let current = lillux::platform::current_target();
        if self.os != current.os || self.arch != current.arch {
            anyhow::bail!(
                "execution target {}-{} does not admit this {}-{} node",
                self.arch,
                self.os,
                current.arch,
                current.os
            );
        }
        Ok(())
    }

    /// Verify that node-owned selection evidence satisfies this exact signed
    /// requirement. Selection order is irrelevant; resource identity remains
    /// exact and unique.
    pub fn validate_selections(
        &self,
        selections: &[ExecutionResourceSelection],
    ) -> anyhow::Result<()> {
        self.validate_current_platform()?;
        let expected_count = self
            .resources
            .iter()
            .try_fold(0_usize, |total, requirement| {
                total
                    .checked_add(usize::from(requirement.count))
                    .ok_or_else(|| anyhow::anyhow!("execution resource selection count overflow"))
            })?;
        if selections.len() != expected_count {
            anyhow::bail!(
                "execution target requires {expected_count} selected resources but received {}",
                selections.len()
            );
        }
        let mut ids = BTreeSet::new();
        for selection in selections {
            selection.validate()?;
            if !ids.insert(selection.stable_id.as_str()) {
                anyhow::bail!("execution resource selection contains a duplicate stable id");
            }
            let requirement = self
                .resources
                .iter()
                .find(|requirement| requirement.class == selection.class)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "selected resource class `{}` was not requested",
                        selection.class
                    )
                })?;
            if selection.access != requirement.access {
                anyhow::bail!(
                    "selected resource `{}` access contradicts its requirement",
                    selection.stable_id
                );
            }
            for (key, expected) in &requirement.exact_facts {
                if selection.matched_facts.get(key)
                    != Some(&ExecutionResourceFactValue::Text(expected.clone()))
                {
                    anyhow::bail!(
                        "selected resource `{}` does not retain exact fact `{key}`",
                        selection.stable_id
                    );
                }
            }
            for (key, expected) in &requirement.minimum_facts {
                if !matches!(
                    selection.matched_facts.get(key),
                    Some(ExecutionResourceFactValue::Integer(actual)) if actual >= expected
                ) {
                    anyhow::bail!(
                        "selected resource `{}` does not retain minimum fact `{key}`",
                        selection.stable_id
                    );
                }
            }
        }
        for requirement in &self.resources {
            let actual = selections
                .iter()
                .filter(|selection| selection.class == requirement.class)
                .count();
            if actual != usize::from(requirement.count) {
                anyhow::bail!(
                    "execution resource `{}` requires {} selections but received {actual}",
                    requirement.class,
                    requirement.count
                );
            }
        }
        Ok(())
    }

    pub fn validate_against_limits(&self, limits: ExecutionTargetLimits) -> anyhow::Result<()> {
        limits.validate()?;
        self.validate()?;
        if self.resources.len() > usize::from(limits.max_requirements) {
            anyhow::bail!(
                "execution target has {} resource requirements but the ceiling is {}",
                self.resources.len(),
                limits.max_requirements
            );
        }
        for resource in &self.resources {
            if resource.count > limits.max_resource_count {
                anyhow::bail!(
                    "execution resource `{}` requests count {} but the ceiling is {}",
                    resource.class,
                    resource.count,
                    limits.max_resource_count
                );
            }
            let facts = resource
                .exact_facts
                .len()
                .saturating_add(resource.minimum_facts.len());
            if facts > usize::from(limits.max_facts_per_requirement) {
                anyhow::bail!(
                    "execution resource `{}` has {facts} facts but the ceiling is {}",
                    resource.class,
                    limits.max_facts_per_requirement
                );
            }
        }
        Ok(())
    }
}

impl ExecutionTargetLimits {
    pub fn validate(self) -> anyhow::Result<()> {
        if self.max_requirements == 0
            || usize::from(self.max_requirements) > MAX_EXECUTION_TARGET_REQUIREMENTS
        {
            anyhow::bail!(
                "execution target max_requirements must be between 1 and {MAX_EXECUTION_TARGET_REQUIREMENTS}"
            );
        }
        if self.max_resource_count == 0 || self.max_resource_count > MAX_EXECUTION_RESOURCE_COUNT {
            anyhow::bail!(
                "execution target max_resource_count must be between 1 and {MAX_EXECUTION_RESOURCE_COUNT}"
            );
        }
        if self.max_facts_per_requirement == 0
            || usize::from(self.max_facts_per_requirement) > MAX_EXECUTION_RESOURCE_FACTS
        {
            anyhow::bail!(
                "execution target max_facts_per_requirement must be between 1 and {MAX_EXECUTION_RESOURCE_FACTS}"
            );
        }
        Ok(())
    }
}

impl ExecutionResourceAdmissionPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.limits.validate()?;
        if self.max_total_resource_count == 0
            || self.max_total_resource_count > MAX_EXECUTION_RESOURCE_COUNT
        {
            anyhow::bail!(
                "node resource max_total_resource_count must be between 1 and {MAX_EXECUTION_RESOURCE_COUNT}"
            );
        }
        if self.max_concurrent_exclusive_allocations == 0 {
            anyhow::bail!("node resource max_concurrent_exclusive_allocations must be positive");
        }
        validate_sorted_unique("resource class", &self.allowed_classes, |value| {
            validate_token("resource class", value)
        })?;
        if self.allowed_classes.is_empty() {
            anyhow::bail!("enabled node resource policy requires an allowed class");
        }
        validate_sorted_unique("resource allocation", &self.allowed_allocations, |_| Ok(()))?;
        if self.allowed_allocations.is_empty() {
            anyhow::bail!("enabled node resource policy requires an allowed allocation mode");
        }
        validate_sorted_unique("resource access", &self.allowed_access, |_| Ok(()))?;
        if self.allowed_access.is_empty() {
            anyhow::bail!("enabled node resource policy requires an allowed access mode");
        }
        Ok(())
    }

    pub fn admit(&self, target: &ExecutionTargetRequirement) -> anyhow::Result<()> {
        self.validate()?;
        target.validate_against_limits(self.limits)?;
        let mut total = 0u16;
        for requirement in &target.resources {
            if self
                .allowed_classes
                .binary_search(&requirement.class)
                .is_err()
            {
                anyhow::bail!(
                    "node resource policy does not allow class `{}`",
                    requirement.class
                );
            }
            if self
                .allowed_allocations
                .binary_search(&requirement.allocation)
                .is_err()
            {
                anyhow::bail!(
                    "node resource policy does not allow allocation mode {:?}",
                    requirement.allocation
                );
            }
            if self
                .allowed_access
                .binary_search(&requirement.access)
                .is_err()
            {
                anyhow::bail!(
                    "node resource policy does not allow access mode {:?}",
                    requirement.access
                );
            }
            total = total.checked_add(requirement.count).ok_or_else(|| {
                anyhow::anyhow!("execution resource count overflowed node policy accounting")
            })?;
        }
        if total > self.max_total_resource_count {
            anyhow::bail!(
                "execution target requests {total} resources but node policy allows {}",
                self.max_total_resource_count
            );
        }
        Ok(())
    }
}

fn validate_sorted_unique<T, F>(label: &str, values: &[T], mut validate: F) -> anyhow::Result<()>
where
    T: Ord,
    F: FnMut(&T) -> anyhow::Result<()>,
{
    let mut previous: Option<&T> = None;
    for value in values {
        validate(value)?;
        if previous.is_some_and(|candidate| candidate >= value) {
            anyhow::bail!("node {label} allowlist must be sorted and unique");
        }
        previous = Some(value);
    }
    Ok(())
}

impl ExecutionResourceRequirement {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_token("resource class", &self.class)?;
        if self.count == 0 || self.count > MAX_EXECUTION_RESOURCE_COUNT {
            anyhow::bail!(
                "execution resource count must be between 1 and {MAX_EXECUTION_RESOURCE_COUNT}"
            );
        }
        if self
            .exact_facts
            .len()
            .saturating_add(self.minimum_facts.len())
            > MAX_EXECUTION_RESOURCE_FACTS
        {
            anyhow::bail!(
                "execution resource requirement exceeds the portable fact bound of {MAX_EXECUTION_RESOURCE_FACTS}"
            );
        }
        for (key, value) in &self.exact_facts {
            validate_fact_key(key)?;
            if value.is_empty()
                || value.len() > MAX_EXECUTION_RESOURCE_FACT_VALUE_BYTES
                || value.chars().any(char::is_control)
            {
                anyhow::bail!("execution resource exact fact `{key}` is invalid");
            }
            if self.minimum_facts.contains_key(key) {
                anyhow::bail!("execution resource fact `{key}` has two comparison modes");
            }
        }
        for key in self.minimum_facts.keys() {
            validate_fact_key(key)?;
        }
        Ok(())
    }
}

fn validate_token(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > MAX_EXECUTION_RESOURCE_FACT_VALUE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("{label} is not a bounded canonical token");
    }
    Ok(())
}

fn validate_fact_key(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > MAX_EXECUTION_RESOURCE_FACT_KEY_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("execution resource fact key is not canonical");
    }
    Ok(())
}

fn validate_digest(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("{label} is not a canonical sha256 digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ExecutionTargetRequirement {
        ExecutionTargetRequirement {
            os: lillux::platform::current_target().os.to_owned(),
            arch: lillux::platform::current_target().arch.to_owned(),
            resources: vec![ExecutionResourceRequirement {
                class: "accelerator".to_owned(),
                count: 1,
                allocation: ExecutionResourceAllocation::Exclusive,
                access: ExecutionResourceAccess::ExecutionRestricted,
                exact_facts: BTreeMap::from([("vendor".to_owned(), "nvidia".to_owned())]),
                minimum_facts: BTreeMap::from([("memory_bytes".to_owned(), 1)]),
            }],
        }
    }

    #[test]
    fn target_contract_is_strict_and_platform_checked() {
        target().validate_current_platform().unwrap();
        let mut plural = target();
        plural.resources[0].count = 2;
        assert!(plural.validate().is_ok());
        plural.resources[0].count = 0;
        assert!(plural.validate().is_err());
        let mut duplicate = target();
        duplicate.resources.push(duplicate.resources[0].clone());
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn authored_limits_narrow_portable_shape_bounds() {
        let limits = ExecutionTargetLimits {
            max_requirements: 1,
            max_resource_count: 1,
            max_facts_per_requirement: 4,
        };
        target().validate_against_limits(limits).unwrap();
        let mut plural = target();
        plural.resources[0].count = 2;
        assert!(plural.validate_against_limits(limits).is_err());
    }
}
