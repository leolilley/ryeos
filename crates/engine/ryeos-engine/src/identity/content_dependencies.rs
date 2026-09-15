//! Mechanical prepared-content contributions to the enclosing program identity.
//!
//! This projection does not authorize dependencies. Admission supplies their
//! finalized resolutions; recovery reproduces the same projection before the
//! enclosing program is finalized. No session capsule, local path, or timestamp
//! participates. Ordered executable search remains ordered executable behavior.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::bail;
use ryeos_handler_protocol::ExecutableSearchPathEntryWire;
use serde::{Deserialize, Serialize};

use crate::resolution::ResolutionOutput;
use crate::runtime_registry::{
    MAX_LAUNCH_CONTENT_DEPENDENCIES, MAX_LAUNCH_CONTENT_TARGETS,
    MAX_LAUNCH_EXECUTABLE_SEARCH_ENTRIES,
};

pub const EFFECTIVE_CONTENT_DEPENDENCIES_DERIVED_KEY: &str = "effective_content_dependencies";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveContentDependencyIdentity {
    pub canonical_ref: String,
    pub effective_definition_digest: String,
    pub targets: Vec<String>,
    pub executable_search: Vec<ExecutableSearchPathEntryWire>,
}

impl EffectiveContentDependencyIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        let reference = crate::canonical_ref::CanonicalRef::parse(&self.canonical_ref)?;
        if reference.to_string() != self.canonical_ref
            || reference.suffix.is_some()
            || self.canonical_ref.len() > 2048
            || !lillux::valid_hash(&self.effective_definition_digest)
            || self
                .effective_definition_digest
                .bytes()
                .any(|byte| byte.is_ascii_uppercase())
            || self.targets.is_empty()
            || self.targets.len() > MAX_LAUNCH_CONTENT_TARGETS
            || self.targets.windows(2).any(|pair| pair[0] >= pair[1])
            || self.executable_search.len() > MAX_LAUNCH_EXECUTABLE_SEARCH_ENTRIES
        {
            bail!("effective content dependency identity is not bounded and canonical");
        }
        for target in &self.targets {
            if !crate::runtime_registry::valid_launch_name(target) {
                bail!("effective content target name is outside the launch contract");
            }
        }
        let mut seen = BTreeSet::new();
        for entry in &self.executable_search {
            crate::external_content::validate_declaration_id(&entry.realization_id)?;
            if entry.relative_directory != "." {
                ryeos_state::objects::validate_canonical_project_relative_path(
                    &entry.relative_directory,
                )?;
            }
            if entry.relative_directory.len() > crate::external_content::MAX_ENTRY_PATH_BYTES
                || !seen.insert((&entry.realization_id, &entry.relative_directory))
            {
                bail!("effective content search entries are not bounded and unique");
            }
        }
        Ok(())
    }
}

pub type EffectiveContentDependencyIdentities =
    BTreeMap<String, EffectiveContentDependencyIdentity>;

pub fn validate_effective_content_dependency_identities(
    identities: &EffectiveContentDependencyIdentities,
) -> anyhow::Result<()> {
    if identities.len() > MAX_LAUNCH_CONTENT_DEPENDENCIES {
        bail!("effective content dependencies exceed the launch contract ceiling");
    }
    for (binding, identity) in identities {
        if !crate::runtime_registry::valid_launch_name(binding) {
            bail!("effective content binding name is outside the launch contract");
        }
        identity.validate()?;
    }
    Ok(())
}

/// Fresh admission installs only its mechanically reproduced projection.
/// Recovery requires its exact captured value, including absence for no content.
/// The caller must have admitted/verified every dependency before invoking this.
pub fn bind_effective_content_dependency_identities(
    resolution: &mut ResolutionOutput,
    identities: EffectiveContentDependencyIdentities,
    recovered: bool,
) -> anyhow::Result<()> {
    validate_effective_content_dependency_identities(&identities)?;
    let expected = (!identities.is_empty())
        .then(|| serde_json::to_value(&identities))
        .transpose()?;
    let captured = resolution
        .composed
        .derived
        .get(EFFECTIVE_CONTENT_DEPENDENCIES_DERIVED_KEY);
    if recovered {
        if let Some(value) = captured {
            let parsed: EffectiveContentDependencyIdentities =
                serde_json::from_value(value.clone())?;
            validate_effective_content_dependency_identities(&parsed)?;
        }
        if captured != expected.as_ref() {
            bail!(
                "recovered effective content dependencies contradict admitted dependency resolutions"
            );
        }
    } else {
        if captured.is_some() {
            bail!("fresh program pre-populated reserved effective content dependencies");
        }
        if let Some(expected) = expected {
            resolution.composed.derived.insert(
                EFFECTIVE_CONTENT_DEPENDENCIES_DERIVED_KEY.to_owned(),
                expected,
            );
        }
    }
    Ok(())
}
