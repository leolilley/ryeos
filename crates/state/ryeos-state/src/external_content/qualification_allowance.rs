//! Signed consumer selection for producing content-qualification evidence.
//!
//! This is neither an acquisition permission nor a runtime-readiness claim.
//! Admission must resolve these references in the consumer's authenticated
//! generation and independently verify the resulting execution evidence.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentQualificationAllowance {
    pub activation_ref: String,
    pub policy_ref: String,
    pub required_claims: Vec<String>,
}

impl ContentQualificationAllowance {
    pub fn validate_policy_source(
        &self,
        source: &super::products::qualification::ProductQualificationPolicySource,
    ) -> anyhow::Result<()> {
        self.validate()?;
        source.validate()?;
        anyhow::ensure!(
            source.canonical_ref == self.policy_ref,
            "qualification allowance differs from resolved policy source"
        );
        anyhow::ensure!(
            self.required_claims.iter().all(|claim| source
                .policy
                .allowed_claims
                .binary_search(claim)
                .is_ok()),
            "consumer requires a claim outside the selected signed policy"
        );
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, value) in [
            ("qualification activation", &self.activation_ref),
            ("qualification policy", &self.policy_ref),
        ] {
            super::products::validate_canonical_unsuffixed_ref(label, value)?;
        }
        super::products::qualification::validate_claims(&self.required_claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_claims_and_policy_identity_must_agree() {
        let source = super::super::products::qualification::tests::launch_purpose().policy_source;
        let mut allowance = ContentQualificationAllowance {
            activation_ref: "config:fixture/activation".into(),
            policy_ref: source.canonical_ref.clone(),
            required_claims: source.policy.allowed_claims.clone(),
        };
        allowance.validate_policy_source(&source).unwrap();
        allowance.policy_ref = "config:fixture/different-policy".into();
        assert!(allowance.validate_policy_source(&source).is_err());
        allowance.policy_ref = source.canonical_ref.clone();
        allowance.required_claims = vec!["unapproved".into()];
        assert!(allowance.validate_policy_source(&source).is_err());
    }

    #[test]
    fn allowance_is_explicit_bounded_and_kind_independent() {
        let mut allowance = ContentQualificationAllowance {
            activation_ref: "config:fixture/activation".into(),
            policy_ref: "config:fixture/policy".into(),
            required_claims: vec!["interactive".into(), "settled".into()],
        };
        allowance.validate().unwrap();
        allowance.required_claims.reverse();
        assert!(allowance.validate().is_err());
        allowance.required_claims.clear();
        assert!(allowance.validate().is_err());
    }

    #[test]
    fn allowance_refuses_unrecognized_authority_fields() {
        assert!(
            serde_json::from_value::<ContentQualificationAllowance>(serde_json::json!({
                "activation_ref": "config:fixture/activation",
                "policy_ref": "config:fixture/policy",
                "required_claims": ["settled"],
                "qualified": true
            }))
            .is_err()
        );
    }
}
