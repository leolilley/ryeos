//! Bounded provider-adapter request binding. The provider owns all template
//! grammar. These messages contain public data only and confer no custody,
//! executable selection, contact, or qualification authority.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub const QUALIFICATION_REQUEST_BINDING_PROTOCOL: &str =
    "ryeos.external-execution.qualification-request-binding.v1";
pub const MAX_QUALIFICATION_REQUEST_BINDING_BYTES: usize = 16 * 1024;
pub const QUALIFICATION_REQUEST_BINDING_FD_ENV: &str = "RYEOS_QUALIFICATION_REQUEST_BINDING_FD";
const MAX_BODY_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationRequestBindingInput {
    pub schema: u32,
    pub template: Value,
    pub controller_canary_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationRequestBindingOutput {
    pub schema: u32,
    pub input_digest: String,
    pub resolved_body: Value,
}

fn body(value: &Value) -> Result<()> {
    ensure!(
        value.is_object() && crate::canonical_json(value)?.len() <= MAX_BODY_BYTES,
        "qualification binding body must be a bounded object"
    );
    Ok(())
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl QualificationRequestBindingInput {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "qualification binding input is not current"
        );
        body(&self.template)?;
        ensure!(
            self.controller_canary_path.starts_with('/')
                && self.controller_canary_path.len() <= 1024
                && !self.controller_canary_path.chars().any(char::is_control),
            "qualification binding path is not bounded absolute public data"
        );
        ensure!(
            crate::canonical_json(self)?.len() <= MAX_QUALIFICATION_REQUEST_BINDING_BYTES,
            "qualification binding input exceeds wire bound"
        );
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        crate::canonical_json(self)
    }
    /// Exact typed wire digest, deliberately distinct from a State object digest.
    pub fn digest(&self) -> Result<String> {
        Ok(digest(&self.canonical_bytes()?))
    }
}
impl QualificationRequestBindingOutput {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.input_digest.len() == 64
                && self
                    .input_digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "qualification binding output schema or input digest is invalid"
        );
        body(&self.resolved_body)?;
        ensure!(
            crate::canonical_json(self)?.len() <= MAX_QUALIFICATION_REQUEST_BINDING_BYTES,
            "qualification binding output exceeds wire bound"
        );
        Ok(())
    }
    pub fn validate_for(&self, input: &QualificationRequestBindingInput) -> Result<()> {
        self.validate()?;
        ensure!(
            self.input_digest == input.digest()?,
            "qualification adapter changed original input"
        );
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        crate::canonical_json(self)
    }
    pub fn digest(&self) -> Result<String> {
        Ok(digest(&self.canonical_bytes()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn binding_wire_is_provider_neutral_closed_and_exact_input_bound() {
        let input = QualificationRequestBindingInput {
            schema: 1,
            template: json!({"provider_field":"é"}),
            controller_canary_path: "/private/controller-canary/canary".into(),
        };
        let output = QualificationRequestBindingOutput {
            schema: 1,
            input_digest: input.digest().unwrap(),
            resolved_body: json!({"another_provider_field":input.controller_canary_path}),
        };
        output.validate_for(&input).unwrap();
        let mut changed = input.clone();
        changed.template = json!({"provider_field":"e"});
        assert!(output.validate_for(&changed).is_err());
        assert_eq!(
            input.digest().unwrap(),
            digest(&crate::canonical_json(&input).unwrap())
        );
        let mut extra = serde_json::to_value(&input).unwrap();
        extra["json_pointer"] = json!("/provider_field");
        assert!(serde_json::from_value::<QualificationRequestBindingInput>(extra).is_err());
        let duplicate = br#"{"schema":1,"schema":1,"template":{},"controller_canary_path":"/x"}"#;
        assert!(
            crate::from_json_slice_strict::<QualificationRequestBindingInput>(
                duplicate,
                MAX_QUALIFICATION_REQUEST_BINDING_BYTES
            )
            .is_err()
        );
    }
    #[test]
    fn binding_wire_refuses_predecessors_and_unbounded_bodies() {
        let mut input = QualificationRequestBindingInput {
            schema: 0,
            template: json!({}),
            controller_canary_path: "/x".into(),
        };
        assert!(input.validate().is_err());
        input.schema = 1;
        input.template = json!({"value":"x".repeat(MAX_BODY_BYTES)});
        assert!(input.validate().is_err());
        input.template = json!([]);
        assert!(input.validate().is_err());
    }
}
