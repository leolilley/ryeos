//! Public original-controller canary observations and exact selected-adapter
//! commitments. These are data, never original custody or contact authority.
//! Provider template syntax and command recipes belong to the admitted adapter.
use crate::objects::canonical_value_digest;
use anyhow::{Result, ensure};
use ryeos_external_execution_contract::qualification_request_binding::{
    QualificationRequestBindingInput, QualificationRequestBindingOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QControllerCanaryContract {
    #[serde(rename = "original_controller_read_and_local_refusal_v1")]
    OriginalControllerReadAndLocalRefusalV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QControllerCanaryFileObservation {
    pub directory_identity: lillux::PinnedDirectoryIdentity,
    pub file_identity: lillux::PinnedRegularFileIdentity,
    pub content_sha256: String,
    pub size_bytes: u64,
    pub permission_mode: u32,
}
impl QControllerCanaryFileObservation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            lillux::valid_hash(&self.content_sha256)
                && self.size_bytes == 65
                && self.permission_mode == 0o600,
            "Q canary is not the original bounded private file"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QControllerCanaryBinding {
    pub schema: u32,
    pub controller_path: String,
    pub initial_file: QControllerCanaryFileObservation,
    pub provider_declaration_id: String,
    pub configuration_adapter_artifact_hash: String,
    pub configuration_adapter_artifact_bytes: u64,
    pub binding_input_digest: String,
    pub binding_output_digest: String,
    pub template_request_digest: String,
    pub resolved_request_digest: String,
    pub resolved_body: Value,
}
impl QControllerCanaryBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "Q canary binding is not current");
        validate_controller_path(&self.controller_path)?;
        self.initial_file.validate()?;
        for value in [
            &self.configuration_adapter_artifact_hash,
            &self.binding_input_digest,
            &self.binding_output_digest,
            &self.template_request_digest,
            &self.resolved_request_digest,
        ] {
            ensure!(
                lillux::valid_hash(value),
                "Q canary adapter/request digest is invalid"
            );
        }
        ensure!(
            !self.provider_declaration_id.is_empty()
                && self.provider_declaration_id.len() <= 128
                && self
                    .provider_declaration_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                && self.configuration_adapter_artifact_bytes > 0,
            "Q canary provider adapter identity is invalid"
        );
        ensure!(
            self.resolved_body.is_object()
                && lillux::canonical_json(&self.resolved_body)?.len() <= 8192
                && canonical_value_digest(&self.resolved_body)? == self.resolved_request_digest
                && self.template_request_digest != self.resolved_request_digest,
            "Q canary changed its actual bounded adapter reply"
        );
        let output = QualificationRequestBindingOutput {
            schema: 1,
            input_digest: self.binding_input_digest.clone(),
            resolved_body: self.resolved_body.clone(),
        };
        ensure!(
            output.digest()? == self.binding_output_digest,
            "Q canary changed its exact adapter output wire"
        );
        Ok(())
    }
    /// Reproduce only generic byte commitments. This neither interprets the
    /// provider template nor proves that the selected adapter was invoked.
    pub fn require_bound_body(&self, template: &Value) -> Result<Value> {
        self.validate()?;
        let input = QualificationRequestBindingInput {
            schema: 1,
            template: template.clone(),
            controller_canary_path: self.controller_path.clone(),
        };
        ensure!(
            canonical_value_digest(template)? == self.template_request_digest
                && input.digest()? == self.binding_input_digest,
            "Q canary changed its signed template or original adapter input"
        );
        Ok(self.resolved_body.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QControllerCanaryEvidence {
    pub schema: u32,
    pub binding: QControllerCanaryBinding,
    pub terminal_file: QControllerCanaryFileObservation,
}
impl QControllerCanaryEvidence {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "Q canary evidence is not current");
        self.binding.validate()?;
        self.terminal_file.validate()?;
        ensure!(
            self.terminal_file == self.binding.initial_file,
            "Q canary terminal file changed"
        );
        Ok(())
    }
}

/// Fixed original-daemon child coordinate, not a provider recipe or path opener.
pub fn validate_controller_path(path: &str) -> Result<()> {
    ensure!(
        path.len() <= 1024
            && path.starts_with('/')
            && path.ends_with("/controller-canary/canary")
            && path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
            && path[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "Q canary path is not the bounded original child coordinate"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn generic_binding_checks_bytes_without_inventing_provider_grammar_or_custody() {
        let template = json!({"provider_defined":"unresolved é"});
        let input = QualificationRequestBindingInput {
            schema: 1,
            template: template.clone(),
            controller_canary_path: "/private/controller-canary/canary".into(),
        };
        let output = QualificationRequestBindingOutput {
            schema: 1,
            input_digest: input.digest().unwrap(),
            resolved_body: json!({"provider_defined":"resolved é"}),
        };
        // Public shape fixture ONLY: no file/process/contact owner is minted.
        let binding:QControllerCanaryBinding=serde_json::from_value(json!({
            "schema":1,"controller_path":input.controller_canary_path,
            "initial_file":{"directory_identity":{"filesystem": {"device": 1},"inode": 2},"file_identity":{"filesystem": {"device": 1},"inode": 3},"content_sha256":"a".repeat(64),"size_bytes":65,"permission_mode":384},
            "provider_declaration_id":"provider","configuration_adapter_artifact_hash":"b".repeat(64),"configuration_adapter_artifact_bytes":1,
            "binding_input_digest":input.digest().unwrap(),"binding_output_digest":output.digest().unwrap(),
            "template_request_digest":canonical_value_digest(&template).unwrap(),"resolved_request_digest":canonical_value_digest(&output.resolved_body).unwrap(),"resolved_body":output.resolved_body
        })).unwrap();
        binding.require_bound_body(&template).unwrap();
        assert!(
            binding
                .require_bound_body(&json!({"provider_defined":"changed"}))
                .is_err()
        );
        let mut changed = binding.clone();
        changed.resolved_body = json!({"provider_defined":"changed"});
        assert!(changed.validate().is_err());
        let mut changed = binding.clone();
        changed.binding_output_digest = "f".repeat(64);
        assert!(changed.validate().is_err());
        let evidence = QControllerCanaryEvidence {
            schema: 1,
            terminal_file: binding.initial_file.clone(),
            binding,
        };
        evidence.validate().unwrap();
        let mut changed = evidence;
        changed.terminal_file.content_sha256 = "e".repeat(64);
        assert!(changed.validate().is_err());
    }
    #[test]
    fn generic_selection_and_original_path_are_closed_data() {
        for bad in [json!("old_v0"), json!({"pointer":"/input"}), Value::Null] {
            assert!(serde_json::from_value::<QControllerCanaryContract>(bad).is_err());
        }
        for bad in [
            "relative/controller-canary/canary",
            "/x/../controller-canary/canary",
            "/x//controller-canary/canary",
            "/x/$HOME/controller-canary/canary",
        ] {
            assert!(validate_controller_path(bad).is_err());
        }
    }
}
