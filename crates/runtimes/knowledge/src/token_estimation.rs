//! Token-estimation policy and estimator implementations for knowledge compose.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::budget::estimate_tokens;
use crate::types::KnowledgeError;

const DEFAULT_CHARS_PER_TOKEN: f64 = 4.0;
const DEFAULT_RESERVE_RATIO: f64 = 0.10;
const EXPECTED_CATEGORY: &str = "knowledge-runtime";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenEstimationRuntimeConfig {
    #[serde(default)]
    pub category: Option<String>,
    pub policy: TokenEstimationPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenEstimationPolicy {
    Heuristic {
        chars_per_token: f64,
        reserve_ratio: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct TokenEstimatorMetadata {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chars_per_token: Option<f64>,
    pub reserve_ratio: f64,
}

pub struct ResolvedTokenEstimator {
    kind: EstimatorKind,
    reserve_ratio: f64,
    metadata: TokenEstimatorMetadata,
}

enum EstimatorKind {
    Heuristic { chars_per_token: f64 },
}

impl ResolvedTokenEstimator {
    pub fn from_runtime_config(
        runtime_config: &BTreeMap<String, serde_json::Value>,
    ) -> Result<Self, KnowledgeError> {
        let token_estimation = runtime_config.get("token_estimation").ok_or_else(|| {
            KnowledgeError::TokenEstimation(
                "required runtime config `token_estimation` was not provided".to_string(),
            )
        })?;

        let config: TokenEstimationRuntimeConfig = serde_json::from_value(token_estimation.clone())
            .map_err(|e| KnowledgeError::TokenEstimation(format!("invalid token_estimation config: {e}")))?;
        if let Some(category) = config.category.as_deref() {
            if category != EXPECTED_CATEGORY {
                return Err(KnowledgeError::TokenEstimation(format!(
                    "token_estimation config category must be `{EXPECTED_CATEGORY}`, got `{category}`"
                )));
            }
        }
        Self::build(&config.policy)
    }

    pub fn estimate(&self, text: &str) -> usize {
        let base = match &self.kind {
            EstimatorKind::Heuristic { chars_per_token } => {
                if is_default_heuristic(*chars_per_token, self.reserve_ratio) {
                    return estimate_tokens(text);
                }
                let chars = text.chars().count() as f64;
                (chars / chars_per_token).floor() as usize
            }
        };
        apply_reserve(base, self.reserve_ratio)
    }

    pub fn metadata(&self) -> &TokenEstimatorMetadata {
        &self.metadata
    }

    fn build(policy: &TokenEstimationPolicy) -> Result<Self, KnowledgeError> {
        match policy {
            TokenEstimationPolicy::Heuristic {
                chars_per_token,
                reserve_ratio,
            } => {
                validate_positive("chars_per_token", *chars_per_token)?;
                validate_non_negative("reserve_ratio", *reserve_ratio)?;
                Ok(Self {
                    kind: EstimatorKind::Heuristic {
                        chars_per_token: *chars_per_token,
                    },
                    reserve_ratio: *reserve_ratio,
                    metadata: TokenEstimatorMetadata {
                        provider: "heuristic".to_string(),
                        encoding: None,
                        model: None,
                        chars_per_token: Some(*chars_per_token),
                        reserve_ratio: *reserve_ratio,
                    },
                })
            }
        }
    }
}

fn is_default_heuristic(chars_per_token: f64, reserve_ratio: f64) -> bool {
    (chars_per_token - DEFAULT_CHARS_PER_TOKEN).abs() < f64::EPSILON
        && (reserve_ratio - DEFAULT_RESERVE_RATIO).abs() < f64::EPSILON
}

fn apply_reserve(base: usize, reserve_ratio: f64) -> usize {
    if base == 0 {
        return 1;
    }
    ((base as f64) * (1.0 + reserve_ratio)).floor().max(1.0) as usize
}

fn validate_positive(name: &str, value: f64) -> Result<(), KnowledgeError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(KnowledgeError::TokenEstimation(format!(
            "{name} must be a positive finite number"
        )))
    }
}

fn validate_non_negative(name: &str, value: f64) -> Result<(), KnowledgeError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(KnowledgeError::TokenEstimation(format!(
            "{name} must be a non-negative finite number"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_runtime_config_errors() {
        assert!(ResolvedTokenEstimator::from_runtime_config(&BTreeMap::new()).is_err());
    }

    #[test]
    fn explicit_heuristic_config_changes_estimates() {
        let config = json!({
            "token_estimation": {
                "policy": {"provider": "heuristic", "chars_per_token": 2.0, "reserve_ratio": 0.0}
            }
        });
        let estimator = ResolvedTokenEstimator::from_runtime_config(&runtime_config(config)).unwrap();
        assert_eq!(estimator.estimate(&"a".repeat(40)), 20);
    }

    #[test]
    fn unsupported_provider_is_rejected() {
        let config = json!({
            "token_estimation": {
                "policy": {"provider": "tokenizer", "encoding": "unknown", "reserve_ratio": 0.0}
            }
        });
        assert!(ResolvedTokenEstimator::from_runtime_config(&runtime_config(config)).is_err());
    }

    fn runtime_config(value: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        let mut config = BTreeMap::new();
        config.insert(
            "token_estimation".to_string(),
            value
                .get("token_estimation")
                .expect("test config has token_estimation")
                .clone(),
        );
        config
    }
}
