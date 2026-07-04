//! `items.effective` — resolve, verify, compose, and return an effective item.
//!
//! Works for any item kind (executable or not). The engine owns the
//! resolution/trust/composition semantics; this handler is intentionally
//! only a typed service wrapper — plus, for surfaces, the server-side
//! view embedding (`crate::surface_views`) so clients receive surfaces
//! with their bound views already resolved.
//!
//! ## Error codes
//!
//! The handler maps engine errors to `HandlerError` variants whose messages
//! carry a stable prefix that clients can branch on:
//!
//! | Prefix | HTTP | Meaning |
//! |--------|------|---------|
//! | `invalid canonical ref` | 400 | Malformed canonical ref string |
//! | `wrong_kind:` | 400 | `expected_kind` guard failed |
//! | `untrusted:` | 403 | Signer not in trust store |
//! | `composition_failed:` | 400 | Composer chain error |
//! | `parse_failed:` | 400 | Parser produced invalid output |
//! | `contract_violation:` | 400 | Structured envelope — see below |
//! | (none) | 404 | Item not found in any space |
//! | (none) | 500 | Unexpected internal error |
//!
//! ## Contract violation envelope
//!
//! When the composed value fails validation against the kind's contract,
//! the response is a structured JSON object:
//!
//! ```text
//! {
//!   "error": "<summary message>",
//!   "error_code": "contract_violation",
//!   "details": {
//!     "errors": [
//!       { "path": "<field>", "code": "<code>", "expected": "<...>", "found": "<...>" },
//!       ...
//!     ],
//!     "warnings": [
//!       { "path": "<field>", "code": "<code>", "expected": "<...>", "found": "<...>" },
//!       ...
//!     ]
//!   }
//! }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::{EffectiveItem, EffectiveItemDiagnostic, EffectiveItemRequest};
use ryeos_engine::error::EngineError;
use ryeos_engine::resolution::{AsLaunchedResolutionDigest, ResolutionDigestNode};
use ryeos_executor::executor::ServiceAvailability;
use ryeos_runtime::events::RuntimeEventType;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Canonical item ref to resolve, e.g. "surface:ryeos/studio/base".
    pub canonical_ref: String,

    /// Optional project path for project-space resolution.
    /// When absent, only bundle roots are searched.
    #[serde(default)]
    pub project_path: Option<String>,

    /// Optional guardrail for clients that know which kind they expect.
    #[serde(default)]
    pub expected_kind: Option<String>,

    /// Thread-scoped mode. When set, the response carries an `as_launched`
    /// block: the digest this thread persisted at launch (extends chain as
    /// composed, policy facts, effective trust class), rendered as truth, with
    /// each ancestor flagged `changed` where its content digest differs from
    /// the fresh (now) resolution above — the drift the caveat used to only
    /// warn about.
    #[serde(default)]
    pub thread_id: Option<String>,
}

pub async fn handle(req: Request, _ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let item_ref = CanonicalRef::parse(&req.canonical_ref).map_err(|e| {
        HandlerError::BadRequest(format!(
            "invalid canonical ref '{}': {e}",
            req.canonical_ref
        ))
    })?;

    let project_root = req.project_path.map(std::path::PathBuf::from);

    let mut effective = state
        .engine
        .effective_item(EffectiveItemRequest {
            item_ref,
            expected_kind: req.expected_kind,
            project_root: project_root.clone(),
        })
        .map_err(map_engine_error)?;

    // A surface binds views by ref and never defines them. Embed each
    // bound view's composed value server-side so renderers receive one
    // complete payload — no per-view follow-up round-trips. A view that
    // fails to resolve embeds a degraded entry (the pane renders the
    // reason) and additionally surfaces as a warn diagnostic; per-view
    // failures never fail the whole surface.
    if effective.kind == "surface" {
        let failures = crate::surface_views::embed_surface_views(
            &state.engine,
            project_root.as_deref(),
            &mut effective.composed_value,
        );
        for (view_ref, reason) in failures {
            effective.diagnostics.push(EffectiveItemDiagnostic {
                level: "warn".to_string(),
                message: format!("view {view_ref} unavailable: {reason}"),
            });
        }
    }

    // Thread-scoped mode: attach the launch-time digest (truth) alongside the
    // fresh resolution (now), with per-ancestor drift flags.
    let as_launched = req
        .thread_id
        .as_deref()
        .map(|thread_id| as_launched_block(&state, thread_id, &effective));

    let mut body = serde_json::to_value(effective)?;
    if let (Value::Object(map), Some(as_launched)) = (&mut body, as_launched) {
        map.insert("as_launched".to_string(), as_launched);
    }
    Ok(body)
}

/// Build the `as_launched` block for a thread: the persisted launch digest
/// rendered as truth, each node flagged `changed` where its content digest
/// differs from the freshly-resolved `effective` (the drift comparison). When
/// no digest event was recorded (or it cannot be read) the block is
/// `{ present: false, reason }` so the view degrades honestly rather than
/// silently showing the fresh resolution as if it were the launched one.
fn as_launched_block(state: &AppState, thread_id: &str, effective: &EffectiveItem) -> Value {
    let digest = match read_launch_digest(state, thread_id) {
        Ok(Some(digest)) => digest,
        Ok(None) => {
            return json!({
                "present": false,
                "reason": "no as-launched resolution digest recorded for this thread",
            });
        }
        Err(e) => return json!({ "present": false, "reason": e.to_string() }),
    };

    // Current (freshly-resolved) content digests by resolved ref, to localize
    // drift against the launched chain.
    let mut current: HashMap<&str, &str> = HashMap::new();
    current.insert(
        effective.provenance.root.resolved_ref.as_str(),
        effective.provenance.root.raw_content_digest.as_str(),
    );
    for anc in &effective.provenance.ancestors {
        current.insert(
            anc.resolved_ref.as_str(),
            anc.raw_content_digest.as_str(),
        );
    }

    let (root_json, root_changed) = digest_node_json(&digest.root, &current);
    let mut drift = root_changed;
    let ancestors: Vec<Value> = digest
        .ancestors
        .iter()
        .map(|node| {
            let (node_json, changed) = digest_node_json(node, &current);
            drift |= changed;
            node_json
        })
        .collect();

    json!({
        "present": true,
        "root": root_json,
        "ancestors": ancestors,
        "effective_trust_class": digest.effective_trust_class,
        "policy_facts": digest.policy_facts,
        "drift": drift,
    })
}

/// A launched digest node as a render row plus whether it drifted: `changed`
/// is true when the fresh resolution no longer carries this ref at the launched
/// content digest (edited, republished, or dropped from the chain).
fn digest_node_json(node: &ResolutionDigestNode, current: &HashMap<&str, &str>) -> (Value, bool) {
    let changed =
        current.get(node.resolved_ref.as_str()).copied() != Some(node.raw_content_digest.as_str());
    (
        json!({
            "requested_id": node.requested_id,
            "resolved_ref": node.resolved_ref,
            "trust_class": node.trust_class,
            "raw_content_digest": node.raw_content_digest,
            "changed": changed,
        }),
        changed,
    )
}

/// Read the thread's persisted `as_launched_resolution` digest from the braid.
/// The digest is emitted once, at launch, so it sits near the head of the
/// thread's events; the first page is enough to find it.
fn read_launch_digest(
    state: &AppState,
    thread_id: &str,
) -> Result<Option<AsLaunchedResolutionDigest>> {
    let replay = state.events.replay(&EventReplayParams {
        chain_root_id: None,
        thread_id: Some(thread_id.to_string()),
        after_chain_seq: None,
        limit: 500,
    })?;
    let launch_type = RuntimeEventType::AsLaunchedResolution.as_str();
    for record in replay.events {
        if record.event_type == launch_type {
            return Ok(Some(serde_json::from_value(record.payload)?));
        }
    }
    Ok(None)
}

/// Map typed engine errors to HTTP-appropriate handler errors with
/// stable error codes. Renderers can branch on these instead of
/// parsing error messages.
fn map_engine_error(e: EngineError) -> HandlerError {
    match &e {
        EngineError::EffectiveItemNotFound { canonical_ref: _ } => HandlerError::NotFound,
        EngineError::EffectiveItemWrongKind {
            expected,
            found,
            canonical_ref,
        } => HandlerError::BadRequest(format!(
            "wrong_kind: expected `{expected}`, got `{found}` for `{canonical_ref}`"
        )),
        EngineError::EffectiveItemUntrusted {
            canonical_ref,
            fingerprint,
        } => HandlerError::Forbidden(format!(
            "untrusted: `{canonical_ref}` (fingerprint: {fingerprint})"
        )),
        EngineError::EffectiveItemCompositionFailed { reason, .. } => {
            HandlerError::BadRequest(format!("composition_failed: {reason}"))
        }
        EngineError::EffectiveItemParseFailed { reason, .. } => {
            HandlerError::BadRequest(format!("parse_failed: {reason}"))
        }
        EngineError::ComposedValueContractViolation {
            canonical_ref,
            report,
        } => {
            let error_count = report.errors.len();
            let warn_count = report.warnings.len();
            let error_suffix = if error_count == 1 { "" } else { "s" };
            let warning_suffix = if warn_count == 1 { "" } else { "s" };
            let summary = format!(
                "contract_violation: `{canonical_ref}` ({error_count} error{error_suffix}, {warn_count} warning{warning_suffix})",
            );

            let violations_to_json =
                |violations: &[ryeos_engine::contracts::InstanceViolation]| {
                    violations
                        .iter()
                        .map(|v| {
                            serde_json::json!({
                                "path": v.path,
                                "code": v.code.to_string(),
                                "expected": v.expected,
                                "found": v.found,
                            })
                        })
                        .collect::<Vec<_>>()
                };

            let body = serde_json::json!({
                "error": summary,
                "error_code": "contract_violation",
                "details": {
                    "errors": violations_to_json(&report.errors),
                    "warnings": violations_to_json(&report.warnings),
                }
            });

            HandlerError::Structured {
                code: "contract_violation".into(),
                body,
            }
        }
        _ => HandlerError::Internal(e.to_string()),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:items/effective",
    endpoint: "items.effective",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use ryeos_engine::contracts::{
        InstanceValidationReport, InstanceViolation, InstanceViolationCode,
    };
    use ryeos_engine::error::EngineError;

    use ryeos_engine::resolution::{ResolutionDigestNode, TrustClass};

    use super::{digest_node_json, map_engine_error};
    use std::collections::HashMap;

    fn node(resolved_ref: &str, digest: &str) -> ResolutionDigestNode {
        ResolutionDigestNode {
            requested_id: resolved_ref.to_string(),
            resolved_ref: resolved_ref.to_string(),
            trust_class: TrustClass::TrustedBundle,
            raw_content_digest: digest.to_string(),
        }
    }

    #[test]
    fn digest_node_drift_flags_edited_and_dropped_ancestors() {
        let mut current: HashMap<&str, &str> = HashMap::new();
        current.insert("directive:base", "same");
        current.insert("directive:mid", "edited-now");

        // Unchanged: fresh digest matches the launched one.
        let (row, changed) = digest_node_json(&node("directive:base", "same"), &current);
        assert!(!changed);
        assert_eq!(row["changed"], false);
        assert_eq!(row["raw_content_digest"], "same");

        // Edited/republished: fresh digest differs.
        let (_, changed) = digest_node_json(&node("directive:mid", "old"), &current);
        assert!(changed);

        // Dropped from the chain entirely: absent from the fresh resolution.
        let (_, changed) = digest_node_json(&node("directive:gone", "old"), &current);
        assert!(changed);
    }

    fn violation(
        path: &str,
        code: InstanceViolationCode,
        expected: &str,
        found: &str,
    ) -> InstanceViolation {
        InstanceViolation {
            path: path.to_string(),
            code,
            expected: expected.to_string(),
            found: found.to_string(),
        }
    }

    #[test]
    fn contract_violation_maps_to_structured_error() {
        let report = InstanceValidationReport {
            errors: vec![
                violation(
                    "launch.mode",
                    InstanceViolationCode::EnumMismatch,
                    "one of [\"managed\", \"edge\"]",
                    "\"local\"",
                ),
                violation(
                    "name",
                    InstanceViolationCode::TypeMismatch,
                    "string",
                    "null",
                ),
            ],
            warnings: vec![violation(
                "affordances",
                InstanceViolationCode::UnexpectedField,
                "no field",
                "present",
            )],
        };

        let err = EngineError::ComposedValueContractViolation {
            canonical_ref: "surface:ryeos/studio/base".into(),
            report,
        };

        let he = map_engine_error(err);

        match he {
            crate::handler_error::HandlerError::Structured { code, body } => {
                assert_eq!(code, "contract_violation");
                assert_eq!(body["error_code"], "contract_violation");
                assert!(body["error"]
                    .as_str()
                    .unwrap()
                    .contains("surface:ryeos/studio/base"));
                assert!(body["error"].as_str().unwrap().contains("2 errors"));
                assert!(body["error"].as_str().unwrap().contains("1 warning"));

                let errors = body["details"]["errors"].as_array().unwrap();
                assert_eq!(errors.len(), 2);
                assert_eq!(errors[0]["path"], "launch.mode");
                assert_eq!(errors[0]["code"], "enum_mismatch");
                assert_eq!(errors[0]["expected"], "one of [\"managed\", \"edge\"]");
                assert_eq!(errors[0]["found"], "\"local\"");
                assert_eq!(errors[1]["path"], "name");
                assert_eq!(errors[1]["code"], "type_mismatch");

                let warnings = body["details"]["warnings"].as_array().unwrap();
                assert_eq!(warnings.len(), 1);
                assert_eq!(warnings[0]["path"], "affordances");
                assert_eq!(warnings[0]["code"], "unexpected_field");
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn contract_violation_with_single_error_uses_singular() {
        let report = InstanceValidationReport {
            errors: vec![violation(
                "name",
                InstanceViolationCode::TypeMismatch,
                "string",
                "null",
            )],
            warnings: vec![],
        };

        let err = EngineError::ComposedValueContractViolation {
            canonical_ref: "tool:my/tool".into(),
            report,
        };

        let he = map_engine_error(err);
        match he {
            crate::handler_error::HandlerError::Structured { body, .. } => {
                let msg = body["error"].as_str().unwrap();
                // Singular: "1 error", not "1 errors"
                assert!(
                    msg.contains("1 error"),
                    "message should use singular: {msg}"
                );
                assert!(
                    msg.contains("0 warnings"),
                    "message should use plural for 0: {msg}"
                );
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn contract_violation_empty_report() {
        let report = InstanceValidationReport {
            errors: vec![],
            warnings: vec![],
        };

        let err = EngineError::ComposedValueContractViolation {
            canonical_ref: "directive:test/x".into(),
            report,
        };

        let he = map_engine_error(err);
        match he {
            crate::handler_error::HandlerError::Structured { body, .. } => {
                let errors = body["details"]["errors"].as_array().unwrap();
                let warnings = body["details"]["warnings"].as_array().unwrap();
                assert!(errors.is_empty());
                assert!(warnings.is_empty());
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }
}
