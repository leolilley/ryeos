//! Preparatory signed source authoring only. No executable implementation,
//! installation, runtime witness, claim result or qualification is minted.
//! The independent verifier must perform every selected-subject check before
//! it returns claims. These sources are not wired into any Worker relationship.

use anyhow::{Result, ensure};
use lillux::crypto::SigningKey;
use ryeos_external_execution_contract::LifecycleCapability;
use ryeos_independent_runtime_verifier::QualificationExecutionEnvironment;
use ryeos_state::external_content::products::producer_recipe::ProductProducerRecipe;
use ryeos_state::external_execution::admission::{
    ExternalCandidateQualificationUse, ExternalCandidateRequirement,
    QUALIFICATION_CONTEXT_PARAMETER,
};
use ryeos_state::objects::{AdmittedStructuredSessionProfile, EffectiveSourceClosureProjection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, net::SocketAddr};

pub const TOOL_REF: &str = "tool:fixtures/independent-runtime/verify";
pub const POLICY_REF: &str = "config:fixtures/independent-runtime/qualification";
pub const PRODUCER_RECIPE_REF: &str = "config:fixtures/independent-runtime/scenario-driver";
pub const PRODUCER_SCENARIO_ID: &str = ryeos_independent_runtime_verifier::PRODUCER_SCENARIO_ID;
pub const VERIFIER_BIN: &str = "independent-runtime-verifier";
const SCENARIO: &str = "test.independent_routed_runtime.v1";
const CLAIMS: [&str; 5] = [
    "bounded_candidate_capture",
    "candidate_only_execution",
    "controller_credential_exclusion",
    "native_writer_exclusion",
    "no_local_execution_fallback",
];

/// Exact authored fixture tuple, never reconstructed from observed output.
/// The configurations input contains the canonical admission-compiled profile,
/// full scripted app-server baseline and command-environment template. Its
/// manifest binds routing/feature/permission settings, not just a hand-picked
/// permissive subset. The dynamic provider origin is chosen before signing;
/// no invocation-time override is permitted.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndependentVerifierScenario {
    pub subject_manifest_hash: String,
    pub controller_manifest_hash: String,
    pub tools_manifest_hash: String,
    pub configurations_manifest_hash: String,
    pub codex_sha256: String,
    pub relay_sha256: String,
    pub scripted_baseline_sha256: String,
    pub command_environment_template_sha256: String,
    pub responses_origin: String,
    pub expected_command_output: String,
    pub requirement: ExternalCandidateRequirement,
    pub qualification_use: ExternalCandidateQualificationUse,
    pub execution_environment: QualificationExecutionEnvironment,
    pub expected_producer_recipe_ref: String,
    pub expected_producer_recipe: ProductProducerRecipe,
    pub capture_limit_bytes: u64,
}

impl IndependentVerifierScenario {
    /// Author the use tuple only from an admitted full Worker profile and its
    /// independently captured signed source closure. Public content import
    /// supplies the four exact manifest hashes before this call. Neither a
    /// verifier result nor a hand-built partial profile is an input.
    pub fn from_admitted_inputs(
        configuration: ryeos_independent_runtime_verifier::ExactScenario,
        profile: &AdmittedStructuredSessionProfile,
        source: &EffectiveSourceClosureProjection,
    ) -> Result<Self> {
        let use_context = ExternalCandidateQualificationUse::from_admitted_inputs(
            &configuration.requirement,
            profile,
            source,
            &configuration.execution_environment.realizations,
            &configuration.execution_environment.executable_search,
            &configuration.execution_environment.process_environment,
        )?;
        let scenario = Self {
            subject_manifest_hash: configuration.subject_manifest_hash,
            controller_manifest_hash: configuration.controller_manifest_hash,
            tools_manifest_hash: configuration.tools_manifest_hash,
            configurations_manifest_hash: configuration.configurations_manifest_hash,
            codex_sha256: configuration.codex_sha256,
            relay_sha256: configuration.relay_sha256,
            scripted_baseline_sha256: configuration.scripted_baseline_sha256,
            command_environment_template_sha256: configuration.command_environment_template_sha256,
            responses_origin: configuration.responses_origin,
            expected_command_output: configuration.expected_command_output,
            requirement: configuration.requirement,
            qualification_use: use_context,
            execution_environment: configuration.execution_environment,
            expected_producer_recipe: configuration.expected_producer_recipe,
            expected_producer_recipe_ref: configuration.expected_producer_recipe_ref,
            capture_limit_bytes: configuration.capture_limit_bytes,
        };
        scenario.parameters()?;
        Ok(scenario)
    }

    pub fn parameters(&self) -> Result<Value> {
        for hash in [
            &self.subject_manifest_hash,
            &self.controller_manifest_hash,
            &self.tools_manifest_hash,
            &self.configurations_manifest_hash,
            &self.codex_sha256,
            &self.relay_sha256,
            &self.scripted_baseline_sha256,
            &self.command_environment_template_sha256,
        ] {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "scenario requires canonical exact hashes"
            );
        }
        let address: SocketAddr = self
            .responses_origin
            .strip_prefix("http://")
            .ok_or_else(|| anyhow::anyhow!("scenario requires explicit loopback HTTP peer"))?
            .parse()?;
        ensure!(
            address.ip().is_loopback()
                && address.port() != 0
                && self.responses_origin == format!("http://{address}"),
            "noncanonical or nonloopback peer"
        );
        self.requirement.validate()?;
        self.expected_producer_recipe.validate()?;
        ensure!(
            self.expected_producer_recipe_ref == PRODUCER_RECIPE_REF,
            "scenario expected producer ref differs from signed Config"
        );
        ensure!(
            self.requirement.provider_declaration_id == "codex-hosted"
                && self.requirement.runtime_product_declaration_id == "guest-runtime"
                && self.requirement.required_lifecycle_capabilities
                    == [LifecycleCapability::ExactTerminalObservation]
                        .into_iter()
                        .collect(),
            "scenario does not select the signed Codex guest-runtime lifecycle requirement"
        );
        self.qualification_use.validate()?;
        ensure!(
            self.qualification_use.execution_environment_digest
                == self.execution_environment.digest()?,
            "scenario qualification use differs from typed execution environment"
        );
        ensure!(
            self.qualification_use.requirement_digest
                == self.requirement.qualification_requirement_digest()?,
            "scenario qualification use differs from requirement"
        );
        ensure!(
            (1..=1024 * 1024).contains(&self.capture_limit_bytes),
            "scenario capture bound"
        );
        ensure!(
            self.expected_command_output.len() <= 8192
                && self
                    .expected_command_output
                    .starts_with("/workspace\nripgrep ")
                && self.expected_command_output.ends_with('\n')
                && !self.expected_command_output.contains('\0'),
            "scenario command output must be authored before execution"
        );
        let mut configuration = serde_json::to_value(self)?;
        configuration
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("scenario configuration is not an object"))?
            .remove("qualification_use");
        let mut value = json!({"scenario": SCENARIO, "configuration": configuration});
        value[QUALIFICATION_CONTEXT_PARAMETER] = serde_json::to_value(&self.qualification_use)?;
        let encoded = lillux::canonical_json(&value)?;
        ensure!(
            encoded.len() <= 8 * 1024,
            "scenario exceeds qualification parameter bound"
        );
        ryeos_independent_runtime_verifier::Parameters::parse(encoded.as_bytes())?;
        Ok(value)
    }

    /// Return signed source members for an independently admitted exact Bundle
    /// executable. The Bundle executable identity is the verifier authority;
    /// this scenario does not repeat an unchecked executable hash claim.
    /// This function writes nothing and does not register/activate a bundle.
    pub fn signed_sources(
        &self,
        publisher: &SigningKey,
        signed_at: &str,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        self.signed_sources_with_mode(publisher, signed_at, false)
    }

    /// Test-only signed variant: the same selected producer and exact inputs,
    /// but the root verifier concurrently requests START and exact RESUME.
    /// It still issues no qualification claims.
    pub fn signed_sources_for_reserved_resume_race(
        &self,
        publisher: &SigningKey,
        signed_at: &str,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        self.signed_sources_with_mode(publisher, signed_at, true)
    }

    fn signed_sources_with_mode(
        &self,
        publisher: &SigningKey,
        signed_at: &str,
        reserved_resume_race: bool,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        let (mut tool, policy, recipe) = self.sources()?;
        if reserved_resume_race {
            tool["config"]["args"] = json!(["--scoped-resume-race-probe"]);
        }
        [
            (".ai/tools/fixtures/independent-runtime/verify.yaml", tool),
            (
                ".ai/config/fixtures/independent-runtime/qualification.yaml",
                policy,
            ),
            (
                ".ai/config/fixtures/independent-runtime/scenario-driver.yaml",
                recipe,
            ),
        ]
        .into_iter()
        .map(|(path, value)| {
            let body = serde_yaml::to_string(&value)?;
            Ok((
                path.to_owned(),
                lillux::signature::sign_content_at(&body, publisher, "#", None, signed_at)
                    .into_bytes(),
            ))
        })
        .collect()
    }

    fn sources(&self) -> Result<(Value, Value, Value)> {
        let parameters = self.parameters()?;
        let declarations = [
            ("subject", &self.subject_manifest_hash),
            ("controller", &self.controller_manifest_hash),
            ("tools", &self.tools_manifest_hash),
            ("configurations", &self.configurations_manifest_hash),
        ]
        .into_iter()
        .map(|(id, hash)| {
            json!({"id":id,"kind":"tree","mode":"pinned",
            "digest":hash,"mount_root":"project","mount":format!("qualification/{id}")})
        })
        .collect::<Vec<_>>();
        let tool = json!({
            "category":"fixtures/independent-runtime", "name":"verify", "version":"1.0.0",
            "description":"Independently check exact scripted runtime scenario; not production profile qualification",
            "executor_id":"@subprocess", "execution_protocol":"protocol:ryeos/core/opaque",
            "effects":"live", "filesystem_authority":"node_policy", "network_authority":"node_policy",
            "external_content":declarations,
            "config":{"command":format!("bin:{VERIFIER_BIN}"), "args":[], "input_data":"${params_json}", "timeout_secs":300},
            "config_schema":{"type":"object", "properties":{
                "scenario":{"const":SCENARIO}, "configuration":{"const":parameters["configuration"]},
                (QUALIFICATION_CONTEXT_PARAMETER):{"const":parameters[QUALIFICATION_CONTEXT_PARAMETER]}
            }, "required":["scenario","configuration",QUALIFICATION_CONTEXT_PARAMETER],
            "additionalProperties":false}
        });
        let policy = json!({"category":"fixtures/independent-runtime","version":"1.0.0",
            "description":"Finite fixture verifier allowance; no runtime qualification is asserted by authoring",
            "product_qualification_policy":{"schema":"ryeos.product_qualification_policy.v2",
                "verifier_ref":TOOL_REF,"subject_declaration_id":"subject",
                "allowed_claims":CLAIMS,
                "minimum_verifier_process_settlement":"scope_empty",
                "verifier_parameters":parameters,
                "producer_scenarios":{(PRODUCER_SCENARIO_ID):{"recipe_ref":PRODUCER_RECIPE_REF}}}});
        let recipe = json!({"category":"fixtures/independent-runtime","name":"scenario-driver",
            "version":"1.0.0",
            "description":"Exact bounded verifier-owned scripted scenario child",
            "product_producer_recipe":self.expected_producer_recipe});
        ryeos_state::external_content::products::qualification::ProductQualificationPolicy::from_value(&policy["product_qualification_policy"])?;
        ryeos_state::external_content::products::producer_recipe::ProductProducerRecipe::from_value(
            recipe["product_producer_recipe"].clone(),
        )?;
        Ok((tool, policy, recipe))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn intended_requirement() -> ExternalCandidateRequirement {
        let mut requirement = crate::candidate_authoring::real_codex_requirement();
        requirement.runtime_product_declaration_id = "guest-runtime".into();
        requirement
            .required_lifecycle_capabilities
            .insert(LifecycleCapability::ExactTerminalObservation);
        requirement
    }
    fn scenario() -> IndependentVerifierScenario {
        let requirement = intended_requirement();
        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(4)
            .unwrap();
        let environment_source = std::fs::read_to_string(
            repository.join("bundles/codex/.ai/config/codex/environments/external-authoring.yaml"),
        )
        .unwrap();
        let environment_definition: Value = serde_yaml::from_str(&environment_source).unwrap();
        let environment_configuration = &environment_definition["configuration"];
        let executable_search =
            serde_json::from_value(environment_configuration["executable_search"].clone()).unwrap();
        let process_environment =
            serde_json::from_value(environment_configuration["process_environment"].clone())
                .unwrap();
        let mut realized = ryeos_state::external_execution::admission::test_support::fixture_provider_realizations()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        realized.push(ryeos_state::objects::ExternalContentRealization {
            id: "authoring-tools".into(),
            kind: ryeos_state::objects::ExternalContentKind::Tree,
            mode: ryeos_state::objects::ExternalContentMode::Pinned,
            manifest_hash: "c".repeat(64),
            entry_count: 2,
            total_bytes: 2,
            mount_root: ryeos_state::objects::ExternalContentMountRoot::ExecutionRuntime,
            mount: "authoring-tools".into(),
        });
        realized.push(ryeos_state::objects::ExternalContentRealization {
            id: "guest-runtime".into(),
            kind: ryeos_state::objects::ExternalContentKind::Tree,
            mode: ryeos_state::objects::ExternalContentMode::Pinned,
            manifest_hash: "a".repeat(64),
            entry_count: 2,
            total_bytes: 2,
            mount_root: ryeos_state::objects::ExternalContentMountRoot::ExecutionRuntime,
            mount: "guest-runtime".into(),
        });
        let execution_environment = QualificationExecutionEnvironment {
            realizations: ryeos_state::objects::ExternalContentRealizationSet::new(realized)
                .unwrap(),
            executable_search,
            process_environment,
        };
        let mut qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        qualification_use.execution_environment_digest = execution_environment.digest().unwrap();
        IndependentVerifierScenario {
            subject_manifest_hash: "a".repeat(64),
            controller_manifest_hash: "b".repeat(64),
            tools_manifest_hash: "c".repeat(64),
            configurations_manifest_hash: "d".repeat(64),
            codex_sha256: "e".repeat(64),
            relay_sha256: "f".repeat(64),
            scripted_baseline_sha256: "2".repeat(64),
            command_environment_template_sha256: "3".repeat(64),
            responses_origin: "http://127.0.0.1:18765".into(),
            expected_command_output: "/workspace\nripgrep fixture\n".into(),
            requirement: requirement.clone(),
            qualification_use,
            execution_environment,
            expected_producer_recipe_ref: PRODUCER_RECIPE_REF.into(),
            expected_producer_recipe: ProductProducerRecipe::from_value(json!({
                "schema":"ryeos.product_producer_recipe.v4",
                "executable_source":{"kind":"admitted_verifier_executable"},
                "argv":["--scenario-driver"],
                "stdin_source":{"kind":"signed_verifier_parameters"},
                "cwd_source":{"kind":"verifier_private_workspace"},
                "environment_sources":["admitted_realizations"],
                "environment_bindings":{},
                "loopback_ingress":null,
                "bounds":{"maximum_wall_time_ms":170000,
                    "maximum_stdout_bytes":6291456,
                    "maximum_stderr_bytes":1048576,
                    "maximum_memory_bytes":2147483648_u64,
                    "maximum_processes":64}
            }))
            .unwrap(),
            capture_limit_bytes: 4096,
        }
    }
    #[test]
    fn use_context_is_derived_from_the_admitted_profile_and_source() {
        let seed = scenario();
        let mut value = serde_json::to_value(&seed).unwrap();
        value.as_object_mut().unwrap().remove("qualification_use");
        let configuration = serde_json::from_value(value).unwrap();
        let profile = ryeos_state::external_execution::admission::test_support::fixture_profile(
            &seed.requirement,
        )
        .unwrap();
        let source =
            ryeos_state::external_execution::admission::test_support::fixture_source_projection();
        let authored =
            IndependentVerifierScenario::from_admitted_inputs(configuration, &profile, &source)
                .unwrap();
        assert_eq!(
            authored.qualification_use.profile_hash,
            profile.profile_hash
        );
        assert_eq!(
            authored.qualification_use.source_binding_hash,
            source.binding_hash
        );
        assert_eq!(
            authored.qualification_use.source_content_manifest_hash,
            source.content_manifest_hash
        );
        assert!(authored.parameters().is_ok());
    }
    #[test]
    fn exact_static_parameters_and_projectless_bundle_input_lane() {
        let scenario = scenario();
        let expected_guest = scenario
            .execution_environment
            .expected_guest_environment(&scenario.requirement)
            .unwrap();
        assert_eq!(
            expected_guest.environment,
            BTreeMap::from([
                ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("GIT_PAGER".into(), "cat".into()),
                (
                    "PATH".into(),
                    "/ryeos/realizations/authoring-tools/bin".into(),
                ),
                ("TMPDIR".into(), "/ryeos/runtime-views/TMPDIR".into()),
                ("TZ".into(), "UTC".into()),
            ])
        );
        let parameters = scenario.parameters().unwrap();
        let parsed = ryeos_independent_runtime_verifier::Parameters::parse(
            &serde_json::to_vec(&parameters).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.scenario, SCENARIO);
        assert_eq!(
            parsed.external_candidate_qualification_context,
            scenario.qualification_use
        );
        assert!(
            parameters["configuration"]
                .get("qualification_use")
                .is_none()
        );
        let (tool, policy, recipe) = scenario.sources().unwrap();
        let signed_recipe =
            ProductProducerRecipe::from_value(recipe["product_producer_recipe"].clone()).unwrap();
        assert_eq!(signed_recipe, scenario.expected_producer_recipe);
        assert_eq!(
            signed_recipe.digest().unwrap(),
            ProductProducerRecipe::from_value(
                policy["product_qualification_policy"]["verifier_parameters"]["configuration"]
                    ["expected_producer_recipe"]
                    .clone(),
            )
            .unwrap()
            .digest()
            .unwrap()
        );
        let mut drifted = signed_recipe.clone();
        drifted.bounds.maximum_stdout_bytes -= 1;
        assert_ne!(drifted.digest().unwrap(), signed_recipe.digest().unwrap());
        assert_eq!(
            policy["product_qualification_policy"]["verifier_parameters"],
            scenario.parameters().unwrap()
        );
        assert_eq!(
            policy["product_qualification_policy"]["verifier_parameters"]
                [QUALIFICATION_CONTEXT_PARAMETER],
            serde_json::to_value(&scenario.qualification_use).unwrap()
        );
        assert_eq!(tool["config"]["input_data"], "${params_json}");
        assert_eq!(
            tool["config"]["command"],
            "bin:independent-runtime-verifier"
        );
        assert_eq!(
            tool["config_schema"]["properties"]["configuration"]["const"],
            scenario.parameters().unwrap()["configuration"]
        );
        assert!(
            tool["external_content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d["mount_root"] == "project")
        );
        assert!(tool.get("claims").is_none());
        assert!(tool.get("external_candidate").is_none());
        assert!(policy.get("claims").is_none());
        assert_eq!(
            policy["product_qualification_policy"]["producer_scenarios"][PRODUCER_SCENARIO_ID]["recipe_ref"],
            PRODUCER_RECIPE_REF
        );
        assert_eq!(
            recipe["product_producer_recipe"]["argv"],
            json!(["--scenario-driver"])
        );
    }
    #[test]
    fn identity_and_endpoint_changes_are_not_normalized_away() {
        let original = scenario();
        let mut changed = original.clone();
        changed.responses_origin = "http://127.0.0.1:18766".into();
        assert_ne!(
            original.parameters().unwrap(),
            changed.parameters().unwrap()
        );
        changed = original.clone();
        changed.expected_command_output = "/workspace\nripgrep changed\n".into();
        assert_ne!(
            original.parameters().unwrap(),
            changed.parameters().unwrap()
        );
        changed.expected_command_output = "derived from observation".into();
        assert!(changed.parameters().is_err());
        changed = original.clone();
        for origin in [
            "https://127.0.0.1:1",
            "http://example.com:1",
            "http://127.0.0.1:0",
            "http://127.0.0.1:1/",
        ] {
            changed.responses_origin = origin.into();
            assert!(changed.parameters().is_err());
        }
        changed = original.clone();
        changed.codex_sha256 = "E".repeat(64);
        assert!(changed.parameters().is_err());
        changed = original;
        changed.capture_limit_bytes = 0;
        assert!(changed.parameters().is_err());
        let mut changed = scenario();
        changed
            .requirement
            .runtime_recipe
            .arguments
            .push("--changed".into());
        assert!(changed.parameters().is_err());
        changed.qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &changed.requirement,
            )
            .unwrap();
        assert!(changed.parameters().is_err());
    }
}
