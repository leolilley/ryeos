//! Program requirements for external candidate execution. These identities
//! never grant cloud allocation or substitute for a protected placement binding.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::external_content::products::composition::ResolvedExternalProductSelections;
use crate::objects::{EXTERNAL_CONTENT_MANIFEST_KIND, ExternalContentKind, canonical_value_digest};

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

/// Admission ceiling for the profile-owned recipe. The launcher bootstrap
/// carries both the admitted program and an independently checked projection
/// of this recipe. Keeping the recipe below 96 KiB leaves more than 64 KiB for
/// the fixed binding/program envelope inside its 256 KiB descriptor limit, so
/// every admitted recipe remains representable after channel attachment.
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
            self.schema == 1,
            "unsupported external runtime recipe schema"
        );
        let mount = Path::new(&self.runtime_mount_destination);
        require_absolute_normalized(mount, "runtime mount destination")?;
        ensure!(
            !mount.starts_with("/workspace")
                && !Path::new("/workspace").starts_with(mount)
                && !["/proc", "/dev", "/sys", "/tmp"]
                    .iter()
                    .any(|reserved| mount.starts_with(reserved)
                        || Path::new(reserved).starts_with(mount)),
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
            self.contain_process_group
                && matches!(
                    (self.proc_filesystem, self.nested_sandbox),
                    (ExternalCandidateProcFilesystem::Empty, false)
                        | (ExternalCandidateProcFilesystem::PidNamespace, false)
                        | (ExternalCandidateProcFilesystem::PidNamespaceNested, true)
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
            self.schema == 3
                && self.protocol == PROTOCOL
                && self.connector_protocol == CONNECTOR_PROTOCOL
                && self.execution_route == ExternalCandidateExecutionRoute::ConnectorOnly,
            "external candidate protocol is not admitted"
        );
        crate::external_content::products::validate_name(&self.runtime_product_declaration_id)?;
        self.runtime_recipe.validate()
    }

    pub fn resolve(
        &self,
        selections: Option<&ResolvedExternalProductSelections>,
    ) -> Result<AdmittedExternalCandidateProgram> {
        self.validate()?;
        let selections =
            selections.context("external candidate requires retained runtime products")?;
        selections.validate()?;
        let runtime = selections
            .get(&self.runtime_product_declaration_id)
            .context("external candidate runtime product is absent")?;
        ensure!(
            runtime.manifest_kind == EXTERNAL_CONTENT_MANIFEST_KIND
                && runtime.declaration.kind == ExternalContentKind::Tree,
            "external candidate runtime requires an exact content tree"
        );
        let qualification = runtime
            .qualification
            .as_ref()
            .context("external candidate runtime has no admitted qualification")?;
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
            runtime_manifest_hash: runtime.manifest_hash.clone(),
            runtime_witness_hash: runtime.witness_hash.clone(),
            qualification_attestation_hash: qualification.attestation_hash.clone(),
            selection_identity_digest: canonical_value_digest(&runtime.semantic_identity_value()?)?,
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
    pub runtime_manifest_hash: String,
    pub runtime_witness_hash: String,
    pub qualification_attestation_hash: String,
    pub selection_identity_digest: String,
    pub runtime_recipe_digest: String,
}

impl AdmittedExternalCandidateProgram {
    pub fn validate(&self) -> Result<()> {
        self.requirement.validate()?;
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
            self.requirement.resolve(selections)? == *self,
            "external candidate program contradicts retained product authority"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-candidate-program.v1",
            "program": self,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_recipe() -> ExternalCandidateRuntimeRecipe {
        ExternalCandidateRuntimeRecipe {
            schema: 1,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::from([("LANG".into(), "C.UTF-8".into())]),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: true,
            nested_sandbox: true,
        }
    }

    fn requirement() -> ExternalCandidateRequirement {
        ExternalCandidateRequirement {
            schema: 3,
            protocol: PROTOCOL.into(),
            connector_protocol: CONNECTOR_PROTOCOL.into(),
            execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
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
        proof.result.claims = claims;
        proof.verifier.result_digest = proof.result.digest().unwrap();
        ResolvedExternalProductSelections::new(selections).unwrap()
    }

    #[test]
    fn external_program_requires_exact_qualified_runtime() {
        let requirement = requirement();
        let selections = selections();
        let program = requirement.resolve(Some(&selections)).unwrap();
        program.verify_selections(Some(&selections)).unwrap();
        assert!(requirement.resolve(None).is_err());
        let mut missing = requirement.clone();
        missing.runtime_product_declaration_id = "absent".into();
        assert!(missing.resolve(Some(&selections)).is_err());
        let mut unqualified = requirement.clone();
        unqualified.runtime_product_declaration_id = "runtime".into();
        assert!(unqualified.resolve(Some(&selections)).is_err());
        let mut weaker = selections.clone().into_inner();
        weaker
            .get_mut("auxiliary")
            .unwrap()
            .relationship
            .qualification
            .required_claims
            .pop();
        let weaker = ResolvedExternalProductSelections::new(weaker).unwrap();
        assert!(requirement.resolve(Some(&weaker)).is_err());
        for field in [
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
        let mut program = requirement.resolve(Some(&selections)).unwrap();
        program.requirement = changed;
        assert!(program.validate().is_err());
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
                "stdout_zero" => changed.max_stdout_bytes = 0,
                "stderr_zero" => changed.max_stderr_bytes = 0,
                "stdout_over" => changed.max_stdout_bytes = 64 * 1024 * 1024 + 1,
                "stderr_over" => changed.max_stderr_bytes = 64 * 1024 * 1024 + 1,
                "containment" => changed.contain_process_group = false,
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
        let contract = json!({"external_candidate":requirement,
            "auxiliary_configs":[], "runtime_configs":[]});
        let exact_program = json!({"resolution_output":{"composed":{"derived":{
            (EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY):selections.semantic_identity_value().unwrap()
        }}}});
        let artifact = crate::external_content::products::qualification::tests::artifact_identity();
        let capsule = AdmittedPersistentSessionCapsule {
            schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
            kind: PERSISTENT_SESSION_CAPSULE_KIND.into(),
            external_candidate: Some(requirement.resolve(Some(&selections)).unwrap()),
            exact_program_hash: canonical_value_digest(&exact_program).unwrap(),
            exact_program,
            retained_product_selections: Some(selections),
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
            source_binding_hash: None,
            structured_session_profile: Some(AdmittedStructuredSessionProfile {
                profile_hash: canonical_value_digest(&contract).unwrap(),
                contract,
                schema_hashes: BTreeMap::from([("response.json".into(), "9".repeat(64))]),
                baseline_source: "baseline.conf".into(),
                baseline_destination: "config.conf".into(),
                auxiliary_configs: vec![],
                runtime_configs: vec![],
            }),
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
    fn external_requirement_refuses_ambient_or_occurrence_authority() {
        for (field, value) in [
            ("schema", serde_json::json!(1)),
            ("protocol", serde_json::json!("local_fallback")),
            (
                "runtime_product_declaration_id",
                serde_json::json!("../runtime"),
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
