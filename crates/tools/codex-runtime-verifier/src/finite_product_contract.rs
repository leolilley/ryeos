//! Finite Codex product data supplied by the admitted signed checker Tool.
//! Pins select the audited content; they do not prove applied routing, native
//! settlement, or any qualification claim. Authentication belongs to the caller.
use anyhow::{Result, ensure};
use ryeos_state::external_content::products::qualification::ProductQualificationConsumerContentIdentity;
use ryeos_state::objects::ExternalContentRealizationSet;
use ryeos_state::objects::thread_snapshot::validate_canonical_hash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_FINITE_PRODUCT_CONTRACT_BYTES: usize = 8 * 1024;
const HELPER_IDS: [&str; 5] = [
    "codex",
    "codex-code-mode-host",
    "codex-zsh",
    "codex-bwrap",
    "codex-rg",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FiniteProductContract {
    schema: String,
    audit_edition: String,
    target: FiniteProductTarget,
    helpers: BTreeMap<String, String>,
    inputs: FiniteInputRestrictions,
    setup: FiniteSetupPredicates,
    families: FiniteEffectFamilies,
    permitted_controller_services: Vec<String>,
    semantic_checker: CheckerProgramSelector,
}

/// Signed selected program data only; this never authenticates a root set,
/// attests child execution, or proves the requested enclosing scope settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckerProgramSelector {
    pub(crate) schema: String,
    pub(crate) wire_schema: String,
    pub(crate) runtime: CheckerRuntimeSelection,
    pub(crate) code: CheckerCodeSelection,
    pub(crate) max_header_bytes: u64,
    pub(crate) max_response_bytes: u64,
    pub(crate) required_root_settlement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckerRuntimeSelection {
    pub(crate) id: String,
    pub(crate) manifest_hash: String,
    pub(crate) loader: CheckerMemberSelection,
    pub(crate) interpreter: CheckerMemberSelection,
    pub(crate) library_path: String,
    pub(crate) prefix_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckerCodeSelection {
    pub(crate) id: String,
    pub(crate) manifest_hash: String,
    pub(crate) script: CheckerMemberSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckerMemberSelection {
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}

impl CheckerProgramSelector {
    pub(crate) fn validate(&self) -> Result<()> {
        use ryeos_state::objects::{MAX_EXTERNAL_CONTENT_FILE_BYTES, validate_canonical_project_relative_path};
        ensure!(self.schema == "ryeos.codex.semantic_checker_program.v1"
            && self.wire_schema == "ryeos.codex.semantic_input.v2", "unsupported checker program/wire");
        ensure!(self.max_header_bytes == 2 * 1024 * 1024 && self.max_response_bytes == 64 * 1024
            && self.required_root_settlement == "scope_empty", "checker safety bounds or required scope changed");
        for id in [&self.runtime.id, &self.code.id] {
            ensure!(!id.is_empty() && id.len() <= 64
                && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b)),
                "invalid checker realization ID");
        }
        ensure!(self.runtime.id != self.code.id, "checker runtime/code IDs alias");
        for hash in [&self.runtime.manifest_hash, &self.code.manifest_hash] {
            validate_canonical_hash("checker realization manifest", hash)?;
        }
        let mut total = 0u64;
        for member in [&self.runtime.loader, &self.runtime.interpreter, &self.code.script] {
            validate_canonical_project_relative_path(&member.path)?;
            ensure!(member.path.len() <= 1024 && (1..=MAX_EXTERNAL_CONTENT_FILE_BYTES).contains(&member.bytes),
                "checker member exceeds fixed native file bound");
            validate_canonical_hash("checker member digest", &member.sha256)?;
            total = total.checked_add(member.bytes).ok_or_else(|| anyhow::anyhow!("checker selected bytes overflow"))?;
        }
        ensure!(total <= 3 * MAX_EXTERNAL_CONTENT_FILE_BYTES, "checker selected file aggregate exceeds bound");
        ensure!(self.runtime.loader.path != self.runtime.interpreter.path, "checker executable roles alias");
        for path in [&self.runtime.library_path, &self.runtime.prefix_path] {
            validate_canonical_project_relative_path(path)?;
            ensure!(path.len() <= 1024, "checker runtime directory path exceeds bound");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FiniteProductTarget {
    worker_ref: String,
    profile_hash: String,
    source_binding_hash: String,
    source_content_manifest_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FiniteInputRestrictions {
    /// Q's closed canary language remains distinct from the audited ordinary
    /// Worker routes. This signed selection is not an applied-route witness.
    pub request_language: String,
    pub ordinary_request_language: String,
    pub environment: String,
    pub cwd: String,
    pub include_local: bool,
    pub session_setup: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FiniteSetupPredicates {
    pub model_provider: String,
    pub approval_policy: String,
    pub approvals_reviewer: String,
    pub sandbox: String,
    pub permission_profile: String,
    pub model_selection: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FiniteEffectFamilies {
    exec_and_stdin: String,
    shell_patch_interception: String,
    file_and_image_reads: String,
    code_mode: String,
    mcp_apps_plugins_skills: String,
    delegation: String,
    web_and_image_generation: String,
    memories_and_goals: String,
    guardian: String,
    deferred_tools: String,
    session_utilities: String,
}

impl FiniteProductContract {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: Self = ryeos_external_execution_contract::from_json_slice_strict(
            bytes,
            MAX_FINITE_PRODUCT_CONTRACT_BYTES,
        )?;
        value.validate()?;
        ensure!(
            value.canonical_bytes()?.as_slice() == bytes,
            "finite product contract bytes are not canonical"
        );
        Ok(value)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }

    pub(crate) fn semantic_checker(&self) -> &CheckerProgramSelector {
        &self.semantic_checker
    }

    pub fn inputs(&self) -> &FiniteInputRestrictions {
        &self.inputs
    }
    pub fn setup(&self) -> &FiniteSetupPredicates {
        &self.setup
    }

    pub fn validate(&self) -> Result<()> {
        self.semantic_checker.validate()?;
        ensure!(
            self.schema == "ryeos.codex.finite_product_contract.v2",
            "unsupported finite product schema"
        );
        ensure!(
            self.audit_edition == "codex-0.147.0-interpreted-remote-environment-v3",
            "unsupported finite product audit_edition"
        );
        ensure!(
            self.target.worker_ref == "worker:codex/external-hosted-authoring",
            "unsupported finite product target.worker_ref"
        );
        validate_canonical_hash(
            "finite product target.profile_hash",
            &self.target.profile_hash,
        )?;
        validate_canonical_hash(
            "finite product target.source_binding_hash",
            &self.target.source_binding_hash,
        )?;
        validate_canonical_hash(
            "finite product target.source_content_manifest_hash",
            &self.target.source_content_manifest_hash,
        )?;
        ensure!(
            self.inputs.request_language == "closed-session-start-and-input-only-canary-turn",
            "unsupported finite product inputs.request_language"
        );
        ensure!(
            self.inputs.ordinary_request_language
                == "closed-start-resume-and-owner-bound-plain-text-turns-v1",
            "unsupported finite product inputs.ordinary_request_language"
        );
        ensure!(
            self.inputs.environment == "sole-default-command-connector",
            "unsupported finite product inputs.environment"
        );
        ensure!(
            self.inputs.cwd == "/workspace",
            "unsupported finite product inputs.cwd"
        );
        ensure!(
            !self.inputs.include_local,
            "unsupported finite product inputs.include_local"
        );
        ensure!(
            self.inputs.session_setup == "raw-events-true-and-optional-canonical-model-only",
            "unsupported finite product inputs.session_setup"
        );
        ensure!(
            self.setup.model_provider == "openai",
            "unsupported finite product setup.model_provider"
        );
        ensure!(
            self.setup.approval_policy == "never",
            "unsupported finite product setup.approval_policy"
        );
        ensure!(
            self.setup.approvals_reviewer == "user",
            "unsupported finite product setup.approvals_reviewer"
        );
        ensure!(
            self.setup.sandbox == "dangerFullAccess",
            "unsupported finite product setup.sandbox"
        );
        ensure!(
            self.setup.permission_profile == "danger-full-access",
            "unsupported finite product setup.permission_profile"
        );
        ensure!(
            self.setup.model_selection == "compare-only-explicit-signed-selection",
            "unsupported finite product setup.model_selection"
        );
        ensure!(
            self.families.exec_and_stdin == "selected-remote-connector-no-local-fallback",
            "unsupported finite product families.exec_and_stdin"
        );
        ensure!(
            self.families.shell_patch_interception == "selected-environment-filesystem",
            "unsupported finite product families.shell_patch_interception"
        );
        ensure!(
            self.families.file_and_image_reads == "selected-environment-filesystem",
            "unsupported finite product families.file_and_image_reads"
        );
        ensure!(
            self.families.code_mode == "bounded-compute-no-imports-same-nested-router",
            "unsupported finite product families.code_mode"
        );
        ensure!(
            self.families.mcp_apps_plugins_skills == "original-checked-disabled-or-empty",
            "unsupported finite product families.mcp_apps_plugins_skills"
        );
        ensure!(
            self.families.delegation == "original-checked-agents-disabled",
            "unsupported finite product families.delegation"
        );
        ensure!(
            self.families.web_and_image_generation == "original-checked-disabled",
            "unsupported finite product families.web_and_image_generation"
        );
        ensure!(
            self.families.memories_and_goals == "original-checked-disabled",
            "unsupported finite product families.memories_and_goals"
        );
        ensure!(
            self.families.guardian
                == "guardian-context-only-and-request-permissions-empty-under-never-approval",
            "unsupported finite product families.guardian"
        );
        ensure!(
            self.families.deferred_tools == "presentation-only-no-added-authority",
            "unsupported finite product families.deferred_tools"
        );
        ensure!(
            self.families.session_utilities
                == "bounded-session-bookkeeping-and-existing-environment-wait-only",
            "unsupported finite product families.session_utilities"
        );
        ensure!(
            self.helpers.len() == HELPER_IDS.len()
                && HELPER_IDS.iter().all(|id| self.helpers.contains_key(*id)),
            "finite product helper declarations are not the exact five admitted roles"
        );
        for hash in self.helpers.values() {
            validate_canonical_hash("finite product helper manifest", hash)?;
        }
        ensure!(
            self.permitted_controller_services
                .iter()
                .map(String::as_str)
                .eq([
                    "auth-model-config",
                    "git-attribution-provider-metadata",
                    "rollout-history",
                    "telemetry-cache",
                    "bounded-compute-timers-session-store"
                ]),
            "finite product controller service allowance changed"
        );
        ensure!(
            lillux::canonical_json(&serde_json::to_value(self)?)?.len()
                <= MAX_FINITE_PRODUCT_CONTRACT_BYTES,
            "finite product contract exceeds its byte bound"
        );
        Ok(())
    }

    /// Compare data with an already authenticated consumer-content record.
    /// This does not authenticate arbitrary deserialized content or prove that
    /// a runtime applied its configuration. Those owners remain mandatory.
    pub fn validate_content(
        &self,
        content: &ProductQualificationConsumerContentIdentity,
    ) -> Result<()> {
        self.validate_content_identities(
            &content.definitions.worker.canonical_ref,
            &content.worker_profile_hash,
            &content.worker_source.binding_hash,
            &content.worker_source.content_manifest_hash,
            &content.worker_literals,
        )
    }

    fn validate_content_identities(
        &self,
        worker_ref: &str,
        profile_hash: &str,
        binding_hash: &str,
        manifest_hash: &str,
        helpers: &ExternalContentRealizationSet,
    ) -> Result<()> {
        self.validate()?;
        ensure!(
            self.target.worker_ref == worker_ref
                && self.target.profile_hash == profile_hash
                && self.target.source_binding_hash == binding_hash
                && self.target.source_content_manifest_hash == manifest_hash,
            "finite product target differs from admitted Worker content"
        );
        helpers.validate()?;
        ensure!(
            helpers.iter().len() == self.helpers.len()
                && helpers
                    .iter()
                    .all(|helper| self.helpers.get(&helper.id) == Some(&helper.manifest_hash)),
            "finite product helper manifests differ from admitted Worker literals"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn checker_selector_fixture() -> CheckerProgramSelector {
    serde_json::from_value(tests::value()["semantic_checker"].clone()).unwrap()
}

/// Unit fixtures still have to supply the exact admitted five-role set.
#[cfg(test)]
pub(crate) fn fixture_for_content(
    content: &ProductQualificationConsumerContentIdentity,
) -> Result<FiniteProductContract> {
    let mut value = tests::value();
    value["target"] = serde_json::json!({
        "worker_ref": content.definitions.worker.canonical_ref,
        "profile_hash": content.worker_profile_hash,
        "source_binding_hash": content.worker_source.binding_hash,
        "source_content_manifest_hash": content.worker_source.content_manifest_hash,
    });
    value["helpers"] = serde_json::to_value(
        content
            .worker_literals
            .iter()
            .map(|entry| (entry.id.clone(), entry.manifest_hash.clone()))
            .collect::<BTreeMap<_, _>>(),
    )?;
    let bytes = lillux::canonical_json(&value)?.into_bytes();
    let contract = FiniteProductContract::parse(&bytes)?;
    contract.validate_content(content)?;
    Ok(contract)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::{
        ExternalContentKind, ExternalContentMode, ExternalContentMountRoot,
        ExternalContentRealization,
    };
    use serde_json::{Value, json};
    pub(super) fn value() -> Value {
        json!(
        {
          "schema": "ryeos.codex.finite_product_contract.v2",
          "audit_edition": "codex-0.147.0-interpreted-remote-environment-v3",
          // Synthetic source-test pins ONLY, never enabled Tool content.
          "semantic_checker": {
            "schema":"ryeos.codex.semantic_checker_program.v1", "wire_schema":"ryeos.codex.semantic_input.v2",
            "runtime": {"id":"checker-python", "manifest_hash":"a".repeat(64),
              "loader":{"path":"lib/loader", "sha256":"b".repeat(64), "bytes":1},
              "interpreter":{"path":"python/bin/python", "sha256":"c".repeat(64), "bytes":1},
              "library_path":"lib", "prefix_path":"python"},
            "code":{"id":"checker-code", "manifest_hash":"d".repeat(64),
              "script":{"path":"checker.py", "sha256":"e".repeat(64), "bytes":1}},
            "max_header_bytes":2097152, "max_response_bytes":65536, "required_root_settlement":"scope_empty"
          },
          "permitted_controller_services": [
            "auth-model-config",
            "git-attribution-provider-metadata",
            "rollout-history",
            "telemetry-cache",
            "bounded-compute-timers-session-store"
          ],
          "target": {
            "worker_ref": "worker:codex/external-hosted-authoring",
            "profile_hash": "1111111111111111111111111111111111111111111111111111111111111111",
            "source_binding_hash": "2222222222222222222222222222222222222222222222222222222222222222",
            "source_content_manifest_hash": "3333333333333333333333333333333333333333333333333333333333333333"
          },
          "helpers": {
            "codex": "4444444444444444444444444444444444444444444444444444444444444444",
            "codex-code-mode-host": "5555555555555555555555555555555555555555555555555555555555555555",
            "codex-zsh": "6666666666666666666666666666666666666666666666666666666666666666",
            "codex-bwrap": "7777777777777777777777777777777777777777777777777777777777777777",
            "codex-rg": "8888888888888888888888888888888888888888888888888888888888888888"
          },
          "inputs": {
            "request_language": "closed-session-start-and-input-only-canary-turn",
            "ordinary_request_language": "closed-start-resume-and-owner-bound-plain-text-turns-v1",
            "environment": "sole-default-command-connector",
            "cwd": "/workspace",
            "include_local": false,
            "session_setup": "raw-events-true-and-optional-canonical-model-only"
          },
          "setup": {
            "model_provider": "openai",
            "approval_policy": "never",
            "approvals_reviewer": "user",
            "sandbox": "dangerFullAccess",
            "permission_profile": "danger-full-access",
            "model_selection": "compare-only-explicit-signed-selection"
          },
          "families": {
            "exec_and_stdin": "selected-remote-connector-no-local-fallback",
            "shell_patch_interception": "selected-environment-filesystem",
            "file_and_image_reads": "selected-environment-filesystem",
            "code_mode": "bounded-compute-no-imports-same-nested-router",
            "mcp_apps_plugins_skills": "original-checked-disabled-or-empty",
            "delegation": "original-checked-agents-disabled",
            "web_and_image_generation": "original-checked-disabled",
            "memories_and_goals": "original-checked-disabled",
            "guardian": "guardian-context-only-and-request-permissions-empty-under-never-approval",
            "deferred_tools": "presentation-only-no-added-authority",
            "session_utilities": "bounded-session-bookkeeping-and-existing-environment-wait-only"
          }
        }
            )
    }
    fn bytes(value: &Value) -> Vec<u8> {
        lillux::canonical_json(value).unwrap().into_bytes()
    }
    fn helpers(contract: &FiniteProductContract) -> ExternalContentRealizationSet {
        ExternalContentRealizationSet::new(
            contract
                .helpers
                .iter()
                .map(|(id, hash)| ExternalContentRealization {
                    id: id.clone(),
                    kind: ExternalContentKind::File,
                    mode: ExternalContentMode::Pinned,
                    manifest_hash: hash.clone(),
                    entry_count: 1,
                    total_bytes: 1,
                    mount_root: ExternalContentMountRoot::ExecutionRuntime,
                    mount: id.clone(),
                })
                .collect(),
        )
        .unwrap()
    }
    #[test]
    fn required_checker_selection_refuses_old_missing_or_unsafe_values() {
        let original = value();
        assert!(FiniteProductContract::parse(&bytes(&original)).is_ok());
        for mutation in ["old", "missing", "alias", "oversize", "escape", "weaken", "unknown", "bool_size"] {
            let mut changed = original.clone();
            match mutation {
                "old" => { changed["schema"] = json!("ryeos.codex.finite_product_contract.v1"); },
                "missing" => { changed.as_object_mut().unwrap().remove("semantic_checker"); },
                "alias" => { changed["semantic_checker"]["code"]["id"] = json!("checker-python"); },
                "oversize" => { changed["semantic_checker"]["code"]["script"]["bytes"] = json!(ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES + 1); },
                "escape" => { changed["semantic_checker"]["runtime"]["loader"]["path"] = json!("../loader"); },
                "weaken" => { changed["semantic_checker"]["required_root_settlement"] = json!("group_absent"); },
                "unknown" => { changed["semantic_checker"]["trusted"] = json!(true); },
                _ => { changed["semantic_checker"]["code"]["script"]["bytes"] = json!(true); },
            }
            assert!(FiniteProductContract::parse(&bytes(&changed)).is_err(), "{mutation}");
        }
    }

    #[test]
    fn finite_product_data_requires_canonical_closed_complete_rules() {
        let original = value();
        let canonical = bytes(&original);
        let parsed = FiniteProductContract::parse(&canonical).unwrap();
        assert_eq!(parsed.canonical_bytes().unwrap(), canonical);
        assert_eq!(parsed.setup().approval_policy, "never");
        assert!(!parsed.inputs().include_local);
        for section in ["target", "inputs", "setup", "families", "helpers"] {
            for key in original[section].as_object().unwrap().keys() {
                let mut changed = original.clone();
                changed[section].as_object_mut().unwrap().remove(key);
                assert!(
                    FiniteProductContract::parse(&bytes(&changed)).is_err(),
                    "missing {section}.{key}"
                );
            }
            let mut changed = original.clone();
            changed[section]["unknown"] = json!("unsupported");
            assert!(
                FiniteProductContract::parse(&bytes(&changed)).is_err(),
                "extra {section}"
            );
        }
        for key in original.as_object().unwrap().keys() {
            let mut changed = original.clone();
            changed.as_object_mut().unwrap().remove(key);
            assert!(
                FiniteProductContract::parse(&bytes(&changed)).is_err(),
                "missing {key}"
            );
        }
        for key in [
            "checker_hash",
            "bundle_manifest_hash",
            "purpose_digest",
            "capsule_hash",
        ] {
            let mut changed = original.clone();
            changed[key] = json!("a".repeat(64));
            assert!(FiniteProductContract::parse(&bytes(&changed)).is_err());
        }
        for section in ["inputs", "setup", "families"] {
            for key in original[section].as_object().unwrap().keys() {
                let mut changed = original.clone();
                changed[section][key] = if changed[section][key].is_boolean() {
                    json!(true)
                } else {
                    json!("other-rule")
                };
                assert!(
                    FiniteProductContract::parse(&bytes(&changed)).is_err(),
                    "changed {section}.{key}"
                );
            }
        }
        let mut changed = original.clone();
        changed["audit_edition"] = json!("future-edition");
        assert!(FiniteProductContract::parse(&bytes(&changed)).is_err());
        let mut changed = original;
        changed["permitted_controller_services"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert!(FiniteProductContract::parse(&bytes(&changed)).is_err());
    }
    #[test]
    fn finite_product_data_refuses_duplicate_noncanonical_and_oversized_bytes() {
        let canonical = bytes(&value());
        let mut whitespace = canonical.clone();
        whitespace.push(b'\n');
        assert!(FiniteProductContract::parse(&whitespace).is_err());
        let duplicate = format!(
            "{{\"schema\":\"ryeos.codex.finite_product_contract.v2\",{}",
            std::str::from_utf8(&canonical[1..]).unwrap()
        );
        assert!(FiniteProductContract::parse(duplicate.as_bytes()).is_err());
        for field in [
            "profile_hash",
            "source_binding_hash",
            "source_content_manifest_hash",
        ] {
            let mut changed = value();
            changed["target"][field] = json!("a".repeat(64));
            FiniteProductContract::parse(&bytes(&changed)).unwrap();
            changed["target"][field] = json!("A".repeat(64));
            assert!(
                FiniteProductContract::parse(&bytes(&changed)).is_err(),
                "uppercase target.{field}"
            );
        }
        for id in HELPER_IDS {
            let mut changed = value();
            changed["helpers"][id] = json!("a".repeat(64));
            FiniteProductContract::parse(&bytes(&changed)).unwrap();
            changed["helpers"][id] = json!("A".repeat(64));
            assert!(
                FiniteProductContract::parse(&bytes(&changed)).is_err(),
                "uppercase helper {id}"
            );
        }
        assert!(
            FiniteProductContract::parse(&vec![b' '; MAX_FINITE_PRODUCT_CONTRACT_BYTES + 1])
                .is_err()
        );
        assert!(FiniteProductContract::parse(b"").is_err());
    }
    #[test]
    fn finite_product_data_joins_exact_target_and_helper_manifest_roles() {
        let contract = FiniteProductContract::parse(&bytes(&value())).unwrap();
        let admitted = helpers(&contract);
        let target = [
            &contract.target.worker_ref,
            &contract.target.profile_hash,
            &contract.target.source_binding_hash,
            &contract.target.source_content_manifest_hash,
        ];
        let check = |parts: [&String; 4], helpers: &ExternalContentRealizationSet| {
            contract.validate_content_identities(parts[0], parts[1], parts[2], parts[3], helpers)
        };
        check(target, &admitted).unwrap();
        let wrong = "a".repeat(64);
        for index in 0..4 {
            let mut changed = target;
            changed[index] = &wrong;
            assert!(check(changed, &admitted).is_err());
        }
        let original: Vec<_> = admitted.iter().cloned().collect();
        let mut missing = original.clone();
        missing.pop();
        assert!(
            check(
                target,
                &ExternalContentRealizationSet::new(missing).unwrap()
            )
            .is_err()
        );
        let mut extra = original.clone();
        let mut other = extra[0].clone();
        other.id = "unadmitted".into();
        other.mount = "unadmitted".into();
        extra.push(other);
        assert!(check(target, &ExternalContentRealizationSet::new(extra).unwrap()).is_err());
        for index in 0..original.len() {
            let mut changed = original.clone();
            changed[index].manifest_hash = wrong.clone();
            assert!(
                check(
                    target,
                    &ExternalContentRealizationSet::new(changed).unwrap()
                )
                .is_err()
            );
        }
        let mut swapped = original.clone();
        swapped[0].manifest_hash = original[1].manifest_hash.clone();
        swapped[1].manifest_hash = original[0].manifest_hash.clone();
        assert!(
            check(
                target,
                &ExternalContentRealizationSet::new(swapped).unwrap()
            )
            .is_err()
        );
    }
}
