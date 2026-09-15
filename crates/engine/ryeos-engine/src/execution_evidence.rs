//! Data-driven execution-evidence projection through signed pure handlers.
//!
//! The projection contract is selected by an exact runtime or protocol
//! descriptor identity supplied from an admitted launch artifact. Projectors
//! can interpret contract-specific program/history data, but their output is
//! only a candidate: daemon owners must corroborate every call against
//! authoritative operation, child, capsule, and terminal state.

use std::collections::BTreeSet;
use std::time::Duration;

use ryeos_handler_protocol::{
    EXECUTION_EVIDENCE_MAX_CALL_ID_BYTES, ExecutionEvidenceDescribeRequest,
    ExecutionEvidenceDescribeResponse, ExecutionEvidenceLimitsWire,
    ExecutionEvidenceProjectRequest, ExecutionEvidenceProjectResponse,
    ExecutionEvidenceProjectorDeclWire, HandlerRequest, HandlerRequestEnvelope, HandlerResponse,
};

use crate::error::EngineError;
use crate::handlers::subprocess::run_handler_subprocess_bounded;
use crate::handlers::{HandlerServes, VerifiedExecutionEvidenceProjectorIdentity, VerifiedHandler};
use crate::resolution::TrustClass;

const EXECUTION_EVIDENCE_HANDLER_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn validate_execution_evidence_projector_declaration(
    declaration: &ExecutionEvidenceProjectorDeclWire,
) -> Result<(), String> {
    declaration.validate()?;
    let handler = crate::canonical_ref::CanonicalRef::parse(&declaration.handler)
        .map_err(|error| error.to_string())?;
    if handler.kind != "handler"
        || handler.suffix.is_some()
        || handler.to_string() != declaration.handler
    {
        return Err(
            "execution evidence handler must be a canonical unsuffixed handler ref".to_owned(),
        );
    }
    Ok(())
}

/// Exact signed projection contract and pure handler selected for one current
/// installed generation.
#[derive(Debug, Clone)]
pub struct ResolvedExecutionEvidenceProjector {
    pub projection_contract_ref: String,
    pub projection_contract_digest: String,
    pub declaration: ExecutionEvidenceProjectorDeclWire,
    pub projector: VerifiedExecutionEvidenceProjectorIdentity,
    handler: VerifiedHandler,
}

impl crate::engine::Engine {
    /// Resolve an optional projector from the exact signed runtime or protocol
    /// contract named by an admitted artifact. The two registries are probed
    /// by canonical identity; executable item kinds do not participate.
    pub fn resolve_execution_evidence_projector(
        &self,
        projection_contract_ref: &str,
        projection_contract_digest: &str,
    ) -> Result<Option<ResolvedExecutionEvidenceProjector>, EngineError> {
        self.with_checked_bundle_generation(|_| {
            resolve_projector_in_generation(
                self,
                projection_contract_ref,
                projection_contract_digest,
            )
        })
    }

    /// Ask the selected pure handler to describe statically required inline
    /// calls. The result is not authorization; callers must independently
    /// decode and authorize each normalized request.
    pub fn describe_execution_evidence(
        &self,
        projector: &ResolvedExecutionEvidenceProjector,
        request: ExecutionEvidenceDescribeRequest,
    ) -> Result<ExecutionEvidenceDescribeResponse, EngineError> {
        self.with_checked_bundle_generation(|_| {
            let current = require_same_current_projector(self, projector)?;
            require_exact_config(&current.declaration, &request.config)?;
            validate_program(&request.effective_program)?;
            let wire = HandlerRequest::ExecutionEvidenceDescribe(request);
            validate_request_size(&wire, &current.declaration.limits)?;
            let response = run_handler_subprocess_bounded(
                &current.handler,
                &wire,
                EXECUTION_EVIDENCE_HANDLER_TIMEOUT,
                self.parser_dispatcher.handler_registry().launch_runtime(),
                Some(current.declaration.limits.max_response_bytes as usize),
            )?;
            let HandlerResponse::ExecutionEvidenceDescribe { response } = response else {
                return Err(unexpected_response(
                    &current.projector.canonical_ref,
                    "execution_evidence_describe",
                ));
            };
            validate_describe_response(&response, &current.declaration.limits)?;
            Ok(response)
        })
    }

    /// Project a bounded completed terminal and exact event sequence into a
    /// result plus candidate call coordinates. Generic daemon code remains
    /// responsible for authenticating all returned coordinates.
    pub fn project_execution_evidence(
        &self,
        projector: &ResolvedExecutionEvidenceProjector,
        request: ExecutionEvidenceProjectRequest,
    ) -> Result<ExecutionEvidenceProjectResponse, EngineError> {
        self.with_checked_bundle_generation(|_| {
            let current = require_same_current_projector(self, projector)?;
            require_exact_config(&current.declaration, &request.config)?;
            validate_program(&request.effective_program)?;
            validate_events(&request, &current.declaration.limits)?;
            let wire = HandlerRequest::ExecutionEvidenceProject(request);
            validate_request_size(&wire, &current.declaration.limits)?;
            let response = run_handler_subprocess_bounded(
                &current.handler,
                &wire,
                EXECUTION_EVIDENCE_HANDLER_TIMEOUT,
                self.parser_dispatcher.handler_registry().launch_runtime(),
                Some(current.declaration.limits.max_response_bytes as usize),
            )?;
            let HandlerResponse::ExecutionEvidenceProject { response } = response else {
                return Err(unexpected_response(
                    &current.projector.canonical_ref,
                    "execution_evidence_project",
                ));
            };
            validate_project_response(&response, &current.declaration.limits)?;
            Ok(response)
        })
    }
}

fn resolve_projector_in_generation(
    engine: &crate::engine::Engine,
    contract_ref: &str,
    contract_digest: &str,
) -> Result<Option<ResolvedExecutionEvidenceProjector>, EngineError> {
    let canonical = validate_projection_contract_selector(contract_ref, contract_digest)?;

    let runtime = engine.runtimes.lookup_by_ref(&canonical);
    let protocol = engine.protocols.require(contract_ref).ok();
    let (observed_digest, declaration, owner_trust) = match (runtime, protocol) {
        (Some(_), Some(_)) => {
            return Err(EngineError::Internal(format!(
                "execution evidence projection contract '{contract_ref}' is ambiguous across descriptor registries"
            )));
        }
        (Some(runtime), None) => (
            runtime.raw_content_digest.as_str(),
            runtime.yaml.execution_evidence.as_ref(),
            runtime.trust_class,
        ),
        (None, Some(protocol)) => (
            protocol.raw_content_digest.as_str(),
            protocol.descriptor.execution_evidence.as_ref(),
            protocol.trust_class,
        ),
        (None, None) => {
            return Err(EngineError::Internal(format!(
                "execution evidence projection contract '{contract_ref}' is not registered"
            )));
        }
    };
    if observed_digest != contract_digest {
        return Err(EngineError::Internal(format!(
            "execution evidence projection contract '{contract_ref}' digest differs from the admitted artifact"
        )));
    }
    if owner_trust != TrustClass::TrustedBundle {
        return Err(EngineError::Internal(format!(
            "execution evidence projection contract '{contract_ref}' is not owned by a trusted bundle"
        )));
    }
    let Some(declaration) = declaration else {
        return Ok(None);
    };
    validate_execution_evidence_projector_declaration(declaration)
        .map_err(EngineError::Internal)?;
    let handlers = engine.parser_dispatcher.handler_registry();
    let handler = handlers
        .ensure_serves(
            &declaration.handler,
            HandlerServes::ExecutionEvidenceProjector,
        )
        .map_err(|error| EngineError::Handler(Box::new(error)))?
        .clone();
    if handler.trust_class() != TrustClass::TrustedBundle {
        return Err(EngineError::Internal(format!(
            "execution evidence projector '{}' is not owned by a trusted bundle",
            declaration.handler
        )));
    }
    let projector = handler
        .execution_evidence_projector_identity()
        .map_err(|error| EngineError::Handler(Box::new(error)))?;
    Ok(Some(ResolvedExecutionEvidenceProjector {
        projection_contract_ref: contract_ref.to_owned(),
        projection_contract_digest: contract_digest.to_owned(),
        declaration: declaration.clone(),
        projector,
        handler,
    }))
}

fn validate_projection_contract_selector(
    contract_ref: &str,
    contract_digest: &str,
) -> Result<crate::canonical_ref::CanonicalRef, EngineError> {
    if !lillux::valid_hash(contract_digest) {
        return Err(EngineError::Internal(
            "execution evidence projection contract digest is not canonical".to_owned(),
        ));
    }
    let canonical = crate::canonical_ref::CanonicalRef::parse(contract_ref)?;
    if canonical.suffix.is_some() || canonical.to_string() != contract_ref {
        return Err(EngineError::Internal(
            "execution evidence projection contract ref is not canonical and unsuffixed".to_owned(),
        ));
    }
    Ok(canonical)
}

fn require_same_current_projector(
    engine: &crate::engine::Engine,
    expected: &ResolvedExecutionEvidenceProjector,
) -> Result<ResolvedExecutionEvidenceProjector, EngineError> {
    let current = resolve_projector_in_generation(
        engine,
        &expected.projection_contract_ref,
        &expected.projection_contract_digest,
    )?
    .ok_or_else(|| {
        EngineError::Internal(format!(
            "execution evidence projection contract '{}' no longer declares a projector",
            expected.projection_contract_ref
        ))
    })?;
    if current.declaration != expected.declaration || current.projector != expected.projector {
        return Err(EngineError::Internal(format!(
            "execution evidence projector for '{}' changed within the installed generation",
            expected.projection_contract_ref
        )));
    }
    Ok(current)
}

fn require_exact_config(
    declaration: &ExecutionEvidenceProjectorDeclWire,
    supplied: &serde_json::Value,
) -> Result<(), EngineError> {
    if &declaration.config != supplied {
        return Err(EngineError::Internal(
            "execution evidence request substituted projector config".to_owned(),
        ));
    }
    Ok(())
}

fn validate_request_size(
    request: &HandlerRequest,
    limits: &ExecutionEvidenceLimitsWire,
) -> Result<(), EngineError> {
    let bytes = serde_json::to_vec(&HandlerRequestEnvelope::new(request.clone()))
        .map_err(|error| {
            EngineError::Internal(format!("encode execution evidence request: {error}"))
        })?
        .len();
    if bytes > limits.max_request_bytes as usize {
        return Err(EngineError::Internal(format!(
            "execution evidence request is {bytes} bytes (max {})",
            limits.max_request_bytes
        )));
    }
    Ok(())
}

fn validate_program(
    program: &ryeos_handler_protocol::ExecutionEvidenceProgramWire,
) -> Result<(), EngineError> {
    let canonical = crate::canonical_ref::CanonicalRef::parse(&program.canonical_ref)?;
    if canonical.suffix.is_some() || canonical.to_string() != program.canonical_ref {
        return Err(EngineError::Internal(
            "execution evidence program ref is not canonical and unsuffixed".to_owned(),
        ));
    }
    if !lillux::valid_hash(&program.effective_definition_digest) {
        return Err(EngineError::Internal(
            "execution evidence program effective-definition digest is not canonical".to_owned(),
        ));
    }
    if program.ancestor_requested_ids.len() > 256
        || program.ancestor_requested_ids.iter().any(|value| {
            value.is_empty()
                || value.len() > 512
                || value.trim() != value
                || value.chars().any(char::is_control)
        })
    {
        return Err(EngineError::Internal(
            "execution evidence ancestor identities are not canonical and bounded".to_owned(),
        ));
    }
    Ok(())
}

fn validate_events(
    request: &ExecutionEvidenceProjectRequest,
    limits: &ExecutionEvidenceLimitsWire,
) -> Result<(), EngineError> {
    if request.events.len() > usize::from(limits.max_events) {
        return Err(EngineError::Internal(format!(
            "execution evidence event count is {} (max {})",
            request.events.len(),
            limits.max_events
        )));
    }
    let mut prior = None;
    for event in &request.events {
        if prior.is_some_and(|value| event.thread_seq <= value) {
            return Err(EngineError::Internal(
                "execution evidence events are not in strictly increasing thread sequence"
                    .to_owned(),
            ));
        }
        prior = Some(event.thread_seq);
        if event.event_type.is_empty()
            || event.event_type.len() > 128
            || event.event_type.trim() != event.event_type
            || event.event_type.chars().any(char::is_control)
            || !event.payload.is_object()
        {
            return Err(EngineError::Internal(
                "execution evidence event type or payload is not canonical".to_owned(),
            ));
        }
        let bytes = serde_json::to_vec(event)
            .map_err(|error| {
                EngineError::Internal(format!("encode execution evidence event: {error}"))
            })?
            .len();
        if bytes > limits.max_event_bytes as usize {
            return Err(EngineError::Internal(format!(
                "execution evidence event is {bytes} bytes (max {})",
                limits.max_event_bytes
            )));
        }
    }
    Ok(())
}

fn validate_describe_response(
    response: &ExecutionEvidenceDescribeResponse,
    limits: &ExecutionEvidenceLimitsWire,
) -> Result<(), EngineError> {
    if let ExecutionEvidenceDescribeResponse::Described { required_calls } = response {
        validate_calls(required_calls, limits, |call| &call.call_id)?;
        if required_calls.iter().any(|call| !call.request.is_object()) {
            return Err(EngineError::Internal(
                "execution evidence required call request must be an object".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_project_response(
    response: &ExecutionEvidenceProjectResponse,
    limits: &ExecutionEvidenceLimitsWire,
) -> Result<(), EngineError> {
    if let ExecutionEvidenceProjectResponse::Projected { calls, .. } = response {
        validate_calls(calls, limits, |call| &call.call_id)?;
        if calls.iter().any(|call| {
            !lillux::valid_hash(&call.operation_id)
                || !lillux::valid_hash(&call.action_digest)
                || !lillux::valid_hash(&call.result_digest)
                || call.child_thread_id.is_empty()
                || call.child_thread_id.len() > 256
                || call.child_thread_id.trim() != call.child_thread_id
                || call.child_thread_id.chars().any(char::is_control)
        }) {
            return Err(EngineError::Internal(
                "execution evidence candidate coordinates are not canonical".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_calls<T: serde::Serialize>(
    calls: &[T],
    limits: &ExecutionEvidenceLimitsWire,
    call_id: impl Fn(&T) -> &str,
) -> Result<(), EngineError> {
    if calls.len() > usize::from(limits.max_calls) {
        return Err(EngineError::Internal(format!(
            "execution evidence call count is {} (max {})",
            calls.len(),
            limits.max_calls
        )));
    }
    let mut seen = BTreeSet::new();
    for call in calls {
        let id = call_id(call);
        if id.is_empty()
            || id.len() > EXECUTION_EVIDENCE_MAX_CALL_ID_BYTES
            || id.trim() != id
            || id.chars().any(char::is_control)
            || !seen.insert(id)
        {
            return Err(EngineError::Internal(
                "execution evidence call_id is empty, oversized, non-canonical, or duplicated"
                    .to_owned(),
            ));
        }
        let bytes = serde_json::to_vec(call)
            .map_err(|error| {
                EngineError::Internal(format!("encode execution evidence call: {error}"))
            })?
            .len();
        if bytes > limits.max_call_bytes as usize {
            return Err(EngineError::Internal(format!(
                "execution evidence call '{id}' is {bytes} bytes (max {})",
                limits.max_call_bytes
            )));
        }
    }
    Ok(())
}

fn unexpected_response(handler: &str, expected: &str) -> EngineError {
    EngineError::HandlerProtocolViolation {
        handler: handler.to_owned(),
        detail: format!("expected {expected} response"),
    }
}

#[cfg(test)]
mod tests {
    use ryeos_handler_protocol::{
        ExecutionEvidenceCandidateCallWire, ExecutionEvidenceEventWire,
        ExecutionEvidenceProgramWire, ExecutionEvidenceRequiredCallWire,
        ExecutionEvidenceTerminalWire, LaunchComposedViewWire,
    };
    use serde_json::json;

    use super::*;

    fn limits() -> ExecutionEvidenceLimitsWire {
        ExecutionEvidenceLimitsWire {
            max_request_bytes: 4096,
            max_response_bytes: 4096,
            max_events: 2,
            max_event_bytes: 1024,
            max_calls: 2,
            max_call_bytes: 1024,
        }
    }

    fn program() -> ExecutionEvidenceProgramWire {
        ExecutionEvidenceProgramWire {
            canonical_ref: "tool:test/verifier".to_owned(),
            effective_definition_digest: "1".repeat(64),
            composed: LaunchComposedViewWire {
                composed: json!({"command": ["bin/verifier"]}),
                derived: Default::default(),
                policy_facts: Default::default(),
            },
            ancestor_requested_ids: vec!["tool:test/verifier".to_owned()],
        }
    }

    #[test]
    fn projection_selector_requires_exact_unsuffixed_ref_and_digest() {
        validate_projection_contract_selector("runtime:test/exact", &"a".repeat(64)).unwrap();
        assert!(
            validate_projection_contract_selector("runtime:test/exact@current", &"a".repeat(64))
                .is_err()
        );
        assert!(validate_projection_contract_selector("runtime:test/exact", "short").is_err());
    }

    #[test]
    fn config_substitution_and_oversized_requests_are_refused() {
        let declaration = ExecutionEvidenceProjectorDeclWire {
            handler: "handler:test/evidence".to_owned(),
            config: json!({"mode": "exact"}),
            limits: limits(),
        };
        assert!(require_exact_config(&declaration, &json!({"mode": "changed"})).is_err());

        let request = HandlerRequest::ExecutionEvidenceDescribe(ExecutionEvidenceDescribeRequest {
            config: json!({"padding": "x".repeat(4096)}),
            effective_program: program(),
        });
        assert!(validate_request_size(&request, &limits()).is_err());
    }

    #[test]
    fn projector_declaration_requires_handler_kind_and_valid_limits() {
        let mut declaration = ExecutionEvidenceProjectorDeclWire {
            handler: "parser:test/evidence".to_owned(),
            config: json!({}),
            limits: limits(),
        };
        assert!(validate_execution_evidence_projector_declaration(&declaration).is_err());

        declaration.handler = "handler:test/evidence".to_owned();
        declaration.limits.max_calls = 0;
        assert!(validate_execution_evidence_projector_declaration(&declaration).is_err());
    }

    #[test]
    fn describe_response_refuses_duplicate_or_non_object_calls() {
        let duplicate = ExecutionEvidenceDescribeResponse::Described {
            required_calls: vec![
                ExecutionEvidenceRequiredCallWire {
                    call_id: "probe".to_owned(),
                    request: json!({"action": "run"}),
                },
                ExecutionEvidenceRequiredCallWire {
                    call_id: "probe".to_owned(),
                    request: json!({"action": "run-again"}),
                },
            ],
        };
        assert!(validate_describe_response(&duplicate, &limits()).is_err());

        let scalar = ExecutionEvidenceDescribeResponse::Described {
            required_calls: vec![ExecutionEvidenceRequiredCallWire {
                call_id: "probe".to_owned(),
                request: json!("not-a-request"),
            }],
        };
        assert!(validate_describe_response(&scalar, &limits()).is_err());
    }

    #[test]
    fn project_response_refuses_malformed_candidate_coordinates() {
        let response = ExecutionEvidenceProjectResponse::Projected {
            result: json!({"accepted": true}),
            calls: vec![ExecutionEvidenceCandidateCallWire {
                call_id: "probe".to_owned(),
                operation_id: "not-a-hash".to_owned(),
                action_digest: "2".repeat(64),
                child_thread_id: "thread:test".to_owned(),
                result_digest: "3".repeat(64),
            }],
        };
        assert!(validate_project_response(&response, &limits()).is_err());
    }

    #[test]
    fn event_sequence_and_per_event_bounds_are_enforced() {
        let request = ExecutionEvidenceProjectRequest {
            config: json!({}),
            effective_program: program(),
            terminal: ExecutionEvidenceTerminalWire {
                status: "completed".to_owned(),
                result: json!(null),
                error: json!(null),
                artifacts: Vec::new(),
            },
            events: vec![
                ExecutionEvidenceEventWire {
                    thread_seq: 2,
                    event_type: "second".to_owned(),
                    payload: json!({}),
                },
                ExecutionEvidenceEventWire {
                    thread_seq: 1,
                    event_type: "first".to_owned(),
                    payload: json!({}),
                },
            ],
        };
        assert!(validate_events(&request, &limits()).is_err());
    }
}
