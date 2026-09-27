//! Program requirements for external candidate execution. These identities
//! never grant cloud allocation or substitute for a protected placement binding.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_external_execution_contract::LifecycleCapability;
use serde::{Deserialize, Serialize};

use crate::external_content::products::composition::ResolvedExternalProductSelections;
use crate::objects::{
    AdmittedStructuredSessionProfile, EFFECTIVE_SOURCE_BINDING_SCHEMA,
    EXTERNAL_CONTENT_MANIFEST_KIND, EffectiveSourceClosureProjection, ExecutableSearchPathEntry,
    ExternalContentKind, ExternalContentMode, ExternalContentMountRoot,
    ExternalContentRealizationSet, SessionProcessEnvironmentValue, canonical_value_digest,
    runtime_view_mount_destination,
};

pub const PROTOCOL: &str = "ryeos.external-candidate.stdio.v1";
pub const CONNECTOR_PROTOCOL: &str = super::connector::EXTERNAL_CONNECTOR_PROTOCOL;
/// The initial runtime must qualify the whole routed execution boundary.
pub const REQUIRED_CLAIMS: &[&str] = &[
    "bounded_candidate_capture",
    "candidate_only_execution",
    "controller_credential_exclusion",
    "native_writer_exclusion",
    "no_local_execution_fallback",
];
pub const QUALIFICATION_CONTEXT_SCHEMA: &str = "ryeos.external_candidate_qualification_context.v3";
pub const QUALIFICATION_CONTEXT_PARAMETER: &str = "external_candidate_qualification_context";

/// Domain-owned use context for an independently qualified runtime. A product
/// witness proves exact bytes, while the verifier must also have exercised the
/// exact command, routing and placement requirement that consumes those bytes.
/// Generic product qualification retains this as opaque signed parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCandidateQualificationUse {
    pub schema: String,
    pub requirement_digest: String,
    pub profile_hash: String,
    pub source_binding_hash: String,
    pub source_content_manifest_hash: String,
    pub provider_executable_manifest_hash: String,
    pub execution_environment_digest: String,
}

/// Pure logical guest environment derived from admitted inputs. The caller
/// must separately prove and mount each referenced descriptor; this value
/// alone grants no filesystem access or placement authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCandidateGuestEnvironment {
    pub environment: BTreeMap<String, String>,
    pub executable_search: Vec<String>,
}

impl ExternalCandidateGuestEnvironment {
    /// Resolve only logical guest mount destinations. Descriptor custody and
    /// exact manifest verification remain with the launch owner.
    pub fn expected_destinations(
        requirement: &ExternalCandidateRequirement,
        realizations: &ExternalContentRealizationSet,
    ) -> Result<BTreeMap<String, String>> {
        requirement.validate()?;
        realizations.validate()?;
        let mut destinations = BTreeMap::new();
        for entry in realizations.iter() {
            let path = if entry.id == requirement.runtime_product_declaration_id {
                PathBuf::from(&requirement.runtime_recipe.runtime_mount_destination)
            } else {
                entry
                    .mount_root
                    .destination(Some(Path::new("/workspace")), &entry.mount)?
            };
            require_absolute_normalized(&path, "external guest realization destination")?;
            let value = path
                .to_str()
                .context("external guest realization destination is not UTF-8")?;
            ensure!(
                value.len() <= 4096 && !value.chars().any(char::is_control) && value != "/",
                "external guest realization destination is invalid"
            );
            ensure!(
                destinations.insert(entry.id.clone(), value.to_owned()).is_none(),
                "external guest realization identity is duplicated"
            );
        }
        ensure!(
            destinations.contains_key(&requirement.runtime_product_declaration_id),
            "external guest runtime product has no exact realization"
        );
        Ok(destinations)
    }

    pub fn derive(
        recipe: &ExternalCandidateRuntimeRecipe,
        process_environment: &BTreeMap<String, SessionProcessEnvironmentValue>,
        executable_search: &[ExecutableSearchPathEntry],
        destinations: &BTreeMap<String, String>,
    ) -> Result<Self> {
        recipe.validate()?;
        crate::objects::validate_session_process_environment(process_environment)?;
        ensure!(
            executable_search.len() <= crate::objects::MAX_EXECUTABLE_SEARCH_PATH_ENTRIES,
            "external guest executable search exceeds its bound"
        );
        let logical_path = |realization_id: &str, relative: &str| -> Result<String> {
            let root = destinations.get(realization_id).with_context(|| {
                format!("external guest environment lost realization `{realization_id}`")
            })?;
            require_absolute_normalized(
                Path::new(root),
                "external guest realization destination",
            )?;
            let mut path = PathBuf::from(root);
            if relative != "." {
                path.push(relative);
            }
            Ok(path
                .to_str()
                .context("external guest environment path is not UTF-8")?
                .to_owned())
        };
        let mut environment = recipe.environment.clone();
        for (name, value) in process_environment {
            let resolved = match value {
                SessionProcessEnvironmentValue::Literal { value } => value.clone(),
                SessionProcessEnvironmentValue::RealizationPath {
                    realization_id,
                    relative_path,
                    ..
                } => logical_path(realization_id, relative_path)?,
                SessionProcessEnvironmentValue::RuntimeViewDirectory { .. } => {
                    runtime_view_mount_destination(name)?
                        .to_str()
                        .context("external guest scratch destination is not UTF-8")?
                        .to_owned()
                }
            };
            ensure!(
                environment.insert(name.clone(), resolved).is_none(),
                "external guest environment collides with its runtime recipe"
            );
        }
        let mut search = Vec::with_capacity(executable_search.len());
        let mut seen = BTreeSet::new();
        for entry in executable_search {
            entry.validate()?;
            ensure!(
                seen.insert((
                    entry.realization_id.as_str(),
                    entry.relative_directory.as_str()
                )),
                "external guest executable search contains a duplicate entry"
            );
            search.push(logical_path(
                &entry.realization_id,
                &entry.relative_directory,
            )?);
        }
        if !search.is_empty() {
            ensure!(
                environment
                    .insert("PATH".into(), search.join(":"))
                    .is_none(),
                "external guest runtime recipe may not replace admitted executable search"
            );
        }
        Ok(Self {
            environment,
            executable_search: search,
        })
    }
}

impl ExternalCandidateQualificationUse {
    /// Exact guest-visible environment coordinate shared with an independent
    /// verifier. A verifier must derive it from selected/observed inputs, not
    /// accept a caller-supplied digest as evidence.
    pub fn admitted_execution_environment_digest(
        realizations: &ExternalContentRealizationSet,
        executable_search: &[ExecutableSearchPathEntry],
        process_environment: &BTreeMap<String, SessionProcessEnvironmentValue>,
    ) -> Result<String> {
        realizations.validate()?;
        ensure!(
            executable_search.len() <= crate::objects::MAX_EXECUTABLE_SEARCH_PATH_ENTRIES,
            "external candidate executable search exceeds its bound"
        );
        for entry in executable_search {
            entry.validate()?;
            ensure!(
                realizations
                    .iter()
                    .any(|realization| realization.id == entry.realization_id),
                "external candidate executable search has no exact realization"
            );
        }
        crate::objects::validate_session_process_environment(process_environment)?;
        for value in process_environment.values() {
            if let SessionProcessEnvironmentValue::RealizationPath { realization_id, .. } = value {
                ensure!(
                    realizations
                        .iter()
                        .any(|realization| realization.id == *realization_id),
                    "external candidate process environment has no exact realization"
                );
            }
        }
        canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-candidate.execution-environment.v1",
            "executable_search": executable_search,
            "process_environment": process_environment,
            "realizations": realizations.to_value()?,
        }))
    }

    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        let context: Self = serde_json::from_value(value.clone())?;
        context.validate()?;
        Ok(context)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == QUALIFICATION_CONTEXT_SCHEMA,
            "invalid external candidate qualification context"
        );
        for hash in [
            &self.requirement_digest,
            &self.profile_hash,
            &self.source_binding_hash,
            &self.source_content_manifest_hash,
            &self.provider_executable_manifest_hash,
            &self.execution_environment_digest,
        ] {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "invalid external candidate qualification identity"
            );
        }
        Ok(())
    }

    pub fn parameters(&self) -> Result<serde_json::Value> {
        self.validate()?;
        Ok(serde_json::json!({(QUALIFICATION_CONTEXT_PARAMETER): self}))
    }

    pub fn from_admitted_inputs(
        requirement: &ExternalCandidateRequirement,
        profile: &AdmittedStructuredSessionProfile,
        source: &EffectiveSourceClosureProjection,
        realizations: &ExternalContentRealizationSet,
        executable_search: &[ExecutableSearchPathEntry],
        process_environment: &BTreeMap<String, SessionProcessEnvironmentValue>,
    ) -> Result<Self> {
        requirement.validate()?;
        profile.validate()?;
        source.validate()?;
        realizations.validate()?;
        ensure!(
            source.schema == EFFECTIVE_SOURCE_BINDING_SCHEMA
                && profile.external_candidate_requirement()?.as_ref() == Some(requirement),
            "external candidate qualification source or profile differs from its requirement"
        );
        let realization_id = profile
            .contract
            .get("workload_realization_id")
            .and_then(serde_json::Value::as_str)
            .context("external candidate profile has no workload realization id")?;
        let realization = realizations
            .iter()
            .find(|entry| entry.id == realization_id)
            .context("external candidate provider executable realization is absent")?;
        ensure!(
            realization.mode == ExternalContentMode::Pinned
                && realization.mount_root == ExternalContentMountRoot::ExecutionRuntime,
            "external candidate provider executable is not an exact pinned runtime realization"
        );
        let context = Self {
            schema: QUALIFICATION_CONTEXT_SCHEMA.to_owned(),
            requirement_digest: requirement.qualification_requirement_digest()?,
            profile_hash: profile.profile_hash.clone(),
            source_binding_hash: source.binding_hash.clone(),
            source_content_manifest_hash: source.content_manifest_hash.clone(),
            provider_executable_manifest_hash: realization.manifest_hash.clone(),
            execution_environment_digest: Self::admitted_execution_environment_digest(
                realizations,
                executable_search,
                process_environment,
            )?,
        };
        context.validate()?;
        Ok(context)
    }
}

/// Admission ceiling for the profile-owned recipe. The signed structured-
/// session profile carries this recipe inside its separately bounded contract;
/// the launcher also carries its admitted program and an independently checked
/// recipe projection. Each enclosing document must pass its own bound.
pub const MAX_EXTERNAL_RUNTIME_RECIPE_BYTES: usize = 96 * 1024;

/// Exact credential-free exec-server recipe selected by the signed worker
/// profile. The referenced executable remains content-owned by the qualified
/// runtime product; this recipe grants no filesystem or placement authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCandidateRuntimeRecipe {
    pub schema: u32,
    pub runtime_mount_destination: String,
    pub executable_relative_path: String,
    pub argv0: String,
    pub arguments: Vec<String>,
    pub cwd: String,
    pub environment: BTreeMap<String, String>,
    pub max_stdout_bytes: u64,
    pub max_stderr_bytes: u64,
    pub proc_filesystem: ExternalCandidateProcFilesystem,
    pub contain_process_group: bool,
    pub nested_sandbox: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCandidateProcFilesystem {
    Empty,
    PidNamespace,
    PidNamespaceNested,
}

impl ExternalCandidateRuntimeRecipe {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 2,
            "unsupported external runtime recipe schema"
        );
        let mount = Path::new(&self.runtime_mount_destination);
        require_absolute_normalized(mount, "runtime mount destination")?;
        ensure!(
            !mount.starts_with("/workspace")
                && !Path::new("/workspace").starts_with(mount)
                && !lillux::linux_sandbox_mount_overlaps_managed_namespace(mount),
            "external runtime mount overlaps a protected namespace"
        );
        let executable = Path::new(&self.executable_relative_path);
        require_relative_normalized(executable, "runtime executable")?;
        ensure!(
            executable.components().count() <= 64,
            "external runtime executable is too deep"
        );
        let namespace_executable = mount.join(executable);
        require_absolute_normalized(&namespace_executable, "namespace executable")?;
        ensure!(
            !self.argv0.is_empty()
                && self.argv0.len() <= 4096
                && !self.argv0.contains('\0')
                && self.arguments.len() <= 256
                && self
                    .arguments
                    .iter()
                    .all(|value| value.len() <= 64 * 1024 && !value.contains('\0')),
            "external runtime arguments exceed bounds"
        );
        let cwd = Path::new(&self.cwd);
        require_absolute_normalized(cwd, "candidate working directory")?;
        ensure!(
            cwd.starts_with("/workspace"),
            "external candidate working directory is outside /workspace"
        );
        ensure!(
            self.environment.len() <= 256
                && self.environment.iter().all(|(name, value)| {
                    valid_environment_name(name)
                        && name != "PATH"
                        && name.len() <= 256
                        && value.len() <= 64 * 1024
                        && !value.contains('\0')
                }),
            "external runtime environment exceeds bounds"
        );
        ensure!(
            (1..=64 * 1024 * 1024).contains(&self.max_stdout_bytes)
                && (1..=64 * 1024 * 1024).contains(&self.max_stderr_bytes),
            "external runtime output bounds are invalid"
        );
        ensure!(
            matches!(
                (
                    self.proc_filesystem,
                    self.contain_process_group,
                    self.nested_sandbox
                ),
                (ExternalCandidateProcFilesystem::Empty, true, false)
                    | (ExternalCandidateProcFilesystem::PidNamespace, true, false)
                    | (
                        ExternalCandidateProcFilesystem::PidNamespaceNested,
                        false,
                        true
                    )
            ),
            "external runtime containment settings are inconsistent"
        );
        ensure!(
            lillux::canonical_json(&serde_json::to_value(self)?)?.len()
                <= MAX_EXTERNAL_RUNTIME_RECIPE_BYTES,
            "external runtime recipe exceeds its encoded bound"
        );
        Ok(())
    }

    pub fn namespace_executable(&self) -> Result<String> {
        self.validate()?;
        Ok(Path::new(&self.runtime_mount_destination)
            .join(&self.executable_relative_path)
            .to_str()
            .context("external runtime executable is not UTF-8")?
            .to_owned())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-candidate-runtime-recipe.v1",
            "recipe": self,
        }))
    }
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn require_absolute_normalized(path: &Path, label: &str) -> Result<()> {
    ensure!(path.is_absolute(), "external {label} is not absolute");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
            && path.as_os_str().as_encoded_bytes().len() <= 4096
            && !path.as_os_str().as_encoded_bytes().contains(&0),
        "external {label} is not normalized"
    );
    Ok(())
}

fn require_relative_normalized(path: &Path, label: &str) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
            && path.as_os_str().as_encoded_bytes().len() <= 4096
            && !path.as_os_str().as_encoded_bytes().contains(&0),
        "external {label} is not normalized"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCandidateRequirement {
    pub schema: u32,
    pub protocol: String,
    pub connector_protocol: String,
    pub execution_route: ExternalCandidateExecutionRoute,
    /// Lifecycle guarantees required by this exact program. An explicit empty
    /// set permits admission without reconciliation guarantees; it does not
    /// authorize recovery from an uncertain provider outcome.
    pub required_lifecycle_capabilities: BTreeSet<LifecycleCapability>,
    /// Stable id of the signed feature-bundle provider declaration. It is not
    /// a bundle path or executable name and grants no placement authority.
    pub provider_declaration_id: String,
    /// Exact profile-home-relative destination owned by that provider's signed
    /// private configuration contract. Repeating it here commits the capture
    /// exclusion in the session capsule; app admission later requires equality
    /// with the installed signed declaration before any contact.
    pub provider_configuration_destination: String,
    pub runtime_product_declaration_id: String,
    pub runtime_recipe: ExternalCandidateRuntimeRecipe,
}

/// Closed provider-side routing policy. `ConnectorOnly` compiles to a single
/// command-backed environment with local execution disabled; there is no
/// compatibility spelling that can silently re-enable the controller host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCandidateExecutionRoute {
    ConnectorOnly,
}

impl ExternalCandidateRequirement {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 6
                && self.protocol == PROTOCOL
                && self.connector_protocol == CONNECTOR_PROTOCOL
                && self.execution_route == ExternalCandidateExecutionRoute::ConnectorOnly,
            "external candidate protocol is not admitted"
        );
        crate::external_content::products::validate_name(&self.provider_declaration_id)?;
        crate::objects::validate_session_configuration_destination(
            &self.provider_configuration_destination,
        )?;
        crate::external_content::products::validate_name(&self.runtime_product_declaration_id)?;
        self.runtime_recipe.validate()
    }

    pub fn qualification_requirement_digest(&self) -> Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-candidate-requirement.v1",
            "requirement": self,
        }))
    }

    pub fn resolve_for_use(
        &self,
        selections: Option<&ResolvedExternalProductSelections>,
        qualification_use: &ExternalCandidateQualificationUse,
    ) -> Result<AdmittedExternalCandidateProgram> {
        self.validate()?;
        qualification_use.validate()?;
        ensure!(
            qualification_use.requirement_digest == self.qualification_requirement_digest()?,
            "external candidate qualification use differs from its requirement"
        );
        let selections =
            selections.context("external candidate requires retained runtime products")?;
        selections.validate()?;
        let runtime = selections
            .get(&self.runtime_product_declaration_id)
            .context("external candidate runtime product is absent")?;
        ensure!(
            matches!(
                runtime.manifest_kind.as_str(),
                EXTERNAL_CONTENT_MANIFEST_KIND
                    | crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
            ) && runtime.declaration.kind == ExternalContentKind::Tree,
            "external candidate runtime requires an exact small or large content tree"
        );
        let qualification = runtime
            .qualification
            .as_ref()
            .context("external candidate runtime has no admitted qualification")?;
        let context = qualification
            .evidence
            .policy_source
            .policy
            .verifier_parameters
            .get(QUALIFICATION_CONTEXT_PARAMETER)
            .context("external candidate qualification has no sealed use context")?;
        let qualified_use = ExternalCandidateQualificationUse::from_value(context)?;
        if qualified_use != *qualification_use {
            let mut changed = Vec::new();
            for (name, qualified, current) in [
                (
                    "requirement",
                    &qualified_use.requirement_digest,
                    &qualification_use.requirement_digest,
                ),
                (
                    "profile",
                    &qualified_use.profile_hash,
                    &qualification_use.profile_hash,
                ),
                (
                    "source_binding",
                    &qualified_use.source_binding_hash,
                    &qualification_use.source_binding_hash,
                ),
                (
                    "source_content",
                    &qualified_use.source_content_manifest_hash,
                    &qualification_use.source_content_manifest_hash,
                ),
                (
                    "provider_executable",
                    &qualified_use.provider_executable_manifest_hash,
                    &qualification_use.provider_executable_manifest_hash,
                ),
                (
                    "execution_environment",
                    &qualified_use.execution_environment_digest,
                    &qualification_use.execution_environment_digest,
                ),
            ] {
                if qualified != current {
                    changed.push(name);
                }
            }
            anyhow::bail!(
                "external candidate qualification tested a different admitted use: {}",
                changed.join(",")
            );
        }
        // Selection validation joins the proof to its exact policy and subject.
        // Require these claims in the signed relationship too: an incidental
        // verifier result cannot widen the consumer's qualification allowance.
        ensure!(
            REQUIRED_CLAIMS.iter().all(|claim| runtime
                .relationship
                .qualification
                .required_claims
                .iter()
                .any(|required| required == claim)),
            "external candidate runtime qualification is insufficient"
        );
        Ok(AdmittedExternalCandidateProgram {
            requirement: self.clone(),
            qualification_use: qualification_use.clone(),
            runtime_manifest_kind: runtime.manifest_kind.clone(),
            runtime_manifest_hash: runtime.manifest_hash.clone(),
            runtime_witness_hash: runtime.witness_hash.clone(),
            qualification_attestation_hash: qualification.attestation_hash.clone(),
            selection_identity_digest: runtime.semantic_identity_digest()?,
            runtime_recipe_digest: self.runtime_recipe.digest()?,
        })
    }
}

/// Projection of already retained product authority. All CAS edges remain
/// owned by the capsule's full product selections, which must be rejoined on
/// validation. No occurrence, capsule hash or operator credential enters it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedExternalCandidateProgram {
    pub requirement: ExternalCandidateRequirement,
    pub qualification_use: ExternalCandidateQualificationUse,
    pub runtime_manifest_kind: String,
    pub runtime_manifest_hash: String,
    pub runtime_witness_hash: String,
    pub qualification_attestation_hash: String,
    pub selection_identity_digest: String,
    pub runtime_recipe_digest: String,
}

impl AdmittedExternalCandidateProgram {
    pub fn validate(&self) -> Result<()> {
        self.requirement.validate()?;
        self.qualification_use.validate()?;
        ensure!(
            self.qualification_use.requirement_digest
                == self.requirement.qualification_requirement_digest()?,
            "external candidate program qualification use differs from its requirement"
        );
        ensure!(
            matches!(
                self.runtime_manifest_kind.as_str(),
                EXTERNAL_CONTENT_MANIFEST_KIND
                    | crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
            ),
            "external candidate runtime manifest kind is not admitted"
        );
        for hash in [
            &self.runtime_manifest_hash,
            &self.runtime_witness_hash,
            &self.qualification_attestation_hash,
            &self.selection_identity_digest,
            &self.runtime_recipe_digest,
        ] {
            super::hash(hash)?;
        }
        ensure!(
            self.runtime_recipe_digest == self.requirement.runtime_recipe.digest()?,
            "external candidate runtime recipe changed its retained identity"
        );
        Ok(())
    }

    pub fn verify_selections(
        &self,
        selections: Option<&ResolvedExternalProductSelections>,
    ) -> Result<()> {
        self.validate()?;
        ensure!(
            self.requirement
                .resolve_for_use(selections, &self.qualification_use)?
                == *self,
            "external candidate program contradicts retained product authority"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-candidate-program.v3",
            "program": self,
        }))
    }
}

/// Closed transport projection. A worker's profile recipe and an ordinary
/// direct execution's compiled plan are different authorities, never fallbacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "program",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AdmittedExternalExecutionProgram {
    StructuredSession(AdmittedExternalCandidateProgram),
    DirectCommand(AdmittedExternalDirectProgram),
}

impl From<AdmittedExternalCandidateProgram> for AdmittedExternalExecutionProgram {
    fn from(program: AdmittedExternalCandidateProgram) -> Self {
        Self::StructuredSession(program)
    }
}

impl AdmittedExternalExecutionProgram {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::StructuredSession(program) => program.validate(),
            Self::DirectCommand(program) => program.validate(),
        }
    }

    pub fn digest(&self) -> Result<String> {
        match self {
            Self::StructuredSession(program) => program.digest(),
            Self::DirectCommand(program) => program.digest(),
        }
    }

    pub fn runtime_manifest_hash(&self) -> Result<&str> {
        match self {
            Self::StructuredSession(program) => Ok(&program.runtime_manifest_hash),
            Self::DirectCommand(program) => program.runtime_manifest_hash(),
        }
    }

    pub fn execution_mode(&self) -> ryeos_external_execution_contract::ExternalExecutionMode {
        match self {
            Self::StructuredSession(_) => {
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {}
            }
            Self::DirectCommand(program) => program.projection.execution_mode,
        }
    }

    pub fn worker(&self) -> Result<&AdmittedExternalCandidateProgram> {
        match self {
            Self::StructuredSession(program) => Ok(program),
            Self::DirectCommand(_) => anyhow::bail!("external program is not a structured session"),
        }
    }

    pub fn worker_mut(&mut self) -> Result<&mut AdmittedExternalCandidateProgram> {
        match self {
            Self::StructuredSession(program) => Ok(program),
            Self::DirectCommand(_) => anyhow::bail!("external program is not a structured session"),
        }
    }

    pub fn validate_guest_inputs(
        &self,
        inputs: &ryeos_external_execution_contract::ExternalGuestInputProjection,
    ) -> Result<()> {
        self.validate()?;
        inputs.validate()?;
        match self {
            Self::StructuredSession(_) => Ok(()),
            Self::DirectCommand(program) => program.validate_guest_inputs(inputs),
        }
    }
}

pub const MAX_EXTERNAL_DIRECT_STDIN_BYTES: usize = 64 * 1024;
pub const MAX_EXTERNAL_DIRECT_PROGRAM_BYTES: usize = 192 * 1024;

/// Exact one-shot input, materialized by the ordinary-plan compiler after
/// typed namespace relocation. Empty input means close stdin, not an open
/// session lane. This value is content, not permission to write or release.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalDirectSealedInput {
    bytes_base64: String,
    bytes: u64,
    sha256: String,
}

impl std::fmt::Debug for ExternalDirectSealedInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalDirectSealedInput")
            .field("bytes", &self.bytes)
            .field("sha256", &self.sha256)
            .finish()
    }
}

impl ExternalDirectSealedInput {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_EXTERNAL_DIRECT_STDIN_BYTES,
            "external direct stdin exceeds bounds"
        );
        Ok(Self {
            bytes_base64: STANDARD.encode(bytes),
            bytes: bytes.len() as u64,
            sha256: lillux::sha256_hex(bytes),
        })
    }

    pub fn decoded_bytes(&self) -> Result<Vec<u8>> {
        // Bound encoded text before decoding, including malformed input.
        ensure!(
            self.bytes <= MAX_EXTERNAL_DIRECT_STDIN_BYTES as u64
                && self.bytes_base64.len() <= MAX_EXTERNAL_DIRECT_STDIN_BYTES.div_ceil(3) * 4,
            "external direct stdin exceeds bounds"
        );
        super::hash(&self.sha256)?;
        let bytes = STANDARD
            .decode(&self.bytes_base64)
            .map_err(|_| anyhow::anyhow!("external direct stdin encoding is invalid"))?;
        ensure!(
            bytes.len() as u64 == self.bytes
                && STANDARD.encode(&bytes) == self.bytes_base64
                && lillux::sha256_hex(&bytes) == self.sha256,
            "external direct stdin changed its sealed identity"
        );
        Ok(bytes)
    }

    pub fn validate(&self) -> Result<()> {
        self.decoded_bytes().map(|_| ())
    }
}

/// Finite native mechanism required by this first direct-command projection.
/// It requires isolated networking, read-only C, empty proc, process-group
/// containment and no nested sandbox. It is not controller enforcement proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalDirectNativeRequirements {
    LinuxIsolatedReadOnly { required_arch: String },
}

impl ExternalDirectNativeRequirements {
    pub fn validate(&self) -> Result<()> {
        let Self::LinuxIsolatedReadOnly { required_arch } = self;
        ensure!(
            matches!(required_arch.as_str(), "x86_64" | "aarch64"),
            "external direct native target architecture is unsupported"
        );
        Ok(())
    }
}

pub const MAX_EXTERNAL_DIRECT_TIMEOUT_SECONDS: u64 = 3_600;

/// Compiler output, not a second user-authored recipe. Only the trusted app
/// compiler can interpret the sealed engine plan and produce these values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalDirectCommandProjection {
    pub argv0: String,
    pub arguments: Vec<String>,
    pub cwd: String,
    pub environment: BTreeMap<String, String>,
    pub stdin: ExternalDirectSealedInput,
    pub endpoint_binding_id: String,
    pub endpoint_binding_digest: String,
    pub execution_mode: ryeos_external_execution_contract::ExternalExecutionMode,
    pub timeout_seconds: u64,
    pub native: ExternalDirectNativeRequirements,
}

impl ExternalDirectCommandProjection {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.argv0.is_empty()
                && self.argv0.len() <= 4096
                && !self.argv0.contains('\0')
                && self.arguments.len() <= 256
                && self
                    .arguments
                    .iter()
                    .all(|arg| arg.len() <= 64 * 1024 && !arg.contains('\0')),
            "external direct arguments exceed bounds"
        );
        require_absolute_normalized(Path::new(&self.cwd), "direct cwd")?;
        ensure!(
            Path::new(&self.cwd).starts_with("/workspace"),
            "external direct cwd is outside /workspace"
        );
        ensure!(
            self.environment.len() <= 256
                && self
                    .environment
                    .iter()
                    .all(|(name, value)| valid_environment_name(name)
                        && name.len() <= 256
                        && value.len() <= 64 * 1024
                        && !value.contains('\0')),
            "external direct environment exceeds bounds"
        );
        ensure!(
            !self.endpoint_binding_id.is_empty()
                && self.endpoint_binding_id.len() <= 128
                && !matches!(self.endpoint_binding_id.as_str(), "." | "..")
                && self
                    .endpoint_binding_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
            "external direct endpoint binding id is invalid"
        );
        super::hash(&self.endpoint_binding_digest)?;
        self.execution_mode.validate()?;
        ensure!(
            (1..=MAX_EXTERNAL_DIRECT_TIMEOUT_SECONDS).contains(&self.timeout_seconds),
            "external direct timeout exceeds bounds"
        );
        ensure!(
            matches!(
                self.execution_mode,
                ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand { .. }
            ),
            "external direct projection requires direct execution mode"
        );
        self.native.validate()?;
        self.stdin.validate()
    }
}

/// Engine-free projection bound to the existing ordinary closure. Deserializing
/// it proves no admission. The app must rejoin it to the sealed capsule before
/// allocation; the channel independently binds that capsule and this digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedExternalDirectProgram {
    execution_plan_hash: String,
    execution_closure_digest: String,
    command: crate::objects::AdmittedDirectCommandClosure,
    projection: ExternalDirectCommandProjection,
    guest_input_identity: String,
}

impl AdmittedExternalDirectProgram {
    /// Checks closure/artifact/guest identities without interpreting engine
    /// JSON. It cannot independently establish that caller-supplied argv/env
    /// matches that opaque plan: the sole trusted app compiler must derive and
    /// recheck the projection. This factory grants no allocation authority.
    pub fn from_compiled_closure(
        closure: &crate::objects::AdmittedExecutionClosure,
        artifact: &crate::objects::AdmittedLaunchArtifactIdentity,
        projection: ExternalDirectCommandProjection,
        inputs: &ryeos_external_execution_contract::ExternalGuestInputProjection,
    ) -> Result<Self> {
        use crate::objects::{
            AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity, DirectExecutableIdentity,
        };
        closure.validate()?;
        artifact.validate()?;
        let AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            command,
            ..
        } = closure
        else {
            anyhow::bail!("external direct projection requires an ordinary direct closure");
        };
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            execution_plan_hash,
            executable_identity,
            ..
        } = artifact
        else {
            anyhow::bail!("external direct projection requires an ordinary direct artifact");
        };
        ensure!(
            lillux::sha256_hex(lillux::canonical_json(execution_plan)?.as_bytes())
                == *execution_plan_hash,
            "external direct projection changed its sealed execution plan"
        );
        let crate::objects::AdmittedDirectCommandClosure::RealizationMember {
            executable_blob_hash,
            ..
        } = command
        else {
            anyhow::bail!(
                "external direct projection requires an exact realization member command"
            );
        };
        let expected_hash = match executable_identity {
            DirectExecutableIdentity::BundleExecutor { content_hash, .. }
            | DirectExecutableIdentity::CapturedContent { content_hash } => content_hash,
            DirectExecutableIdentity::NodePolicy => {
                anyhow::bail!("external direct projection refuses ambient executable authority")
            }
        };
        ensure!(
            expected_hash == executable_blob_hash,
            "external direct command contradicts its artifact identity"
        );
        let program = Self {
            execution_plan_hash: execution_plan_hash.clone(),
            execution_closure_digest: lillux::sha256_hex(
                lillux::canonical_json(&serde_json::to_value(closure)?)?.as_bytes(),
            ),
            command: command.clone(),
            projection,
            guest_input_identity: inputs.identity_digest()?,
        };
        program.validate_guest_inputs(inputs)?;
        Ok(program)
    }

    pub fn execution_plan_hash(&self) -> &str {
        &self.execution_plan_hash
    }
    pub fn execution_closure_digest(&self) -> &str {
        &self.execution_closure_digest
    }
    pub fn command(&self) -> &crate::objects::AdmittedDirectCommandClosure {
        &self.command
    }
    pub fn projection(&self) -> &ExternalDirectCommandProjection {
        &self.projection
    }
    pub fn guest_input_identity(&self) -> &str {
        &self.guest_input_identity
    }

    pub fn runtime_manifest_hash(&self) -> Result<&str> {
        match &self.command {
            crate::objects::AdmittedDirectCommandClosure::RealizationMember {
                realization_manifest_hash,
                ..
            } => Ok(realization_manifest_hash),
            _ => anyhow::bail!(
                "external direct projection requires an exact realization member command"
            ),
        }
    }

    pub fn namespace_executable(&self) -> Result<String> {
        Self::guest_executable_for_command(&self.command)
    }

    /// Pure guest-coordinate projection. The retained closure path remains
    /// independently validated against ADMITTED_DIRECT_PROJECT_ROOT below.
    pub fn guest_executable_for_command(
        command: &crate::objects::AdmittedDirectCommandClosure,
    ) -> Result<String> {
        let crate::objects::AdmittedDirectCommandClosure::RealizationMember {
            realization_mount_root,
            realization_mount,
            relative_path,
            ..
        } = command
        else {
            anyhow::bail!(
                "external direct projection requires an exact realization member command"
            );
        };
        require_relative_normalized(Path::new(relative_path), "direct executable member")?;
        let executable = realization_mount_root
            .destination(Some(Path::new("/workspace")), realization_mount)?
            .join(relative_path);
        require_absolute_normalized(&executable, "direct guest executable")?;
        Ok(executable
            .to_str()
            .context("external direct executable is not UTF-8")?
            .to_owned())
    }

    pub fn validate(&self) -> Result<()> {
        for hash in [
            &self.execution_plan_hash,
            &self.execution_closure_digest,
            &self.guest_input_identity,
        ] {
            super::hash(hash)?;
        }
        self.projection.validate()?;
        let crate::objects::AdmittedDirectCommandClosure::RealizationMember {
            executable_blob_hash,
            realization_id,
            realization_manifest_hash,
            realization_mount_root,
            realization_mount,
            relative_path,
            execution_path,
        } = &self.command
        else {
            anyhow::bail!(
                "external direct projection requires an exact realization member command"
            );
        };
        super::hash(executable_blob_hash)?;
        super::hash(realization_manifest_hash)?;
        crate::external_content::products::validate_name(realization_id)?;
        require_relative_normalized(Path::new(realization_mount), "direct realization mount")?;
        require_relative_normalized(Path::new(relative_path), "direct executable member")?;
        let expected = realization_mount_root
            .destination(
                Some(Path::new(crate::objects::ADMITTED_DIRECT_PROJECT_ROOT)),
                realization_mount,
            )?
            .join(relative_path);
        ensure!(
            *execution_path == expected,
            "external direct command changed its retained execution path"
        );
        require_absolute_normalized(
            Path::new(&self.namespace_executable()?),
            "direct executable",
        )?;
        ensure!(
            self.projection.argv0 == self.namespace_executable()?,
            "external direct argv0 changed its guest executable coordinate"
        );
        ensure!(
            lillux::canonical_json(&serde_json::to_value(self)?)?.len()
                <= MAX_EXTERNAL_DIRECT_PROGRAM_BYTES,
            "external direct program exceeds its encoded bound"
        );
        Ok(())
    }

    pub fn validate_guest_inputs(
        &self,
        inputs: &ryeos_external_execution_contract::ExternalGuestInputProjection,
    ) -> Result<()> {
        use ryeos_external_execution_contract::{
            GuestMountAccess, GuestMountContentAuthority, GuestMountKind, GuestMountRole,
        };
        self.validate()?;
        inputs.validate()?;
        ensure!(
            inputs.workspace_outputs.is_none()
                && inputs.identity_digest()? == self.guest_input_identity
                && inputs.environment == self.projection.environment,
            "external direct guest inputs changed their sealed read-only identity"
        );
        let crate::objects::AdmittedDirectCommandClosure::RealizationMember {
            realization_id,
            realization_manifest_hash,
            realization_mount_root,
            realization_mount,
            ..
        } = &self.command
        else {
            unreachable!("validated realization member");
        };
        let destination =
            realization_mount_root.destination(Some(Path::new("/workspace")), realization_mount)?;
        ensure!(inputs.inputs.iter().any(|input| input.role == GuestMountRole::Product
            && input.authority_id == *realization_id && Path::new(&input.destination) == destination
            && input.kind == GuestMountKind::Directory && input.access == GuestMountAccess::ReadOnly
            && matches!(&input.content_authority, GuestMountContentAuthority::ProductManifest {manifest_hash, ..} if manifest_hash == realization_manifest_hash)),
            "external direct guest inputs lost their exact command realization");
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        canonical_value_digest(
            &serde_json::json!({"domain":"ryeos.external-direct-program.v1","program":self}),
        )
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::json;

    use super::{
        CONNECTOR_PROTOCOL, ExternalCandidateExecutionRoute, ExternalCandidateProcFilesystem,
        ExternalCandidateQualificationUse, ExternalCandidateRequirement,
        ExternalCandidateRuntimeRecipe, PROTOCOL,
    };
    use crate::external_content::products::composition::EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY;
    use crate::objects::{
        AdmittedDirectCommandClosure, AdmittedExecutionClosure, AdmittedPersistentSessionCapsule,
        AdmittedStructuredSessionProfile, EFFECTIVE_SOURCE_BINDING_SCHEMA,
        EXTERNAL_REALIZATIONS_DERIVED_KEY, EffectiveSourceClosureProjection, ExternalContentKind,
        ExternalContentMode, ExternalContentMountRoot, ExternalContentRealization,
        ExternalContentRealizationSet, PERSISTENT_SESSION_CAPSULE_KIND,
        PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION, PersistentSessionLifecycleContract,
        PersistentSessionWireContract, SOURCE_CLOSURE_DERIVED_KEY,
        admitted_direct_command_execution_path, canonical_value_digest,
    };

    pub fn fixture_requirement() -> ExternalCandidateRequirement {
        ExternalCandidateRequirement {
            schema: 6,
            protocol: PROTOCOL.into(),
            connector_protocol: CONNECTOR_PROTOCOL.into(),
            execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
            required_lifecycle_capabilities: BTreeSet::new(),
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "auxiliary".into(),
            runtime_recipe: ExternalCandidateRuntimeRecipe {
                schema: 2,
                runtime_mount_destination: "/runtime".into(),
                executable_relative_path: "bin/candidate".into(),
                argv0: "candidate".into(),
                arguments: vec![],
                cwd: "/workspace".into(),
                environment: BTreeMap::new(),
                max_stdout_bytes: 1024 * 1024,
                max_stderr_bytes: 1024 * 1024,
                proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
                contain_process_group: false,
                nested_sandbox: true,
            },
        }
    }

    pub fn fixture_profile(
        requirement: &ExternalCandidateRequirement,
    ) -> anyhow::Result<AdmittedStructuredSessionProfile> {
        let contract = json!({
            "external_candidate": requirement,
            "workload_client": null,
            "workload_realization_id": "codex",
            "auxiliary_configs": [],
            "runtime_configs": [],
        });
        Ok(AdmittedStructuredSessionProfile {
            profile_hash: canonical_value_digest(&contract)?,
            contract,
            schema_hashes: BTreeMap::from([("response.json".into(), "9".repeat(64))]),
            baseline_source: "baseline.conf".into(),
            baseline_destination: "config.conf".into(),
            auxiliary_configs: Vec::new(),
            runtime_configs: Vec::new(),
        })
    }

    pub fn fixture_source_projection() -> EffectiveSourceClosureProjection {
        EffectiveSourceClosureProjection {
            schema: EFFECTIVE_SOURCE_BINDING_SCHEMA,
            binding_hash: "b".repeat(64),
            content_manifest_hash: "c".repeat(64),
            owner_key: "d".repeat(64),
            file_count: 1,
            total_bytes: 1,
        }
    }

    pub fn fixture_provider_realizations() -> anyhow::Result<ExternalContentRealizationSet> {
        ExternalContentRealizationSet::new(vec![ExternalContentRealization {
            id: "codex".into(),
            kind: ExternalContentKind::File,
            mode: ExternalContentMode::Pinned,
            manifest_hash: "e".repeat(64),
            entry_count: 1,
            total_bytes: 1,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "codex".into(),
        }])
    }

    pub fn fixture_qualification_use(
        requirement: &ExternalCandidateRequirement,
    ) -> anyhow::Result<ExternalCandidateQualificationUse> {
        ExternalCandidateQualificationUse::from_admitted_inputs(
            requirement,
            &fixture_profile(requirement)?,
            &fixture_source_projection(),
            &fixture_provider_realizations()?,
            &[],
            &BTreeMap::new(),
        )
    }

    pub fn qualified_external_candidate_capsule(
        requirement: ExternalCandidateRequirement,
        runtime_manifest_hash: &str,
    ) -> anyhow::Result<AdmittedPersistentSessionCapsule> {
        requirement.validate()?;
        let profile = fixture_profile(&requirement)?;
        let source = fixture_source_projection();
        let realizations = fixture_provider_realizations()?;
        let qualification_use = ExternalCandidateQualificationUse::from_admitted_inputs(
            &requirement,
            &profile,
            &source,
            &realizations,
            &[],
            &BTreeMap::new(),
        )?;
        let selections = qualified_external_candidate_selections(
            runtime_manifest_hash,
            &requirement,
            &qualification_use,
        )?;
        let program = requirement.resolve_for_use(Some(&selections), &qualification_use)?;
        let exact_program = json!({"resolution_output":{"composed":{"derived":{
            (EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY): selections.semantic_identity_value()?,
            (SOURCE_CLOSURE_DERIVED_KEY): source.to_value()?,
            (EXTERNAL_REALIZATIONS_DERIVED_KEY): realizations.to_value()?
        }}}});
        let artifact = crate::external_content::products::qualification::test_support::launch_artifact_identity();
        let executable_blob_hash = "6".repeat(64);
        let capsule = AdmittedPersistentSessionCapsule {
            schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
            kind: PERSISTENT_SESSION_CAPSULE_KIND.into(),
            external_candidate: Some(program),
            exact_program_hash: canonical_value_digest(&exact_program)?,
            exact_program,
            retained_product_selections: Some(selections),
            retained_external_runtime_qualification: None,
            lifecycle: PersistentSessionLifecycleContract {
                max_processes: 1,
                max_inflight_per_process: 1,
                max_address_space_bytes: 64 * 1024 * 1024,
                max_cpu_seconds: 60,
                real_uid_process_limit: 8,
                ready_timeout_ms: 10_000,
                request_timeout_ms: 10_000,
                idle_timeout_ms: 10_000,
            },
            wire: PersistentSessionWireContract {
                channel_env: "RYEOS_SESSION_FD".into(),
                wire_protocol: "ryeos.structured-session".into(),
                wire_version: 2,
                max_frame_bytes: 1024 * 1024,
            },
            executor_ref: artifact.executor_ref().into(),
            artifact_identity: artifact,
            execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
                execution_plan: json!({}),
                protocol_descriptor_document: "external candidate composed fixture".into(),
                command: AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash: executable_blob_hash.clone(),
                    execution_path: admitted_direct_command_execution_path(
                        &executable_blob_hash,
                        std::path::Path::new("fixture"),
                    )?,
                },
                admitted_project_root: None,
            },
            execution_realization_hash: "8".repeat(64),
            source_binding_hash: Some(source.binding_hash),
            structured_session_profile: Some(profile),
            executable_search: Vec::new(),
            process_environment: BTreeMap::new(),
            runtime_ref: "runtime:fixtures/qualified".into(),
        };
        capsule.validate()?;
        Ok(capsule)
    }

    pub fn qualified_external_candidate_selections(
        runtime_manifest_hash: &str,
        requirement: &ExternalCandidateRequirement,
        qualification_use: &ExternalCandidateQualificationUse,
    ) -> anyhow::Result<
        crate::external_content::products::composition::ResolvedExternalProductSelections,
    > {
        let selections = crate::external_content::products::qualification::test_support::qualified_runtime_selections(
            runtime_manifest_hash,
        )?;
        let mut selections = selections.into_inner();
        let mut runtime = selections
            .remove(&requirement.runtime_product_declaration_id)
            .ok_or_else(|| anyhow::anyhow!("qualified fixture omits requested runtime product"))?;
        let claims: Vec<String> = super::REQUIRED_CLAIMS
            .iter()
            .map(|claim| (*claim).to_owned())
            .collect();
        runtime.relationship.qualification.required_claims = claims.clone();
        let proof = &mut runtime
            .qualification
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("qualified fixture runtime omits qualification"))?
            .evidence;
        proof.policy_source.policy.allowed_claims = claims.clone();
        anyhow::ensure!(
            qualification_use.requirement_digest
                == requirement.qualification_requirement_digest()?,
            "fixture qualification use differs from requirement"
        );
        proof.policy_source.policy.verifier_parameters = qualification_use.parameters()?;
        proof.verifier.admitted_parameters_digest =
            proof.policy_source.policy.admitted_parameters_digest()?;
        proof.result.claims = claims;
        proof.verifier.result_digest = proof.result.digest()?;
        crate::external_content::products::composition::ResolvedExternalProductSelections::new(
            BTreeMap::from([(requirement.runtime_product_declaration_id.clone(), runtime)]),
        )
    }

    pub fn external_candidate_program(
        runtime_recipe: ExternalCandidateRuntimeRecipe,
        runtime_manifest_hash: &str,
    ) -> anyhow::Result<super::AdmittedExternalCandidateProgram> {
        let requirement = ExternalCandidateRequirement {
            schema: 6,
            protocol: super::PROTOCOL.into(),
            connector_protocol: super::CONNECTOR_PROTOCOL.into(),
            execution_route: super::ExternalCandidateExecutionRoute::ConnectorOnly,
            required_lifecycle_capabilities: BTreeSet::new(),
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "auxiliary".into(),
            runtime_recipe,
        };
        let capsule = qualified_external_candidate_capsule(requirement, runtime_manifest_hash)?;
        Ok(capsule
            .external_candidate
            .expect("qualified fixture capsule retains its external program"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_fixture() -> (
        crate::objects::AdmittedExecutionClosure,
        crate::objects::AdmittedLaunchArtifactIdentity,
        ExternalDirectCommandProjection,
        ryeos_external_execution_contract::ExternalGuestInputProjection,
    ) {
        use crate::objects::*;
        use ryeos_external_execution_contract::*;
        let execution_plan = serde_json::json!({"opaque_engine_plan_fixture":true});
        let command = AdmittedDirectCommandClosure::RealizationMember {
            executable_blob_hash: "a".repeat(64),
            realization_id: "runtime".into(),
            realization_manifest_hash: "b".repeat(64),
            realization_mount_root: ExternalContentMountRoot::ExecutionRuntime,
            realization_mount: "runtime".into(),
            relative_path: "bin/evaluate".into(),
            execution_path: "/ryeos/realizations/runtime/bin/evaluate".into(),
        };
        let artifact = AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executor_ref: "tool:fixture/evaluate".into(),
            root_subject_source_content_digest: "c".repeat(64),
            root_subject_signer_fingerprint: Some("d".repeat(64)),
            root_subject_source_identity: DirectRootSourceIdentity::Project,
            protocol_ref: "protocol:fixture/opaque".into(),
            protocol_content_hash: "e".repeat(64),
            protocol_signer_fingerprint: "f".repeat(64),
            execution_plan_hash: lillux::sha256_hex(
                lillux::canonical_json(&execution_plan).unwrap().as_bytes(),
            ),
            executable_identity: DirectExecutableIdentity::CapturedContent {
                content_hash: "a".repeat(64),
            },
            runtime_identity: DirectRuntimeIdentity {
                runtime_ref: "runtime:fixture/direct".into(),
                runtime_source_space: DirectRuntimeSourceSpace::Project,
                runtime_content_hash: "1".repeat(64),
                runtime_signer_fingerprint: "2".repeat(64),
                runtime_bundle_manifest_hash: None,
                runtime_bundle_signer_fingerprint: None,
            },
        };
        let closure = AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            protocol_descriptor_document:
                "opaque descriptor fixture; no production signature claim".into(),
            command,
            admitted_project_root: Some(ADMITTED_DIRECT_PROJECT_ROOT.into()),
        };
        let inputs = ExternalGuestInputProjection {
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 56,
                snapshot_hash: "3".repeat(64),
                closure_digest: "4".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![GuestMountInput {
                role: GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: 64,
                destination: "/ryeos/realizations/runtime".into(),
                kind: GuestMountKind::Directory,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: None,
                content_authority: GuestMountContentAuthority::ProductManifest {
                    manifest_kind: GuestProductManifestKind::Content,
                    manifest_hash: "b".repeat(64),
                    manifest_descriptor: 65,
                    manifest_bytes: 256,
                },
                bytes: 1,
            }],
            executable_search: vec![],
            environment: BTreeMap::new(),
        };
        let projection = ExternalDirectCommandProjection {
            argv0: "/ryeos/realizations/runtime/bin/evaluate".into(),
            arguments: vec!["--check".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            stdin: ExternalDirectSealedInput::from_bytes(b"exact input\n").unwrap(),
            endpoint_binding_id: "farm-direct".into(),
            endpoint_binding_digest: "5".repeat(64),
            execution_mode: ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: 1024,
                stderr_max_bytes: 1024,
            },
            timeout_seconds: 30,
            native: ExternalDirectNativeRequirements::LinuxIsolatedReadOnly {
                required_arch: "x86_64".into(),
            },
        };
        (closure, artifact, projection, inputs)
    }

    #[test]
    fn direct_command_separates_retained_and_guest_project_coordinates() {
        use crate::objects::{
            AdmittedDirectCommandClosure, AdmittedExecutionClosure, ExternalContentMountRoot,
        };
        for (root, retained_mount, guest_mount) in [
            (
                ExternalContentMountRoot::Project,
                "/ryeos/admitted-project/runtime",
                "/workspace/runtime",
            ),
            (
                ExternalContentMountRoot::ExecutionRuntime,
                "/ryeos/realizations/runtime",
                "/ryeos/realizations/runtime",
            ),
        ] {
            let (mut closure, artifact, mut projection, mut inputs) = direct_fixture();
            let AdmittedExecutionClosure::DirectItemExecutor { command, .. } = &mut closure else {
                unreachable!()
            };
            let AdmittedDirectCommandClosure::RealizationMember {
                realization_mount_root,
                execution_path,
                ..
            } = command
            else {
                unreachable!()
            };
            *realization_mount_root = root;
            *execution_path = Path::new(retained_mount).join("bin/evaluate");
            projection.argv0 = format!("{guest_mount}/bin/evaluate");
            inputs.inputs[0].destination = guest_mount.into();
            let program = AdmittedExternalDirectProgram::from_compiled_closure(
                &closure,
                &artifact,
                projection.clone(),
                &inputs,
            )
            .unwrap();
            assert_eq!(program.namespace_executable().unwrap(), projection.argv0);
            if retained_mount != guest_mount {
                // A serialized predecessor with identity paths used as guest
                // coordinates must refuse, never be silently reinterpreted.
                let mut predecessor = program.clone();
                let mut predecessor_inputs = inputs.clone();
                predecessor_inputs.inputs[0].destination = retained_mount.into();
                predecessor.guest_input_identity = predecessor_inputs.identity_digest().unwrap();
                predecessor.projection.argv0 = format!("{retained_mount}/bin/evaluate");
                let mut restored: AdmittedExternalDirectProgram =
                    serde_json::from_value(serde_json::to_value(predecessor).unwrap()).unwrap();
                assert!(restored.validate().is_err());
                restored.projection.argv0 = projection.argv0.clone();
                assert!(restored.validate_guest_inputs(&predecessor_inputs).is_err());
            }
            for mutation in ["mount", "argv0", "retained_path"] {
                let mut changed = program.clone();
                let mut changed_inputs = inputs.clone();
                match mutation {
                    "mount" => {
                        changed_inputs.inputs[0].destination = "/wrong/runtime".into();
                        // Recompute to test the coordinate join, not merely the
                        // outer guest-input identity mismatch.
                        changed.guest_input_identity = changed_inputs.identity_digest().unwrap();
                    }
                    "argv0" => changed.projection.argv0 = "/wrong/bin/evaluate".into(),
                    "retained_path" => {
                        let AdmittedDirectCommandClosure::RealizationMember {
                            execution_path, ..
                        } = &mut changed.command
                        else {
                            unreachable!()
                        };
                        *execution_path = "/wrong/bin/evaluate".into();
                    }
                    _ => unreachable!(),
                }
                assert!(
                    changed.validate_guest_inputs(&changed_inputs).is_err(),
                    "accepted {mutation}"
                );
            }
        }
    }

    #[test]
    fn direct_program_joins_ordinary_closure_artifact_and_exact_guest_inputs() {
        let (closure, artifact, projection, inputs) = direct_fixture();
        let program = AdmittedExternalDirectProgram::from_compiled_closure(
            &closure,
            &artifact,
            projection.clone(),
            &inputs,
        )
        .unwrap();
        program.validate_guest_inputs(&inputs).unwrap();
        assert_eq!(
            program.namespace_executable().unwrap(),
            "/ryeos/realizations/runtime/bin/evaluate"
        );
        assert_eq!(
            program.projection().stdin.decoded_bytes().unwrap(),
            b"exact input\n"
        );
        let wire = serde_json::to_value(AdmittedExternalExecutionProgram::DirectCommand(
            program.clone(),
        ))
        .unwrap();
        let restored: AdmittedExternalExecutionProgram =
            serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(restored.digest().unwrap(), program.digest().unwrap());
        assert!(restored.worker().is_err());
        for path in [
            "execution_plan_hash",
            "execution_closure_digest",
            "command",
            "projection",
            "guest_input_identity",
        ] {
            let mut missing = wire.clone();
            missing["program"].as_object_mut().unwrap().remove(path);
            assert!(serde_json::from_value::<AdmittedExternalExecutionProgram>(missing).is_err());
        }
        let mut unknown = wire;
        unknown["program"]["recipe"] = serde_json::json!({});
        assert!(serde_json::from_value::<AdmittedExternalExecutionProgram>(unknown).is_err());
        let mut changed = closure.clone();
        if let crate::objects::AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan, ..
        } = &mut changed
        {
            execution_plan["changed"] = true.into();
        }
        assert!(
            AdmittedExternalDirectProgram::from_compiled_closure(
                &changed,
                &artifact,
                projection.clone(),
                &inputs
            )
            .is_err()
        );
        let mut changed = closure.clone();
        if let crate::objects::AdmittedExecutionClosure::DirectItemExecutor { command, .. } =
            &mut changed
        {
            *command = crate::objects::AdmittedDirectCommandClosure::NodePolicy;
        }
        assert!(
            AdmittedExternalDirectProgram::from_compiled_closure(
                &changed,
                &artifact,
                projection.clone(),
                &inputs
            )
            .is_err()
        );
        for mutation in ["manifest", "mount", "environment", "access"] {
            let mut changed = inputs.clone();
            match mutation {
                "manifest" => if let ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest{manifest_hash,..}=&mut changed.inputs[0].content_authority { *manifest_hash="6".repeat(64); },
                "mount" => changed.inputs[0].destination="/other".into(),
                "environment" => { changed.environment.insert("EXTRA".into(),"value".into()); },
                "access" => changed.inputs[0].access=ryeos_external_execution_contract::GuestMountAccess::PrivateWritable,
                _=>unreachable!(),
            }
            assert!(
                program.validate_guest_inputs(&changed).is_err(),
                "accepted {mutation}"
            );
            assert!(
                AdmittedExternalDirectProgram::from_compiled_closure(
                    &closure,
                    &artifact,
                    projection.clone(),
                    &changed
                )
                .is_err(),
                "rebound {mutation}"
            );
        }
        let mut renumbered = inputs.clone();
        renumbered.base_snapshot.descriptor = 100;
        renumbered.inputs[0].descriptor = 101;
        if let ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
            manifest_descriptor,
            ..
        } = &mut renumbered.inputs[0].content_authority
        {
            *manifest_descriptor = 102;
        }
        program.validate_guest_inputs(&renumbered).unwrap();
        // State only validates transport identity: it does not interpret the
        // opaque engine plan or attest the compiler's argv projection.
        let mut changed = projection;
        changed.arguments.push("different compiler output".into());
        let different = AdmittedExternalDirectProgram::from_compiled_closure(
            &closure, &artifact, changed, &inputs,
        )
        .unwrap();
        assert_ne!(different.digest().unwrap(), program.digest().unwrap());
    }

    #[test]
    fn direct_input_and_native_projection_are_closed_canonical_and_bounded() {
        for bytes in [vec![], vec![0; MAX_EXTERNAL_DIRECT_STDIN_BYTES]] {
            let input = ExternalDirectSealedInput::from_bytes(&bytes).unwrap();
            assert_eq!(input.decoded_bytes().unwrap(), bytes);
        }
        assert!(
            ExternalDirectSealedInput::from_bytes(&vec![0; MAX_EXTERNAL_DIRECT_STDIN_BYTES + 1])
                .is_err()
        );
        let (_, _, projection, _) = direct_fixture();
        for mutation in ["oversized", "digest", "noncanonical", "length"] {
            let mut input = projection.stdin.clone();
            match mutation {
                "oversized" => input.bytes_base64 = "?".repeat(MAX_EXTERNAL_DIRECT_STDIN_BYTES * 2),
                "digest" => input.sha256 = "0".repeat(64),
                "noncanonical" => input.bytes_base64.push('\n'),
                "length" => input.bytes += 1,
                _ => unreachable!(),
            }
            assert!(input.validate().is_err());
        }
        for timeout in [0, 3601, u64::MAX] {
            let mut changed = projection.clone();
            changed.timeout_seconds = timeout;
            assert!(changed.validate().is_err());
        }
        let mut changed = projection.clone();
        changed.native = ExternalDirectNativeRequirements::LinuxIsolatedReadOnly {
            required_arch: "ambient".into(),
        };
        assert!(changed.validate().is_err());
        let mut changed = projection;
        changed.execution_mode =
            ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {};
        assert!(changed.validate().is_err());
    }

    fn runtime_recipe() -> ExternalCandidateRuntimeRecipe {
        ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::from([("LANG".into(), "C.UTF-8".into())]),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        }
    }

    fn requirement() -> ExternalCandidateRequirement {
        ExternalCandidateRequirement {
            schema: 6,
            protocol: PROTOCOL.into(),
            connector_protocol: CONNECTOR_PROTOCOL.into(),
            execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
            required_lifecycle_capabilities: BTreeSet::new(),
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "auxiliary".into(),
            runtime_recipe: runtime_recipe(),
        }
    }

    fn selections() -> ResolvedExternalProductSelections {
        let evidence = crate::external_content::products::qualification::tests::dynamic_evidence_with_auxiliary_proof();
        let mut selections = evidence.verifier_root_selections.unwrap().into_inner();
        let runtime = selections.get_mut("auxiliary").unwrap();
        let claims: Vec<String> = REQUIRED_CLAIMS
            .iter()
            .map(|claim| (*claim).into())
            .collect();
        runtime.relationship.qualification.required_claims = claims.clone();
        let proof = &mut runtime.qualification.as_mut().unwrap().evidence;
        proof.policy_source.policy.allowed_claims = claims.clone();
        proof.policy_source.policy.verifier_parameters =
            test_support::fixture_qualification_use(&requirement())
                .unwrap()
                .parameters()
                .unwrap();
        proof.verifier.admitted_parameters_digest = proof
            .policy_source
            .policy
            .admitted_parameters_digest()
            .unwrap();
        proof.result.claims = claims;
        proof.verifier.result_digest = proof.result.digest().unwrap();
        ResolvedExternalProductSelections::new(selections).unwrap()
    }

    #[test]
    fn external_program_requires_exact_qualified_runtime() {
        let requirement = requirement();
        let selections = selections();
        let qualification_use = test_support::fixture_qualification_use(&requirement).unwrap();
        let program = requirement
            .resolve_for_use(Some(&selections), &qualification_use)
            .unwrap();
        assert_eq!(
            program.selection_identity_digest,
            selections
                .get(&requirement.runtime_product_declaration_id)
                .unwrap()
                .semantic_identity_digest()
                .unwrap()
        );
        program.verify_selections(Some(&selections)).unwrap();
        assert!(
            requirement
                .resolve_for_use(None, &qualification_use)
                .is_err()
        );
        let mut missing = requirement.clone();
        missing.runtime_product_declaration_id = "absent".into();
        assert!(
            missing
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut unqualified = requirement.clone();
        unqualified.runtime_product_declaration_id = "runtime".into();
        assert!(
            unqualified
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut weaker = selections.clone().into_inner();
        weaker
            .get_mut("auxiliary")
            .unwrap()
            .relationship
            .qualification
            .required_claims
            .pop();
        let weaker = ResolvedExternalProductSelections::new(weaker).unwrap();
        assert!(
            requirement
                .resolve_for_use(Some(&weaker), &qualification_use)
                .is_err()
        );
        for field in [
            "runtime_manifest_kind",
            "runtime_manifest_hash",
            "runtime_witness_hash",
            "qualification_attestation_hash",
            "selection_identity_digest",
            "runtime_recipe_digest",
        ] {
            let mut wire = serde_json::to_value(&program).unwrap();
            wire[field] = serde_json::json!("0".repeat(64));
            let changed: AdmittedExternalCandidateProgram = serde_json::from_value(wire).unwrap();
            assert!(
                changed.verify_selections(Some(&selections)).is_err(),
                "{field}"
            );
        }
        let mut changed = requirement.clone();
        changed.runtime_recipe.arguments.push("--changed".into());
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut program = requirement
            .resolve_for_use(Some(&selections), &qualification_use)
            .unwrap();
        program.requirement = changed;
        assert!(program.validate().is_err());
        let mut changed = requirement.clone();
        changed.runtime_recipe.cwd = "/workspace/changed".into();
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut changed = requirement.clone();
        changed
            .runtime_recipe
            .environment
            .insert("SCENARIO".into(), "changed".into());
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut changed = requirement.clone();
        changed.runtime_recipe.proc_filesystem = ExternalCandidateProcFilesystem::PidNamespace;
        changed.runtime_recipe.contain_process_group = true;
        changed.runtime_recipe.nested_sandbox = false;
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut changed = requirement.clone();
        changed.runtime_recipe.max_stdout_bytes += 1;
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut changed = requirement.clone();
        changed.runtime_recipe.max_stderr_bytes += 1;
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut changed = requirement.clone();
        changed.runtime_recipe.executable_relative_path = "bin/other".into();
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut changed = requirement.clone();
        changed.provider_configuration_destination = "other-environments.toml".into();
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut changed = requirement.clone();
        changed.provider_declaration_id = "other-provider".into();
        assert!(
            changed
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let mut without_context = selections.clone().into_inner();
        let proof = &mut without_context
            .get_mut("auxiliary")
            .unwrap()
            .qualification
            .as_mut()
            .unwrap()
            .evidence;
        proof.policy_source.policy.verifier_parameters = serde_json::json!({});
        proof.verifier.admitted_parameters_digest = proof
            .policy_source
            .policy
            .admitted_parameters_digest()
            .unwrap();
        let without_context = ResolvedExternalProductSelections::new(without_context).unwrap();
        assert!(
            requirement
                .resolve_for_use(Some(&without_context), &qualification_use)
                .is_err()
        );
        for invalid in [
            serde_json::json!({"schema":"unknown","requirement_digest":"a".repeat(64)}),
            serde_json::json!({"schema":QUALIFICATION_CONTEXT_SCHEMA,"requirement_digest":"A".repeat(64)}),
            serde_json::json!({"schema":QUALIFICATION_CONTEXT_SCHEMA,"requirement_digest":"a".repeat(64),"extra":true}),
        ] {
            assert!(ExternalCandidateQualificationUse::from_value(&invalid).is_err());
        }
    }

    #[test]
    fn qualification_use_moves_with_profile_source_and_provider_realization() {
        let requirement = requirement();
        let profile = test_support::fixture_profile(&requirement).unwrap();
        let source = test_support::fixture_source_projection();
        let realizations = test_support::fixture_provider_realizations().unwrap();
        let original = ExternalCandidateQualificationUse::from_admitted_inputs(
            &requirement,
            &profile,
            &source,
            &realizations,
            &[],
            &BTreeMap::new(),
        )
        .unwrap();
        let selections = selections();
        requirement
            .resolve_for_use(Some(&selections), &original)
            .unwrap();

        let mut changed_profile = profile.clone();
        changed_profile.contract["feature_marker"] = serde_json::json!("changed");
        changed_profile.profile_hash = canonical_value_digest(&changed_profile.contract).unwrap();
        let changed = ExternalCandidateQualificationUse::from_admitted_inputs(
            &requirement,
            &changed_profile,
            &source,
            &realizations,
            &[],
            &BTreeMap::new(),
        )
        .unwrap();
        assert_ne!(changed.profile_hash, original.profile_hash);
        let error = requirement
            .resolve_for_use(Some(&selections), &changed)
            .unwrap_err();
        assert!(error.to_string().contains("different admitted use: profile"));

        for field in ["binding_hash", "content_manifest_hash"] {
            let mut changed_source = source.clone();
            match field {
                "binding_hash" => changed_source.binding_hash = "f".repeat(64),
                "content_manifest_hash" => changed_source.content_manifest_hash = "f".repeat(64),
                _ => unreachable!(),
            }
            let changed = ExternalCandidateQualificationUse::from_admitted_inputs(
                &requirement,
                &profile,
                &changed_source,
                &realizations,
                &[],
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(changed.profile_hash, original.profile_hash);
            assert!(
                requirement
                    .resolve_for_use(Some(&selections), &changed)
                    .is_err()
            );
        }

        let mut changed_realizations = realizations.to_value().unwrap();
        changed_realizations[0]["manifest_hash"] = serde_json::json!("f".repeat(64));
        let changed_realizations =
            ExternalContentRealizationSet::from_value(&changed_realizations).unwrap();
        let changed = ExternalCandidateQualificationUse::from_admitted_inputs(
            &requirement,
            &profile,
            &source,
            &changed_realizations,
            &[],
            &BTreeMap::new(),
        )
        .unwrap();
        assert_ne!(
            changed.provider_executable_manifest_hash,
            original.provider_executable_manifest_hash
        );
        assert!(
            requirement
                .resolve_for_use(Some(&selections), &changed)
                .is_err()
        );

        for field in [
            "requirement_digest",
            "profile_hash",
            "source_binding_hash",
            "source_content_manifest_hash",
            "provider_executable_manifest_hash",
            "execution_environment_digest",
        ] {
            let mut value = serde_json::to_value(&original).unwrap();
            value[field] = serde_json::json!("A".repeat(64));
            assert!(
                ExternalCandidateQualificationUse::from_value(&value).is_err(),
                "{field}"
            );
        }
        let mut predecessor = serde_json::to_value(&original).unwrap();
        predecessor["schema"] =
            serde_json::json!("ryeos.external_candidate_qualification_context.v1");
        assert!(ExternalCandidateQualificationUse::from_value(&predecessor).is_err());
    }

    #[test]
    fn qualification_use_refuses_changed_guest_environment_search_and_tools() {
        let requirement = requirement();
        let profile = test_support::fixture_profile(&requirement).unwrap();
        let source = test_support::fixture_source_projection();
        let provider = test_support::fixture_provider_realizations().unwrap();
        let mut entries = provider.iter().cloned().collect::<Vec<_>>();
        entries.push(crate::objects::ExternalContentRealization {
            id: "authoring-tools".into(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Pinned,
            manifest_hash: "1".repeat(64),
            entry_count: 2,
            total_bytes: 2,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "authoring-tools".into(),
        });
        let realizations = ExternalContentRealizationSet::new(entries).unwrap();
        let search = vec![ExecutableSearchPathEntry {
            realization_id: "authoring-tools".into(),
            relative_directory: "bin".into(),
        }];
        let environment = BTreeMap::from([(
            "TZ".into(),
            SessionProcessEnvironmentValue::Literal {
                value: "UTC".into(),
            },
        )]);
        let derive =
            |realizations: &ExternalContentRealizationSet,
             search: &[ExecutableSearchPathEntry],
             environment: &BTreeMap<String, SessionProcessEnvironmentValue>| {
                ExternalCandidateQualificationUse::from_admitted_inputs(
                    &requirement,
                    &profile,
                    &source,
                    realizations,
                    search,
                    environment,
                )
                .unwrap()
            };
        let original = derive(&realizations, &search, &environment);
        assert_eq!(original, derive(&realizations, &search, &environment));

        let changed_search = vec![ExecutableSearchPathEntry {
            realization_id: "authoring-tools".into(),
            relative_directory: ".".into(),
        }];
        let changed_environment = BTreeMap::from([(
            "TZ".into(),
            SessionProcessEnvironmentValue::Literal {
                value: "Pacific/Auckland".into(),
            },
        )]);
        let mut changed_tools = realizations.to_value().unwrap();
        changed_tools[0]["manifest_hash"] = serde_json::json!("2".repeat(64));
        let changed_tools = ExternalContentRealizationSet::from_value(&changed_tools).unwrap();
        for changed in [
            derive(&realizations, &changed_search, &environment),
            derive(&realizations, &search, &changed_environment),
            derive(&changed_tools, &search, &environment),
        ] {
            assert_eq!(
                changed.provider_executable_manifest_hash,
                original.provider_executable_manifest_hash
            );
            assert_ne!(
                changed.execution_environment_digest,
                original.execution_environment_digest
            );
            assert!(
                requirement
                    .resolve_for_use(Some(&selections()), &changed)
                    .is_err()
            );
        }
    }

    #[test]
    fn retained_candidate_capsule_refuses_reused_qualification_after_environment_drift() {
        let requirement = requirement();
        let mut capsule =
            test_support::qualified_external_candidate_capsule(requirement, &"9".repeat(64))
                .unwrap();
        capsule.validate().unwrap();
        capsule.process_environment.insert(
            "TZ".into(),
            SessionProcessEnvironmentValue::Literal {
                value: "UTC".into(),
            },
        );
        assert!(capsule.validate().is_err());
    }

    #[test]
    fn guest_environment_derives_exact_signed_values_without_ambient_inheritance() {
        let requirement = test_support::fixture_requirement();
        let recipe = requirement.runtime_recipe.clone();
        let search = vec![ExecutableSearchPathEntry {
            realization_id: "authoring-tools".into(),
            relative_directory: "bin".into(),
        }];
        let bindings = BTreeMap::from([
            (
                "GIT_CONFIG_NOSYSTEM".into(),
                SessionProcessEnvironmentValue::Literal { value: "1".into() },
            ),
            (
                "TMPDIR".into(),
                SessionProcessEnvironmentValue::RuntimeViewDirectory {
                    relative_path: "scratch".into(),
                },
            ),
            (
                "TZ".into(),
                SessionProcessEnvironmentValue::Literal {
                    value: "UTC".into(),
                },
            ),
        ]);
        let realizations = ExternalContentRealizationSet::new(vec![
            crate::objects::ExternalContentRealization {
                id: "auxiliary".into(),
                kind: ExternalContentKind::Tree,
                mode: ExternalContentMode::Pinned,
                manifest_hash: "a".repeat(64),
                entry_count: 1,
                total_bytes: 1,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "guest-runtime".into(),
            },
            crate::objects::ExternalContentRealization {
                id: "authoring-tools".into(),
                kind: ExternalContentKind::Tree,
                mode: ExternalContentMode::Pinned,
                manifest_hash: "b".repeat(64),
                entry_count: 1,
                total_bytes: 1,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "authoring-tools".into(),
            },
        ])
        .unwrap();
        let destinations =
            ExternalCandidateGuestEnvironment::expected_destinations(&requirement, &realizations)
                .unwrap();
        assert_eq!(destinations["auxiliary"], "/runtime");
        assert_eq!(
            destinations["authoring-tools"],
            "/ryeos/realizations/authoring-tools"
        );
        let derived =
            ExternalCandidateGuestEnvironment::derive(&recipe, &bindings, &search, &destinations)
                .unwrap();
        assert_eq!(
            derived.executable_search,
            ["/ryeos/realizations/authoring-tools/bin"]
        );
        assert_eq!(
            derived.environment,
            BTreeMap::from([
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("TMPDIR".into(), "/ryeos/runtime-views/TMPDIR".into()),
                ("TZ".into(), "UTC".into()),
                (
                    "PATH".into(),
                    "/ryeos/realizations/authoring-tools/bin".into(),
                ),
            ])
        );
        assert!(
            ExternalCandidateGuestEnvironment::derive(
                &recipe,
                &bindings,
                &search,
                &BTreeMap::new(),
            )
            .is_err()
        );
        let mut collision = recipe.clone();
        collision.environment.insert("TZ".into(), "ambient".into());
        assert!(
            ExternalCandidateGuestEnvironment::derive(
                &collision,
                &bindings,
                &search,
                &destinations,
            )
            .is_err()
        );
        let duplicated = [search[0].clone(), search[0].clone()];
        assert!(
            ExternalCandidateGuestEnvironment::derive(
                &recipe,
                &bindings,
                &duplicated,
                &destinations,
            )
            .is_err()
        );
    }

    #[test]
    fn qualified_runtime_identity_changes_with_qualification_attestation() {
        let requirement = requirement();
        let original = selections();
        let qualification_use = test_support::fixture_qualification_use(&requirement).unwrap();
        let program = requirement
            .resolve_for_use(Some(&original), &qualification_use)
            .unwrap();
        let mut changed = original.into_inner();
        changed
            .get_mut(&requirement.runtime_product_declaration_id)
            .unwrap()
            .qualification
            .as_mut()
            .unwrap()
            .attestation_hash = "f".repeat(64);
        let changed = ResolvedExternalProductSelections::new(changed).unwrap();
        let replacement = requirement
            .resolve_for_use(Some(&changed), &qualification_use)
            .unwrap();
        assert_ne!(
            program.selection_identity_digest,
            replacement.selection_identity_digest
        );
        assert!(program.verify_selections(Some(&changed)).is_err());
    }

    #[test]
    fn lifecycle_requirements_are_explicit_closed_and_schema_current() {
        let wire = serde_json::to_value(requirement()).unwrap();
        assert_eq!(
            wire["required_lifecycle_capabilities"],
            serde_json::json!([])
        );
        serde_json::from_value::<ExternalCandidateRequirement>(wire.clone())
            .unwrap()
            .validate()
            .unwrap();

        let mut missing = wire.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("required_lifecycle_capabilities");
        assert!(serde_json::from_value::<ExternalCandidateRequirement>(missing).is_err());
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!("exact_allocation_reconciliation"),
            serde_json::json!(["unknown_lifecycle_capability"]),
        ] {
            let mut changed = wire.clone();
            changed["required_lifecycle_capabilities"] = invalid;
            assert!(serde_json::from_value::<ExternalCandidateRequirement>(changed).is_err());
        }
        let mut unknown = wire.clone();
        unknown["lifecycle_capabilities"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ExternalCandidateRequirement>(unknown).is_err());
        let mut predecessor = wire;
        predecessor["schema"] = serde_json::json!(5);
        assert!(
            serde_json::from_value::<ExternalCandidateRequirement>(predecessor)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn lifecycle_requirements_move_retained_program_and_profile_identity() {
        let requirement = requirement();
        let selections = selections();
        let qualification_use = test_support::fixture_qualification_use(&requirement).unwrap();
        let program = requirement
            .resolve_for_use(Some(&selections), &qualification_use)
            .unwrap();
        let capsule = test_support::qualified_external_candidate_capsule(
            requirement.clone(),
            &"a".repeat(64),
        )
        .unwrap();
        let mut reconciled = requirement;
        reconciled
            .required_lifecycle_capabilities
            .insert(LifecycleCapability::ExactAllocationReconciliation);
        assert!(
            reconciled
                .resolve_for_use(Some(&selections), &qualification_use)
                .is_err()
        );
        let reconciled_use = test_support::fixture_qualification_use(&reconciled).unwrap();
        let reconciled_selections = test_support::qualified_external_candidate_selections(
            &"a".repeat(64),
            &reconciled,
            &reconciled_use,
        )
        .unwrap();
        let reconciled_program = reconciled
            .resolve_for_use(Some(&reconciled_selections), &reconciled_use)
            .unwrap();
        let reconciled_capsule =
            test_support::qualified_external_candidate_capsule(reconciled, &"a".repeat(64))
                .unwrap();
        assert_ne!(
            program.digest().unwrap(),
            reconciled_program.digest().unwrap()
        );
        assert_ne!(
            capsule
                .structured_session_profile
                .as_ref()
                .unwrap()
                .profile_hash,
            reconciled_capsule
                .structured_session_profile
                .as_ref()
                .unwrap()
                .profile_hash
        );
        assert_ne!(
            capsule.external_candidate,
            reconciled_capsule.external_candidate
        );
        assert_eq!(
            program.runtime_recipe_digest,
            reconciled_program.runtime_recipe_digest
        );
        assert_ne!(
            program.selection_identity_digest,
            reconciled_program.selection_identity_digest
        );
    }

    #[test]
    fn large_runtime_manifest_tier_moves_the_admitted_program_identity() {
        let requirement = requirement();
        let ordinary = selections();
        let qualification_use = test_support::fixture_qualification_use(&requirement).unwrap();
        let ordinary_program = requirement
            .resolve_for_use(Some(&ordinary), &qualification_use)
            .unwrap();
        let mut large = ordinary.into_inner();
        let runtime = large.get_mut("auxiliary").unwrap();
        runtime.relationship.required_product.storage =
            crate::external_content::products::ProductStorage::LargeContent;
        runtime.manifest_kind = crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into();
        let large = ResolvedExternalProductSelections::new(large).unwrap();
        let large_program = requirement
            .resolve_for_use(Some(&large), &qualification_use)
            .unwrap();
        assert_eq!(
            large_program.runtime_manifest_kind,
            crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
        );
        assert_ne!(
            large_program.digest().unwrap(),
            ordinary_program.digest().unwrap()
        );
        assert!(ordinary_program.verify_selections(Some(&large)).is_err());
    }

    #[test]
    fn runtime_recipe_is_closed_bounded_and_content_relative() {
        let recipe = runtime_recipe();
        recipe.validate().unwrap();
        assert_eq!(recipe.namespace_executable().unwrap(), "/runtime/bin/codex");
        for mutation in [
            "schema",
            "mount",
            "relative_mount",
            "noncanonical_mount",
            "executable",
            "absolute_executable",
            "noncanonical_executable",
            "cwd",
            "relative_cwd",
            "noncanonical_cwd",
            "environment",
            "ambient_path",
            "stdout_zero",
            "stderr_zero",
            "stdout_over",
            "stderr_over",
            "containment",
            "nested",
        ] {
            let mut changed = recipe.clone();
            match mutation {
                "schema" => changed.schema += 1,
                "mount" => changed.runtime_mount_destination = "/workspace/runtime".into(),
                "relative_mount" => changed.runtime_mount_destination = "runtime".into(),
                "noncanonical_mount" => {
                    changed.runtime_mount_destination = "/runtime//tools".into()
                }
                "executable" => changed.executable_relative_path = "../bin/codex".into(),
                "absolute_executable" => {
                    changed.executable_relative_path = "/runtime/bin/codex".into()
                }
                "noncanonical_executable" => changed.executable_relative_path = "bin//codex".into(),
                "cwd" => changed.cwd = "/controller".into(),
                "relative_cwd" => changed.cwd = "workspace".into(),
                "noncanonical_cwd" => changed.cwd = "/workspace//candidate".into(),
                "environment" => {
                    changed
                        .environment
                        .insert("BAD=NAME".into(), "value".into());
                }
                "ambient_path" => {
                    changed.environment.insert("PATH".into(), "/usr/bin".into());
                }
                "stdout_zero" => changed.max_stdout_bytes = 0,
                "stderr_zero" => changed.max_stderr_bytes = 0,
                "stdout_over" => changed.max_stdout_bytes = 64 * 1024 * 1024 + 1,
                "stderr_over" => changed.max_stderr_bytes = 64 * 1024 * 1024 + 1,
                "containment" => changed.contain_process_group = true,
                "nested" => changed.nested_sandbox = false,
                _ => unreachable!(),
            }
            assert!(changed.validate().is_err(), "accepted {mutation}");
        }
        let mut exact_maximum = recipe;
        exact_maximum.max_stdout_bytes = 64 * 1024 * 1024;
        exact_maximum.max_stderr_bytes = 64 * 1024 * 1024;
        exact_maximum.validate().unwrap();
    }

    #[test]
    fn external_capsule_roundtrip_rejoins_profile_and_retained_selection() {
        use crate::external_content::products::composition::EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY;
        use crate::objects::*;
        use serde_json::json;
        use std::collections::BTreeMap;
        let selections = selections();
        let requirement = requirement();
        let profile = test_support::fixture_profile(&requirement).unwrap();
        let source = test_support::fixture_source_projection();
        let realizations = test_support::fixture_provider_realizations().unwrap();
        let qualification_use = ExternalCandidateQualificationUse::from_admitted_inputs(
            &requirement,
            &profile,
            &source,
            &realizations,
            &[],
            &BTreeMap::new(),
        )
        .unwrap();
        let exact_program = json!({"resolution_output":{"composed":{"derived":{
            (EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY):selections.semantic_identity_value().unwrap(),
            (SOURCE_CLOSURE_DERIVED_KEY):source.to_value().unwrap(),
            (EXTERNAL_REALIZATIONS_DERIVED_KEY):realizations.to_value().unwrap()
        }}}});
        let artifact = crate::external_content::products::qualification::tests::artifact_identity();
        let capsule = AdmittedPersistentSessionCapsule {
            schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
            kind: PERSISTENT_SESSION_CAPSULE_KIND.into(),
            external_candidate: Some(
                requirement
                    .resolve_for_use(Some(&selections), &qualification_use)
                    .unwrap(),
            ),
            exact_program_hash: canonical_value_digest(&exact_program).unwrap(),
            exact_program,
            retained_product_selections: Some(selections),
            retained_external_runtime_qualification: None,
            lifecycle: PersistentSessionLifecycleContract {
                max_processes: 1,
                max_inflight_per_process: 1,
                max_address_space_bytes: 64 * 1024 * 1024,
                max_cpu_seconds: 1,
                real_uid_process_limit: 1,
                ready_timeout_ms: 1,
                request_timeout_ms: 1,
                idle_timeout_ms: 1,
            },
            wire: PersistentSessionWireContract {
                channel_env: "RYEOS_SESSION_FD".into(),
                wire_protocol: "ryeos.structured-session".into(),
                wire_version: 2,
                max_frame_bytes: 1024,
            },
            executor_ref: artifact.executor_ref().into(),
            artifact_identity: artifact,
            execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
                execution_plan: json!({}),
                protocol_descriptor_document: "fixture".into(),
                command: AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash: "6".repeat(64),
                    execution_path: admitted_direct_command_execution_path(
                        &"6".repeat(64),
                        std::path::Path::new("fixture"),
                    )
                    .unwrap(),
                },
                admitted_project_root: None,
            },
            execution_realization_hash: "8".repeat(64),
            source_binding_hash: Some(source.binding_hash),
            structured_session_profile: Some(profile),
            executable_search: vec![],
            process_environment: BTreeMap::new(),
            runtime_ref: "runtime:fixtures/qualified".into(),
        };
        capsule.validate().unwrap();
        let wire = capsule.to_value().unwrap();
        assert_eq!(
            AdmittedPersistentSessionCapsule::from_current_value(&wire).unwrap(),
            capsule
        );
        let mut changed = capsule.clone();
        changed.external_candidate = None;
        assert!(changed.validate().is_err());
        let mut changed = capsule.clone();
        let profile = changed.structured_session_profile.as_mut().unwrap();
        profile.contract["external_candidate"]["runtime_product_declaration_id"] = json!("runtime");
        profile.profile_hash = canonical_value_digest(&profile.contract).unwrap();
        assert!(changed.validate().is_err());
        for mutation in [
            "profile",
            "source_content",
            "provider_executable",
            "missing_source",
            "missing_realization",
        ] {
            let mut changed = capsule.clone();
            match mutation {
                "profile" => {
                    let profile = changed.structured_session_profile.as_mut().unwrap();
                    profile.contract["feature_marker"] = json!("changed");
                    profile.profile_hash = canonical_value_digest(&profile.contract).unwrap();
                }
                "source_content" => {
                    changed.exact_program["resolution_output"]["composed"]["derived"]
                        [SOURCE_CLOSURE_DERIVED_KEY]["content_manifest_hash"] =
                        json!("f".repeat(64));
                }
                "provider_executable" => {
                    changed.exact_program["resolution_output"]["composed"]["derived"]
                        [EXTERNAL_REALIZATIONS_DERIVED_KEY][0]["manifest_hash"] =
                        json!("f".repeat(64));
                }
                "missing_source" => {
                    changed.exact_program["resolution_output"]["composed"]["derived"]
                        .as_object_mut()
                        .unwrap()
                        .remove(SOURCE_CLOSURE_DERIVED_KEY);
                    changed.source_binding_hash = None;
                }
                "missing_realization" => {
                    changed.exact_program["resolution_output"]["composed"]["derived"]
                        .as_object_mut()
                        .unwrap()
                        .remove(EXTERNAL_REALIZATIONS_DERIVED_KEY);
                }
                _ => unreachable!(),
            }
            changed.exact_program_hash = canonical_value_digest(&changed.exact_program).unwrap();
            assert!(changed.validate().is_err(), "{mutation}");
        }
        // Keep the semantic program consistent with a changed retained proof;
        // the independent external projection must still refuse that change.
        let mut changed = capsule.clone();
        let mut selections = changed
            .retained_product_selections
            .take()
            .unwrap()
            .into_inner();
        selections
            .get_mut("auxiliary")
            .unwrap()
            .relationship_raw_content_digest = "0".repeat(64);
        let selections = ResolvedExternalProductSelections::new(selections).unwrap();
        changed.exact_program["resolution_output"]["composed"]["derived"]
            [EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY] =
            selections.semantic_identity_value().unwrap();
        changed.exact_program_hash = canonical_value_digest(&changed.exact_program).unwrap();
        changed.retained_product_selections = Some(selections);
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("external candidate program")
        );
    }

    #[test]
    fn composed_test_support_builds_a_valid_qualified_capsule() {
        let capsule =
            test_support::qualified_external_candidate_capsule(requirement(), &"a".repeat(64))
                .unwrap();
        capsule.validate().unwrap();
        assert_eq!(
            capsule
                .external_candidate
                .as_ref()
                .unwrap()
                .runtime_manifest_hash,
            "a".repeat(64)
        );
    }

    #[test]
    fn external_requirement_refuses_ambient_or_occurrence_authority() {
        for (field, value) in [
            ("schema", serde_json::json!(1)),
            ("protocol", serde_json::json!("local_fallback")),
            (
                "runtime_product_declaration_id",
                serde_json::json!("../runtime"),
            ),
            ("provider_declaration_id", serde_json::json!("../provider")),
            (
                "provider_configuration_destination",
                serde_json::json!("../environments.toml"),
            ),
        ] {
            let mut wire = serde_json::to_value(requirement()).unwrap();
            wire[field] = value;
            let changed: ExternalCandidateRequirement = serde_json::from_value(wire).unwrap();
            assert!(changed.validate().is_err());
        }
        for field in [
            "url",
            "credential",
            "occurrence_id",
            "execution_binding_hash",
            "capsule_hash",
        ] {
            let mut wire = serde_json::to_value(requirement()).unwrap();
            wire[field] = serde_json::json!("unexpected");
            assert!(serde_json::from_value::<ExternalCandidateRequirement>(wire).is_err());
        }
    }
}
