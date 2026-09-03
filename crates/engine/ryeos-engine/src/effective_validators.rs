//! Boot-bound kind effective-validator handlers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ryeos_handler_protocol::{
    EffectiveValidateRequest, EffectiveValidateResponse, HandlerRequest, HandlerResponse,
    LaunchComposedViewWire,
};

use crate::effective_program::EffectiveValidationSuccess;
use crate::error::EngineError;
use crate::handlers::subprocess::{HandlerLaunchRuntime, run_handler_subprocess};
use crate::handlers::{HandlerRegistry, HandlerServes, VerifiedHandler};
use crate::isolation::IsolationRuntime;
use crate::kind_registry::KindRegistry;
use crate::resolution::ResolutionOutput;

const EFFECTIVE_VALIDATE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct BoundEffectiveValidator {
    handler: VerifiedHandler,
    config: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct EffectiveValidatorRegistry {
    by_kind: HashMap<String, BoundEffectiveValidator>,
    launch_runtime: Option<Arc<HandlerLaunchRuntime>>,
}

impl EffectiveValidatorRegistry {
    pub fn from_kinds(
        kinds: &KindRegistry,
        handlers: &HandlerRegistry,
        isolation: Arc<IsolationRuntime>,
        bundle_roots: &[PathBuf],
    ) -> Result<Self, EngineError> {
        let mut by_kind = HashMap::new();
        for kind in kinds.kinds() {
            let schema = kinds.get(kind).expect("kind came from registry");
            let Some(declaration) = schema
                .execution
                .as_ref()
                .and_then(|execution| execution.effective_validator.as_ref())
            else {
                continue;
            };
            let handler = handlers
                .ensure_serves(&declaration.handler, HandlerServes::EffectiveValidator)
                .map_err(|error| EngineError::SchemaLoaderError {
                    reason: format!(
                        "kind `{kind}` effective validator `{}` is unavailable: {error}",
                        declaration.handler
                    ),
                })?;
            if handler.trust_class() != crate::resolution::TrustClass::TrustedBundle {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "kind `{kind}` effective validator `{}` is not trusted_bundle",
                        declaration.handler
                    ),
                });
            }
            by_kind.insert(
                kind.to_string(),
                BoundEffectiveValidator {
                    handler: handler.clone(),
                    config: declaration.config.clone(),
                },
            );
        }
        Ok(Self {
            by_kind,
            launch_runtime: Some(Arc::new(HandlerLaunchRuntime::new(
                isolation,
                bundle_roots.to_vec(),
            ))),
        })
    }

    pub fn validate(
        &self,
        kind: &str,
        resolution: &ResolutionOutput,
    ) -> Result<EffectiveValidationSuccess, EngineError> {
        let Some(bound) = self.by_kind.get(kind) else {
            return Ok(EffectiveValidationSuccess::no_declared_validator());
        };
        let launch_runtime = self.launch_runtime.as_deref().ok_or_else(|| {
            EngineError::Internal("effective validator registry has no launch runtime".to_string())
        })?;
        let request = HandlerRequest::EffectiveValidate(EffectiveValidateRequest {
            validator_config: bound.config.clone(),
            canonical_ref: resolution.root.resolved_ref.clone(),
            composed: LaunchComposedViewWire {
                composed: resolution.composed.composed.clone(),
                derived: resolution.composed.derived.clone().into_iter().collect(),
                policy_facts: resolution
                    .composed
                    .policy_facts
                    .clone()
                    .into_iter()
                    .collect(),
            },
            ancestor_requested_ids: resolution
                .ancestors
                .iter()
                .map(|ancestor| ancestor.requested_id.clone())
                .collect(),
        });
        match run_handler_subprocess(
            &bound.handler,
            &request,
            EFFECTIVE_VALIDATE_TIMEOUT,
            launch_runtime,
        )? {
            HandlerResponse::EffectiveValidate {
                response:
                    EffectiveValidateResponse::Valid {
                        normalized,
                        effect_authorizations,
                    },
            } => EffectiveValidationSuccess::from_normalized(&normalized, effect_authorizations),
            HandlerResponse::EffectiveValidate {
                response: EffectiveValidateResponse::Invalid { code, message },
            } => Err(EngineError::EffectiveValidationRejected {
                canonical_ref: resolution.root.resolved_ref.clone(),
                code,
                message,
            }),
            response => Err(EngineError::HandlerProtocolViolation {
                handler: bound.handler.canonical_ref().to_string(),
                detail: format!("unexpected effective-validator response: {response:?}"),
            }),
        }
    }
}
