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


// Unused finite data leaf. These structural bounds grant no admission,
// physical allocation, heap fit, funding or process-lifetime authority.
const ORIGINAL_PHYSICAL_BOUNDS_SCHEMA: &str = "ryeos.original_physical_resource_bounds.v1";
const MAX_ORIGINAL_PHYSICAL_BOUNDS_DOCUMENT_BYTES: usize = 1024;
const MAX_ORIGINAL_PHYSICAL_MEMORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_ORIGINAL_PHYSICAL_PROCESSES: u32 = 256;
const MAX_ORIGINAL_PRIVATE_SOURCE_INODES: u64 = 2_000_000;
const MAX_ORIGINAL_RETAINED_EXPORT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ORIGINAL_SINGLE_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ORIGINAL_CONSTRUCTOR_BUFFER_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginalPhysicalResourceBoundsV1 {
    schema: String,
    maximum_memory_bytes: u64,
    maximum_processes: u32,
    maximum_private_source_bytes: u64,
    maximum_private_source_inodes: u64,
    maximum_retained_export_bytes: u64,
    maximum_single_document_bytes: u64,
    maximum_constructor_buffer_bytes: u64,
}

impl OriginalPhysicalResourceBoundsV1 {
    /// Decode directly into the closed type so duplicate known fields are
    /// rejected before any generic JSON map could collapse them.
    fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_ORIGINAL_PHYSICAL_BOUNDS_DOCUMENT_BYTES,
            "original physical bounds document exceeds decode bound"
        );
        let bounds: Self = serde_json::from_slice(bytes)?;
        bounds.validate()?;
        Ok(bounds)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == ORIGINAL_PHYSICAL_BOUNDS_SCHEMA,
            "unsupported original physical bounds schema"
        );
        for (name, value, ceiling) in [
            ("memory", self.maximum_memory_bytes, MAX_ORIGINAL_PHYSICAL_MEMORY_BYTES),
            ("processes", u64::from(self.maximum_processes), u64::from(MAX_ORIGINAL_PHYSICAL_PROCESSES)),
            ("private source", self.maximum_private_source_bytes, MAX_ORIGINAL_PHYSICAL_MEMORY_BYTES),
            ("private source inodes", self.maximum_private_source_inodes, MAX_ORIGINAL_PRIVATE_SOURCE_INODES),
            ("retained exports", self.maximum_retained_export_bytes, MAX_ORIGINAL_RETAINED_EXPORT_BYTES),
            ("single document", self.maximum_single_document_bytes, MAX_ORIGINAL_SINGLE_DOCUMENT_BYTES),
            ("constructor buffer", self.maximum_constructor_buffer_bytes, MAX_ORIGINAL_CONSTRUCTOR_BUFFER_BYTES),
        ] {
            anyhow::ensure!(value > 0 && value <= ceiling, "original physical {name} exceeds finite structural bounds");
        }
        anyhow::ensure!(
            self.maximum_single_document_bytes <= self.maximum_retained_export_bytes,
            "original physical document exceeds retained export bound"
        );
        let known_bytes = self.maximum_private_source_bytes
            .checked_add(self.maximum_retained_export_bytes)
            .and_then(|bytes| bytes.checked_add(self.maximum_constructor_buffer_bytes))
            .ok_or_else(|| anyhow::anyhow!("original physical byte sum overflow"))?;
        anyhow::ensure!(
            known_bytes < self.maximum_memory_bytes,
            "original physical byte bounds leave no memory headroom"
        );
        // Positive numerical headroom does not prove retained typed heaps,
        // helper/kernel/allocator peaks fit it. That needs later admission.
        Ok(())
    }

    /// Argument roles are data labels only; a future admitted caller must
    /// supply their independently authenticated request/resource/node owners.
    fn intersect(request: &Self, descriptor: &Self, node: &Self) -> anyhow::Result<Self> {
        request.validate()?;
        descriptor.validate()?;
        node.validate()?;
        let effective = Self {
            schema: ORIGINAL_PHYSICAL_BOUNDS_SCHEMA.to_owned(),
            maximum_memory_bytes: request.maximum_memory_bytes.min(descriptor.maximum_memory_bytes).min(node.maximum_memory_bytes),
            maximum_processes: request.maximum_processes.min(descriptor.maximum_processes).min(node.maximum_processes),
            maximum_private_source_bytes: request.maximum_private_source_bytes.min(descriptor.maximum_private_source_bytes).min(node.maximum_private_source_bytes),
            maximum_private_source_inodes: request.maximum_private_source_inodes.min(descriptor.maximum_private_source_inodes).min(node.maximum_private_source_inodes),
            maximum_retained_export_bytes: request.maximum_retained_export_bytes.min(descriptor.maximum_retained_export_bytes).min(node.maximum_retained_export_bytes),
            maximum_single_document_bytes: request.maximum_single_document_bytes.min(descriptor.maximum_single_document_bytes).min(node.maximum_single_document_bytes),
            maximum_constructor_buffer_bytes: request.maximum_constructor_buffer_bytes.min(descriptor.maximum_constructor_buffer_bytes).min(node.maximum_constructor_buffer_bytes),
        };
        effective.validate()?;
        Ok(effective)
    }

    /// The sole RyeOS canonical JSON encoding, explicitly not JCS. Hash the
    /// same returned bytes; no provenance fields or self-referential digest.
    fn canonical_commitment(&self) -> anyhow::Result<(Vec<u8>, String)> {
        self.validate()?;
        let value = serde_json::to_value(self)?;
        let bytes = lillux::canonical_json(&value)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .into_bytes();
        anyhow::ensure!(
            bytes.len() <= MAX_ORIGINAL_PHYSICAL_BOUNDS_DOCUMENT_BYTES,
            "original physical canonical document exceeds decode bound"
        );
        let digest = lillux::sha256_hex(&bytes);
        Ok((bytes, digest))
    }
}

#[cfg(test)]
mod original_physical_bounds_tests {
    use super::*;

    // Pure data controls only; never admitted or verified runtime owners.
    fn bounds() -> OriginalPhysicalResourceBoundsV1 {
        OriginalPhysicalResourceBoundsV1 {
            schema: ORIGINAL_PHYSICAL_BOUNDS_SCHEMA.to_owned(),
            maximum_memory_bytes: 1000,
            maximum_processes: 2,
            maximum_private_source_bytes: 100,
            maximum_private_source_inodes: 10,
            maximum_retained_export_bytes: 80,
            maximum_single_document_bytes: 40,
            maximum_constructor_buffer_bytes: 50,
        }
    }

    #[test]
    fn exact_canonical_wire_and_same_byte_commitment() {
        let actual = bounds();
        let expected = br#"{"maximum_constructor_buffer_bytes":50,"maximum_memory_bytes":1000,"maximum_private_source_bytes":100,"maximum_private_source_inodes":10,"maximum_processes":2,"maximum_retained_export_bytes":80,"maximum_single_document_bytes":40,"schema":"ryeos.original_physical_resource_bounds.v1"}"#;
        let (bytes, digest) = actual.canonical_commitment().unwrap();
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(digest, lillux::sha256_hex(expected));
        assert_eq!(OriginalPhysicalResourceBoundsV1::decode(expected).unwrap(), actual);
        let mut changed = bounds();
        changed.maximum_private_source_inodes += 1;
        let (changed_bytes, changed_digest) = changed.canonical_commitment().unwrap();
        assert_ne!(changed_bytes, bytes);
        assert_ne!(changed_digest, digest);
    }

    #[test]
    fn conflicting_component_minima_and_permutations_preserve_constraints() {
        let mut request = bounds();
        request.maximum_memory_bytes = 500;
        request.maximum_processes = 1;
        request.maximum_constructor_buffer_bytes = 20;
        let mut descriptor = bounds();
        descriptor.maximum_private_source_bytes = 90;
        descriptor.maximum_private_source_inodes = 5;
        descriptor.maximum_single_document_bytes = 30;
        let mut node = bounds();
        node.maximum_retained_export_bytes = 60;
        let mut expected = bounds();
        expected.maximum_memory_bytes = 500;
        expected.maximum_processes = 1;
        expected.maximum_constructor_buffer_bytes = 20;
        expected.maximum_private_source_bytes = 90;
        expected.maximum_private_source_inodes = 5;
        expected.maximum_single_document_bytes = 30;
        expected.maximum_retained_export_bytes = 60;
        let expected_commitment = expected.canonical_commitment().unwrap();
        for (r, d, c) in [
            (&request, &descriptor, &node), (&request, &node, &descriptor),
            (&descriptor, &request, &node), (&descriptor, &node, &request),
            (&node, &request, &descriptor), (&node, &descriptor, &request),
        ] {
            let effective = OriginalPhysicalResourceBoundsV1::intersect(r, d, c).unwrap();
            assert_eq!(effective, expected);
            assert_eq!(effective.canonical_commitment().unwrap(), expected_commitment);
        }
    }

    #[test]
    fn invalid_inputs_refuse_before_component_clipping_in_every_role() {
        let valid = bounds();
        let mut invalid = bounds();
        invalid.maximum_processes = MAX_ORIGINAL_PHYSICAL_PROCESSES + 1;
        for (r, d, c) in [
            (&invalid, &valid, &valid), (&valid, &invalid, &valid),
            (&valid, &valid, &invalid),
        ] {
            let error = OriginalPhysicalResourceBoundsV1::intersect(r, d, c).unwrap_err();
            assert!(error.to_string().contains("processes exceeds finite structural bounds"));
        }
        assert!(invalid.canonical_commitment().is_err());
    }

    #[test]
    fn closed_typed_wire_refuses_malformed_values_without_a_map_decoder() {
        let wire = String::from_utf8(bounds().canonical_commitment().unwrap().0).unwrap();
        let field = "\"maximum_processes\":2";
        for malformed in [
            wire.replace(field, "\"maximum_processes\":null"),
            wire.replace(field, "\"maximum_processes\":-1"),
            wire.replace(field, "\"maximum_processes\":1.0"),
            wire.replace(field, "\"maximum_processes\":\"2\""),
            wire.replace(field, "\"maximum_processes\":4294967296"),
            wire.replace(field, "\"maximum_processes\":2,\"maximum_processes\":2"),
            wire.replace("\"maximum_processes\":2,", ""),
            wire.replace(field, "\"maximum_processes\":2,\"unknown\":1"),
            wire.replace(ORIGINAL_PHYSICAL_BOUNDS_SCHEMA, "ryeos.original_physical_resource_bounds.v2"),
            wire.replace("\"maximum_memory_bytes\":1000", "\"maximum_memory_bytes\":18446744073709551616"),
        ] {
            assert!(OriginalPhysicalResourceBoundsV1::decode(malformed.as_bytes()).is_err(), "{malformed}");
        }
        assert!(OriginalPhysicalResourceBoundsV1::decode(b"").is_err());
        assert!(OriginalPhysicalResourceBoundsV1::decode(b"{}").is_err());
        let mut padded = wire.clone();
        padded.extend(std::iter::repeat_n(' ', MAX_ORIGINAL_PHYSICAL_BOUNDS_DOCUMENT_BYTES - wire.len()));
        assert_eq!(OriginalPhysicalResourceBoundsV1::decode(padded.as_bytes()).unwrap(), bounds());
        padded.push(' ');
        let error = OriginalPhysicalResourceBoundsV1::decode(padded.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("exceeds decode bound"));
    }

    #[test]
    fn all_numeric_fields_refuse_zero_and_structural_maximum_plus_one() {
        let wire = String::from_utf8(bounds().canonical_commitment().unwrap().0).unwrap();
        for (key, old, maximum) in [
            ("maximum_memory_bytes", 1000, MAX_ORIGINAL_PHYSICAL_MEMORY_BYTES),
            ("maximum_processes", 2, u64::from(MAX_ORIGINAL_PHYSICAL_PROCESSES)),
            ("maximum_private_source_bytes", 100, MAX_ORIGINAL_PHYSICAL_MEMORY_BYTES),
            ("maximum_private_source_inodes", 10, MAX_ORIGINAL_PRIVATE_SOURCE_INODES),
            ("maximum_retained_export_bytes", 80, MAX_ORIGINAL_RETAINED_EXPORT_BYTES),
            ("maximum_single_document_bytes", 40, MAX_ORIGINAL_SINGLE_DOCUMENT_BYTES),
            ("maximum_constructor_buffer_bytes", 50, MAX_ORIGINAL_CONSTRUCTOR_BUFFER_BYTES),
        ] {
            for invalid in [0, maximum + 1] {
                let changed = wire.replace(&format!("\"{key}\":{old}"), &format!("\"{key}\":{invalid}"));
                let error = OriginalPhysicalResourceBoundsV1::decode(changed.as_bytes()).unwrap_err();
                assert!(error.to_string().contains("finite structural bounds"), "{key}: {error}");
            }
        }
    }

    #[test]
    fn inclusive_reachable_maxima_and_strict_relationship_boundaries() {
        let mut maximum = bounds();
        maximum.maximum_memory_bytes = MAX_ORIGINAL_PHYSICAL_MEMORY_BYTES;
        maximum.maximum_processes = MAX_ORIGINAL_PHYSICAL_PROCESSES;
        maximum.maximum_private_source_inodes = MAX_ORIGINAL_PRIVATE_SOURCE_INODES;
        maximum.maximum_retained_export_bytes = MAX_ORIGINAL_RETAINED_EXPORT_BYTES;
        maximum.maximum_single_document_bytes = MAX_ORIGINAL_SINGLE_DOCUMENT_BYTES;
        maximum.maximum_constructor_buffer_bytes = MAX_ORIGINAL_CONSTRUCTOR_BUFFER_BYTES;
        maximum.validate().unwrap();
        let wire = maximum.canonical_commitment().unwrap().0;
        assert_eq!(OriginalPhysicalResourceBoundsV1::decode(&wire).unwrap(), maximum);
        // Source's absolute ceiling cannot be reached with positive other
        // bounds and memory<=16GiB. Test its greatest feasible value instead.
        maximum.maximum_retained_export_bytes = 1;
        maximum.maximum_single_document_bytes = 1;
        maximum.maximum_constructor_buffer_bytes = 1;
        maximum.maximum_private_source_bytes = MAX_ORIGINAL_PHYSICAL_MEMORY_BYTES - 3;
        maximum.validate().unwrap();
        maximum.maximum_private_source_bytes += 1;
        assert!(maximum.validate().unwrap_err().to_string().contains("no memory headroom"));
        let mut relationship = bounds();
        relationship.maximum_single_document_bytes = relationship.maximum_retained_export_bytes;
        relationship.validate().unwrap();
        relationship.maximum_single_document_bytes += 1;
        assert!(relationship.validate().unwrap_err().to_string().contains("document exceeds retained export"));
        let mut sum = bounds();
        sum.maximum_memory_bytes = 230;
        assert!(sum.validate().unwrap_err().to_string().contains("no memory headroom"));
        sum.maximum_memory_bytes += 1;
        sum.validate().unwrap();
    }
}


// Bounded request DATA decoder. It supplies neither signed-request provenance nor
// original admission, physical coverage, allocation or funding authority.
const ORIGINAL_TARGET_REQUEST_SCHEMA: &str = "ryeos.execution_target.v1";
const MAX_ORIGINAL_TARGET_RAW_BYTES: usize = 16 * 1024 * 1024;
const MAX_ORIGINAL_TARGET_RAW_DEPTH: usize = 64;
const MAX_ORIGINAL_TARGET_RAW_VALUES: usize = 16_384;
const MAX_ORIGINAL_TARGET_RAW_MEMBERS: usize = 128;
// Conservative canonical semantic envelope: 64*64*(6*(64+256)+8)
// +64*2048+2*(6*256+16)+512+1024 = 8,032,800 <16MiB. This is
// a formatting/decode bound, not a serde or parent-daemon heap bound.

#[derive(Debug)]
enum DecodedOriginalTargetRequest {
    LegacyOrdinary(ExecutionTargetRequirement),
    CurrentV1(CurrentOriginalTargetRequestV1),
}

#[derive(Debug, Clone)]
struct CurrentOriginalTargetRequestV1 {
    schema: String,
    ordinary: ExecutionTargetRequirement,
    original_physical_bounds: Option<OriginalPhysicalResourceBoundsV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentOriginalTargetRequestWireV1 {
    schema: String,
    os: String,
    arch: String,
    resources: Vec<ExecutionResourceRequirement>,
    #[serde(deserialize_with = "decode_required_original_physical_request")]
    original_physical_bounds: Option<OriginalPhysicalResourceBoundsV1>,
}

fn decode_required_original_physical_request<'de, D>(
    deserializer: D,
) -> Result<Option<OriginalPhysicalResourceBoundsV1>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<OriginalPhysicalResourceBoundsV1>::deserialize(deserializer)
}

#[derive(Serialize)]
struct CurrentOriginalTargetRequestSerialization<'a> {
    schema: &'a str,
    os: &'a str,
    arch: &'a str,
    resources: &'a [ExecutionResourceRequirement],
    original_physical_bounds: &'a Option<OriginalPhysicalResourceBoundsV1>,
}

impl DecodedOriginalTargetRequest {
    fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        use serde::de::DeserializeSeed as _;
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_ORIGINAL_TARGET_RAW_BYTES,
            "original target raw document exceeds decode bound"
        );
        // Visit all raw object keys before a BTreeMap can overwrite facts.
        // The same borrowed input is used throughout; no Value-tree preparse.
        let mut scan = serde_json::Deserializer::from_slice(bytes);
        let mut budget = RawOriginalTargetScanBudget { values: 0, output: None };
        RawOriginalTargetScanSeed { budget: &mut budget, depth: 1, comma: false }
            .deserialize(&mut scan)?;
        scan.end()?;
        let presence: RawOriginalTargetPresence = serde_json::from_slice(bytes)?;
        match presence.schema {
            RawOriginalTargetSchema::Absent => {
                anyhow::ensure!(
                    !presence.physical_request_present,
                    "unversioned target cannot carry original physical request"
                );
                let ordinary: ExecutionTargetRequirement = serde_json::from_slice(bytes)?;
                ordinary.validate()?;
                Ok(Self::LegacyOrdinary(ordinary))
            }
            RawOriginalTargetSchema::Null => {
                anyhow::bail!("original target schema is explicitly null")
            }
            RawOriginalTargetSchema::Text(schema) => {
                anyhow::ensure!(
                    schema == ORIGINAL_TARGET_REQUEST_SCHEMA,
                    "unsupported original target request schema"
                );
                // Authoritative absence gate before any required-nullable
                // typed parse. Missing is never repaired to an ordinary null.
                anyhow::ensure!(
                    presence.physical_request_present,
                    "current target is missing original physical request field"
                );
                let wire: CurrentOriginalTargetRequestWireV1 = serde_json::from_slice(bytes)?;
                let ordinary = ExecutionTargetRequirement {
                    os: wire.os,
                    arch: wire.arch,
                    resources: wire.resources,
                };
                ordinary.validate()?;
                if let Some(request) = wire.original_physical_bounds.as_ref() {
                    request.validate()?;
                }
                Ok(Self::CurrentV1(CurrentOriginalTargetRequestV1 {
                    schema: wire.schema,
                    ordinary,
                    original_physical_bounds: wire.original_physical_bounds,
                }))
            }
        }
    }

    fn require_original_physical_request(&self) -> anyhow::Result<&OriginalPhysicalResourceBoundsV1> {
        match self {
            Self::LegacyOrdinary(_) => anyhow::bail!("legacy ordinary target has no original physical request"),
            Self::CurrentV1(current) => {
                anyhow::ensure!(current.schema == ORIGINAL_TARGET_REQUEST_SCHEMA, "unsupported original target request schema");
                current.ordinary.validate()?;
                let request = current.original_physical_bounds.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("current ordinary target has null original physical request"))?;
                request.validate()?;
                Ok(request)
            }
        }
    }

    fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let value = match self {
            Self::LegacyOrdinary(ordinary) => {
                ordinary.validate()?;
                serde_json::to_value(ordinary)?
            }
            Self::CurrentV1(current) => {
                anyhow::ensure!(current.schema == ORIGINAL_TARGET_REQUEST_SCHEMA, "unsupported original target request schema");
                current.ordinary.validate()?;
                if let Some(request) = current.original_physical_bounds.as_ref() {
                    request.validate()?;
                }
                serde_json::to_value(CurrentOriginalTargetRequestSerialization {
                    schema: &current.schema,
                    os: &current.ordinary.os,
                    arch: &current.ordinary.arch,
                    resources: &current.ordinary.resources,
                    original_physical_bounds: &current.original_physical_bounds,
                })?
            }
        };
        let bytes = lillux::canonical_json(&value)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .into_bytes();
        anyhow::ensure!(bytes.len() <= MAX_ORIGINAL_TARGET_RAW_BYTES, "original target canonical bytes exceed decode bound");
        Ok(bytes)
    }
}

// A capped buffer is data storage, not a parent-heap/physical guarantee.
struct OriginalTargetWriter(Vec<u8>);
impl std::io::Write for OriginalTargetWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_ORIGINAL_TARGET_RAW_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("original target encoded bytes exceed bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
struct RawOriginalTargetScanBudget {
    values: usize,
    output: Option<OriginalTargetWriter>,
}
impl RawOriginalTargetScanBudget {
    fn token<E: serde::de::Error>(&mut self, bytes: &[u8]) -> Result<(), E> {
        use std::io::Write as _;
        if let Some(writer) = self.output.as_mut() {
            writer.write_all(bytes).map_err(E::custom)?;
        }
        Ok(())
    }
    fn scalar<E: serde::de::Error, T: serde::Serialize + ?Sized>(&mut self, value: &T) -> Result<(), E> {
        if let Some(writer) = self.output.as_mut() {
            serde_json::to_writer(writer, value).map_err(E::custom)?;
        }
        Ok(())
    }
}
struct RawOriginalTargetScanSeed<'a> {
    budget: &'a mut RawOriginalTargetScanBudget,
    depth: usize,
    comma: bool,
}
impl<'de> serde::de::DeserializeSeed<'de> for RawOriginalTargetScanSeed<'_> {
    type Value = ();
    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where D: serde::Deserializer<'de> {
        use serde::de::Error as _;
        if self.depth > MAX_ORIGINAL_TARGET_RAW_DEPTH {
            return Err(D::Error::custom("original target raw depth exceeds bound"));
        }
        self.budget.values = self.budget.values.checked_add(1)
            .ok_or_else(|| D::Error::custom("original target raw value count overflow"))?;
        if self.budget.values > MAX_ORIGINAL_TARGET_RAW_VALUES {
            return Err(D::Error::custom("original target raw value count exceeds bound"));
        }
        if self.comma { self.budget.token::<D::Error>(b",")?; }
        deserializer.deserialize_any(RawOriginalTargetScanVisitor { budget: self.budget, depth: self.depth })
    }
}
struct RawOriginalTargetScanVisitor<'a> {
    budget: &'a mut RawOriginalTargetScanBudget,
    depth: usize,
}
impl<'de> serde::de::Visitor<'de> for RawOriginalTargetScanVisitor<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded original target JSON data without duplicate object keys")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> { self.budget.token(b"null") }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<(), E> { self.budget.scalar(&v) }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<(), E> { self.budget.scalar(&v) }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<(), E> { self.budget.scalar(&v) }
    fn visit_i128<E: serde::de::Error>(self, v: i128) -> Result<(), E> {
        if let Ok(signed) = i64::try_from(v) { self.budget.scalar(&signed) }
        else { let unsigned = u64::try_from(v).map_err(E::custom)?; self.budget.scalar(&unsigned) }
    }
    fn visit_u128<E: serde::de::Error>(self, v: u128) -> Result<(), E> {
        let v = u64::try_from(v).map_err(E::custom)?; self.budget.scalar(&v)
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<(), E> {
        if !v.is_finite() { return Err(E::custom("nonfinite target scalar")); }
        self.budget.scalar(&v)
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<(), E> {
        if v.len() > MAX_ORIGINAL_TARGET_RAW_BYTES { return Err(E::custom("target string exceeds bound")); }
        self.budget.scalar(v)
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<(), E> { self.visit_str(&v) }
    fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error> where A: serde::de::SeqAccess<'de> {
        use serde::de::Error as _;
        let depth = self.depth.checked_add(1).ok_or_else(|| A::Error::custom("target depth overflow"))?;
        self.budget.token::<A::Error>(b"[")?;
        let mut comma = false;
        while seq.next_element_seed(RawOriginalTargetScanSeed { budget: &mut *self.budget, depth, comma })?.is_some() {
            comma = true;
        }
        self.budget.token(b"]")
    }
    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error> where A: serde::de::MapAccess<'de> {
        use serde::de::Error as _;
        let depth = self.depth.checked_add(1).ok_or_else(|| A::Error::custom("target depth overflow"))?;
        self.budget.token::<A::Error>(b"{")?;
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if keys.len() >= MAX_ORIGINAL_TARGET_RAW_MEMBERS || key.len() > MAX_ORIGINAL_TARGET_RAW_BYTES {
                return Err(A::Error::custom("original target raw object member count exceeds bound"));
            }
            if keys.contains(&key) { return Err(A::Error::custom("original target raw duplicate object key")); }
            if !keys.is_empty() { self.budget.token::<A::Error>(b",")?; }
            self.budget.scalar::<A::Error, _>(&key)?;
            self.budget.token::<A::Error>(b":")?;
            keys.insert(key);
            map.next_value_seed(RawOriginalTargetScanSeed { budget: &mut *self.budget, depth, comma: false })?;
        }
        self.budget.token(b"}")
    }
}

/// Validated current request DATA. Deserialization conveys no launch authority.
#[derive(Debug, Clone)]
pub struct CurrentExecutionTargetRequest(CurrentOriginalTargetRequestV1);
impl CurrentExecutionTargetRequest {
    pub fn ordinary(&self) -> &ExecutionTargetRequirement { &self.0.ordinary }
    pub fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let value = serde_json::to_value(self)?;
        let bytes = lillux::canonical_json(&value)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?.into_bytes();
        anyhow::ensure!(bytes.len() <= MAX_ORIGINAL_TARGET_RAW_BYTES,
            "current target canonical bytes exceed decode bound");
        Ok(bytes)
    }
}
impl serde::Serialize for CurrentExecutionTargetRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CurrentOriginalTargetRequestSerialization {
            schema: &self.0.schema, os: &self.0.ordinary.os, arch: &self.0.ordinary.arch,
            resources: &self.0.ordinary.resources, original_physical_bounds: &self.0.original_physical_bounds,
        }.serialize(serializer)
    }
}
fn transcode_original_target<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    use serde::de::DeserializeSeed as _;
    let mut budget = RawOriginalTargetScanBudget { values: 0, output: Some(OriginalTargetWriter(Vec::new())) };
    RawOriginalTargetScanSeed { budget: &mut budget, depth: 1, comma: false }.deserialize(deserializer)?;
    Ok(budget.output.expect("emission mode retained").0)
}
impl<'de> serde::Deserialize<'de> for CurrentExecutionTargetRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let bytes = transcode_original_target(deserializer)?;
        match DecodedOriginalTargetRequest::decode(&bytes).map_err(D::Error::custom)? {
            DecodedOriginalTargetRequest::CurrentV1(v) => Ok(Self(v)),
            DecodedOriginalTargetRequest::LegacyOrdinary(_) => Err(D::Error::custom("current wrapper refuses ordinary target")),
        }
    }
}
// Borrowed composed input still has upstream allocation/duplicate limitations.
pub(crate) fn project_current_target_data(value: &serde_json::Value)
    -> anyhow::Result<(ExecutionTargetRequirement, Option<CurrentExecutionTargetRequest>)> {
    let bytes = transcode_original_target(value)?;
    match DecodedOriginalTargetRequest::decode(&bytes)? {
        DecodedOriginalTargetRequest::LegacyOrdinary(v) => Ok((v, None)),
        DecodedOriginalTargetRequest::CurrentV1(v) => Ok((v.ordinary.clone(), Some(CurrentExecutionTargetRequest(v)))),
    }
}

#[derive(Debug)]
enum RawOriginalTargetSchema {
    Absent,
    Null,
    Text(String),
}

impl<'de> Deserialize<'de> for RawOriginalTargetSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SchemaVisitor;
        impl<'de> serde::de::Visitor<'de> for SchemaVisitor {
            type Value = RawOriginalTargetSchema;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an explicitly present target schema string or null")
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(RawOriginalTargetSchema::Null)
            }
            fn visit_str<E>(self, text: &str) -> Result<Self::Value, E> {
                Ok(RawOriginalTargetSchema::Text(text.to_owned()))
            }
            fn visit_string<E>(self, text: String) -> Result<Self::Value, E> {
                Ok(RawOriginalTargetSchema::Text(text))
            }
        }
        deserializer.deserialize_any(SchemaVisitor)
    }
}

struct RawOriginalTargetPresence {
    schema: RawOriginalTargetSchema,
    physical_request_present: bool,
}

impl<'de> Deserialize<'de> for RawOriginalTargetPresence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct PresenceVisitor;
        impl<'de> serde::de::Visitor<'de> for PresenceVisitor {
            type Value = RawOriginalTargetPresence;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an original target object with explicit schema/request presence")
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut presence = RawOriginalTargetPresence {
                    schema: RawOriginalTargetSchema::Absent,
                    physical_request_present: false,
                };
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "schema" => presence.schema = map.next_value()?,
                        "original_physical_bounds" => {
                            presence.physical_request_present = true;
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                        _ => { let _: serde::de::IgnoredAny = map.next_value()?; }
                    }
                }
                Ok(presence)
            }
        }
        deserializer.deserialize_map(PresenceVisitor)
    }
}


#[cfg(test)]
mod original_target_decoder_tests {
    use super::*;

    const LEGACY: &[u8] = br#"{"arch":"x86_64","os":"linux","resources":[]}"#;
    // The existing ordinary serializer omits an empty resources vector.
    const LEGACY_CANONICAL: &[u8] = br#"{"arch":"x86_64","os":"linux"}"#;
    const CURRENT_NULL: &[u8] = br#"{"arch":"x86_64","original_physical_bounds":null,"os":"linux","resources":[],"schema":"ryeos.execution_target.v1"}"#;

    fn request() -> OriginalPhysicalResourceBoundsV1 {
        OriginalPhysicalResourceBoundsV1 {
            schema: ORIGINAL_PHYSICAL_BOUNDS_SCHEMA.to_owned(),
            maximum_memory_bytes: 1000,
            maximum_processes: 2,
            maximum_private_source_bytes: 100,
            maximum_private_source_inodes: 10,
            maximum_retained_export_bytes: 80,
            maximum_single_document_bytes: 40,
            maximum_constructor_buffer_bytes: 50,
        }
    }

    fn current_request_wire() -> Vec<u8> {
        let (bytes, _) = request().canonical_commitment().unwrap();
        String::from_utf8(CURRENT_NULL.to_vec()).unwrap()
            .replace("\"original_physical_bounds\":null", &format!(
                "\"original_physical_bounds\":{}", String::from_utf8(bytes).unwrap(),
            )).into_bytes()
    }

    // Raw scan guard fixtures are JSON DATA only, not valid target/owner proof.
    fn scan_data(bytes: &[u8]) -> anyhow::Result<()> {
        use serde::de::DeserializeSeed as _;
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let mut budget = RawOriginalTargetScanBudget { values: 0, output: None };
        RawOriginalTargetScanSeed { budget: &mut budget, depth: 1, comma: false }
            .deserialize(&mut deserializer)?;
        deserializer.end()?;
        Ok(())
    }

    #[test]
    fn legacy_and_explicit_current_null_preserve_ordinary_data_only() {
        let legacy = DecodedOriginalTargetRequest::decode(LEGACY).unwrap();
        assert!(matches!(&legacy, DecodedOriginalTargetRequest::LegacyOrdinary(_)));
        assert_eq!(legacy.canonical_bytes().unwrap(), LEGACY_CANONICAL);
        let canonical_legacy = DecodedOriginalTargetRequest::decode(LEGACY_CANONICAL).unwrap();
        assert!(matches!(&canonical_legacy, DecodedOriginalTargetRequest::LegacyOrdinary(_)));
        assert_eq!(canonical_legacy.canonical_bytes().unwrap(), LEGACY_CANONICAL);
        assert!(legacy.require_original_physical_request().unwrap_err().to_string().contains("legacy ordinary"));
        let omitted = DecodedOriginalTargetRequest::decode(br#"{"os":"linux","arch":"x86_64"}"#).unwrap();
        assert_eq!(omitted.canonical_bytes().unwrap(), LEGACY_CANONICAL);
        let current = DecodedOriginalTargetRequest::decode(CURRENT_NULL).unwrap();
        assert!(matches!(&current, DecodedOriginalTargetRequest::CurrentV1(_)));
        assert_eq!(current.canonical_bytes().unwrap(), CURRENT_NULL);
        assert!(current.require_original_physical_request().unwrap_err().to_string().contains("null original physical"));
    }

    #[test]
    fn current_request_retains_same_nested_bounds_and_canonical_commitment() {
        let wire = current_request_wire();
        let decoded = DecodedOriginalTargetRequest::decode(&wire).unwrap();
        let borrowed = decoded.require_original_physical_request().unwrap();
        let DecodedOriginalTargetRequest::CurrentV1(current) = &decoded else { panic!("current branch required") };
        assert!(std::ptr::eq(borrowed, current.original_physical_bounds.as_ref().unwrap()));
        assert_eq!(borrowed, &request());
        assert_eq!(borrowed.canonical_commitment().unwrap(), request().canonical_commitment().unwrap());
        let canonical = decoded.canonical_bytes().unwrap();
        assert_eq!(canonical, wire);
        let roundtrip = DecodedOriginalTargetRequest::decode(&canonical).unwrap();
        assert_eq!(roundtrip.require_original_physical_request().unwrap(), borrowed);
        let changed = String::from_utf8(wire).unwrap().replace("\"maximum_private_source_inodes\":10", "\"maximum_private_source_inodes\":11");
        assert_ne!(DecodedOriginalTargetRequest::decode(changed.as_bytes()).unwrap().canonical_bytes().unwrap(), canonical);
    }

    #[test]
    fn schema_and_request_presence_are_authoritative_without_legacy_fallback() {
        for raw in [
            br#"{"os":"linux","arch":"x86_64","schema":null}"#.as_slice(),
            br#"{"os":"linux","arch":"x86_64","schema":1}"#.as_slice(),
            br#"{"os":"linux","arch":"x86_64","schema":false}"#.as_slice(),
            br#"{"os":"linux","arch":"x86_64","schema":"unknown"}"#.as_slice(),
            br#"{"os":"linux","arch":"x86_64","original_physical_bounds":null}"#.as_slice(),
            br#"{"os":"linux","arch":"x86_64","original_physical_bounds":{}}"#.as_slice(),
        ] {
            assert!(DecodedOriginalTargetRequest::decode(raw).is_err());
        }
        let current = String::from_utf8(CURRENT_NULL.to_vec()).unwrap();
        let absent = current.replace("\"original_physical_bounds\":null,", "");
        let error = DecodedOriginalTargetRequest::decode(absent.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("missing original physical request field"));
        for malformed in [
            current.replace("\"resources\":[],", ""),
            current.replace("\"resources\":[]", "\"resources\":null"),
            current.replace("\"original_physical_bounds\":null", "\"original_physical_bounds\":{}"),
            current.replace("\"os\":\"linux\"", "\"os\":1"),
            current.replace("\"os\":\"linux\"", "\"os\":\"linux\",\"unknown\":1"),
        ] {
            assert!(DecodedOriginalTargetRequest::decode(malformed.as_bytes()).is_err(), "{malformed}");
        }
    }

    #[test]
    fn nested_invalid_request_uses_existing_finite_validator() {
        let wire = String::from_utf8(current_request_wire()).unwrap();
        for (old, new, diagnostic) in [
            ("\"maximum_processes\":2", "\"maximum_processes\":0", "finite structural bounds"),
            ("\"maximum_processes\":2", "\"maximum_processes\":257", "finite structural bounds"),
            ("\"maximum_memory_bytes\":1000", "\"maximum_memory_bytes\":230", "no memory headroom"),
            ("\"maximum_single_document_bytes\":40", "\"maximum_single_document_bytes\":81", "document exceeds retained export"),
            (ORIGINAL_PHYSICAL_BOUNDS_SCHEMA, "ryeos.original_physical_resource_bounds.v2", "unsupported original physical"),
        ] {
            let changed = wire.replace(old, new);
            let error = DecodedOriginalTargetRequest::decode(changed.as_bytes()).unwrap_err();
            assert!(error.to_string().contains(diagnostic), "{error}");
        }
        for changed in [
            wire.replace("\"maximum_processes\":2,", ""),
            wire.replace("\"maximum_processes\":2", "\"maximum_processes\":null"),
            wire.replace("\"maximum_processes\":2", "\"maximum_processes\":1.0"),
            wire.replace("\"maximum_memory_bytes\":1000", "\"maximum_memory_bytes\":\"1000\""),
        ] {
            assert!(DecodedOriginalTargetRequest::decode(changed.as_bytes()).is_err());
        }
    }

    #[test]
    fn recursive_raw_duplicates_refuse_before_fact_maps_overwrite() {
        for raw in [
            br#"{"os":"linux","os":"linux","arch":"x86_64"}"#.as_slice(),
            br#"{"schema":"ryeos.execution_target.v1","schema":"ryeos.execution_target.v1"}"#.as_slice(),
            br#"{"os":"linux","arch":"x86_64","resources":[{"exact_facts":{"gpu":"a","gpu":"b"}}]}"#.as_slice(),
            br#"{"os":"linux","arch":"x86_64","resources":[{"minimum_facts":{"gpu":1,"gpu":2}}]}"#.as_slice(),
            br#"{"os":"linux","arch":"x86_64","resources":[{"exact_facts":{"gpu":"a","\u0067pu":"b"}}]}"#.as_slice(),
            br#"{"schema":"ryeos.execution_target.v1","original_physical_bounds":null,"original_physical_bounds":null}"#.as_slice(),
            br#"{"schema":"ryeos.execution_target.v1","original_physical_bounds":{"maximum_processes":2,"maximum_processes":2}}"#.as_slice(),
            br#"{"unknown":[{"nested":1,"nested":2}]}"#.as_slice(),
        ] {
            let error = DecodedOriginalTargetRequest::decode(raw).unwrap_err();
            assert!(error.to_string().contains("raw duplicate object key"), "{error}");
        }
    }

    #[test]
    fn raw_decode_bound_and_trailing_tokens_refuse_before_admission() {
        assert!(DecodedOriginalTargetRequest::decode(b"").is_err());
        assert!(DecodedOriginalTargetRequest::decode(b"{}").is_err());
        let mut trailing = LEGACY.to_vec();
        trailing.extend_from_slice(b" {}");
        assert!(DecodedOriginalTargetRequest::decode(&trailing).is_err());
        let mut padded = LEGACY.to_vec();
        padded.resize(MAX_ORIGINAL_TARGET_RAW_BYTES, b' ');
        assert_eq!(DecodedOriginalTargetRequest::decode(&padded).unwrap().canonical_bytes().unwrap(), LEGACY_CANONICAL);
        padded.push(b' ');
        let error = DecodedOriginalTargetRequest::decode(&padded).unwrap_err();
        assert!(error.to_string().contains("raw document exceeds decode bound"));
    }

    #[test]
    fn raw_depth_node_and_object_guards_have_exact_data_boundaries() {
        let nested = |arrays: usize| format!("{}null{}", "[".repeat(arrays), "]".repeat(arrays));
        scan_data(nested(MAX_ORIGINAL_TARGET_RAW_DEPTH - 1).as_bytes()).unwrap();
        let error = scan_data(nested(MAX_ORIGINAL_TARGET_RAW_DEPTH).as_bytes()).unwrap_err();
        assert!(error.to_string().contains("raw depth exceeds bound"));
        let nodes = |values: usize| format!("[{}]", vec!["null"; values].join(","));
        scan_data(nodes(MAX_ORIGINAL_TARGET_RAW_VALUES - 1).as_bytes()).unwrap();
        let error = scan_data(nodes(MAX_ORIGINAL_TARGET_RAW_VALUES).as_bytes()).unwrap_err();
        assert!(error.to_string().contains("raw value count exceeds bound"));
        let members = |count: usize| format!("{{{}}}", (0..count)
            .map(|index| format!("\"key{index}\":null")).collect::<Vec<_>>().join(","));
        scan_data(members(MAX_ORIGINAL_TARGET_RAW_MEMBERS).as_bytes()).unwrap();
        let error = scan_data(members(MAX_ORIGINAL_TARGET_RAW_MEMBERS + 1).as_bytes()).unwrap_err();
        assert!(error.to_string().contains("raw object member count exceeds bound"));
        // Successful raw scans are NOT successful target decoding.
        assert!(DecodedOriginalTargetRequest::decode(nodes(2).as_bytes()).is_err());
        assert!(DecodedOriginalTargetRequest::decode(members(2).as_bytes()).is_err());
    }

    #[test]
    fn portable_ordinary_resource_semantics_remain_separate_from_physical_request() {
        let target = br#"{"os":"windows","arch":"x86_64","resources":[{"class":"gpu","count":1,"allocation":"exclusive","access":"deployment_visible","exact_facts":{"model":"example"},"minimum_facts":{"memory":1}}]}"#;
        let decoded = DecodedOriginalTargetRequest::decode(target).unwrap();
        assert!(decoded.require_original_physical_request().is_err());
        // Pure decoding deliberately does not match this host's platform.
        let target = String::from_utf8(target.to_vec()).unwrap();
        for invalid in [
            target.replace("\"count\":1", "\"count\":0"),
            target.replace("\"count\":1", "\"count\":1025"),
            target.replace("\"count\":1", "\"count\":-1"),
            target.replace("\"count\":1", "\"count\":1.0"),
            target.replace("\"count\":1", "\"count\":\"1\""),
            target.replace("\"memory\":1", "\"memory\":18446744073709551616"),
            target.replace("\"memory\":1", "\"model\":1"),
            target.replace("\"gpu\"", "\"bad class\""),
        ] {
            assert!(DecodedOriginalTargetRequest::decode(invalid.as_bytes()).is_err(), "{invalid}");
        }
    }
}

#[cfg(test)]
mod current_request_codec_controls {
    use super::*;
    const CURRENT: &str = r#"{"schema":"ryeos.execution_target.v1","os":"linux","arch":"x86_64","resources":[],"original_physical_bounds":null}"#;
    #[test]
    fn current_wrapper_stream_roundtrip_and_required_presence() {
        let current: CurrentExecutionTargetRequest = serde_json::from_str(CURRENT).unwrap();
        let canonical = current.canonical_bytes().unwrap();
        let round: CurrentExecutionTargetRequest = serde_json::from_slice(&canonical).unwrap();
        assert_eq!(round.canonical_bytes().unwrap(), canonical);
        for bad in [CURRENT.replace(",\"original_physical_bounds\":null", ""),
            CURRENT.replace(",\"resources\":[]", ""),
            CURRENT.replace("ryeos.execution_target.v1", "unknown"),
            CURRENT.replace("\"resources\":[]", "\"resources\":[],\"resources\":[]") ] {
            assert!(serde_json::from_str::<CurrentExecutionTargetRequest>(&bad).is_err());
        }
        assert!(serde_json::from_str::<CurrentExecutionTargetRequest>(r#"{"os":"linux","arch":"x86_64"}"#).is_err());
        assert!(serde_json::from_str::<CurrentExecutionTargetRequest>("null").is_err());
    }
    #[test]
    fn transcode_keeps_lossless_integer_and_caps_escaped_output_before_growth() {
        let mut de = serde_json::Deserializer::from_str("18446744073709551615");
        assert_eq!(transcode_original_target(&mut de).unwrap(), b"18446744073709551615");
        // Quoted control bytes expand sixfold; no unbounded Value tree.
        let value = serde_json::Value::String("\u{0001}".repeat(MAX_ORIGINAL_TARGET_RAW_BYTES / 6 + 1));
        assert!(transcode_original_target(&value).is_err());
        let mut writer = OriginalTargetWriter(Vec::new());
        use std::io::Write as _;
        writer.write_all(&vec![b'x'; MAX_ORIGINAL_TARGET_RAW_BYTES]).unwrap();
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.0.len(), MAX_ORIGINAL_TARGET_RAW_BYTES);
    }
    #[test]
    fn borrowed_projection_preserves_legacy_and_current_without_value_clone() {
        let legacy = serde_json::json!({"os":"linux","arch":"x86_64"});
        assert!(project_current_target_data(&legacy).unwrap().1.is_none());
        let current: serde_json::Value = serde_json::from_str(CURRENT).unwrap();
        let (ordinary, request) = project_current_target_data(&current).unwrap();
        assert_eq!(&ordinary, request.as_ref().unwrap().ordinary());
    }
}
