//! `bundle.list` — enumerate registered bundles from the node-config snapshot,
//! and the bundles staged for a later start.
//!
//! `bundles` holds only installed (registered) bundles. `staged` holds the
//! stages in the node's staging area, each with status `staged`: inert bytes,
//! not registered and not loaded. An operator sees the stage's detail (digest,
//! source, requester, what the bundle declares); any other caller sees the name
//! and the status only. A stage whose record does not verify is not listed.
//! `apply_refused` is a later status of a stage and has its place here.

use std::sync::Arc;

use anyhow::Result;
use ryeos_node::bundle_staging::apply::STATUS_APPLY_REFUSED;
use ryeos_node::bundle_staging::{STATUS_STAGED, StagedBundle, StagingArea};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

/// One stage as the caller may see it.
fn staged_entry(
    stage: &StagedBundle,
    operator: bool,
    policy: &ryeos_node_runtime::node_policy::NodePolicySnapshot,
) -> Value {
    let mut entry = serde_json::json!({
        "name": stage.record.bundle_name,
        "status": STATUS_STAGED,
    });
    if operator && let Some(object) = entry.as_object_mut() {
        if let Some(refusal) = &stage.refusal {
            object.insert("status".into(), serde_json::json!(STATUS_APPLY_REFUSED));
            object.insert(
                "apply_refusal".into(),
                serde_json::json!({
                    "code": refusal.code.as_str(),
                    "request_digest": refusal.request_digest,
                }),
            );
        }
        object.insert(
            "tree_digest".into(),
            serde_json::json!(stage.record.tree_digest),
        );
        object.insert("source".into(), serde_json::json!(stage.record.source));
        object.insert(
            "requested_by".into(),
            serde_json::json!(stage.record.requested_by),
        );
        // What the bundle declares, under a name that cannot be read as a lane
        // marker: nothing is claimed until a signed operation applies it.
        object.insert("declared".into(), serde_json::json!(stage.record.declared));
        if stage.record.source.kind == ryeos_node::bundle_staging::SOURCE_PUBLICATION {
            object.insert(
                "predicted_lifts".into(),
                super::stage_lane::predicted_lifts(policy, &stage.record.bundle_name),
            );
        }
        object.insert(
            "file_count".into(),
            serde_json::json!(stage.record.file_count),
        );
        object.insert(
            "total_bytes".into(),
            serde_json::json!(stage.record.total_bytes),
        );
        object.insert(
            "staged_at".into(),
            serde_json::json!(stage.record.staged_at),
        );
    }
    entry
}

pub async fn handle(_req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let operator = ryeos_app::operator_authority::disclosure_class_for(&state, &ctx)
        == ryeos_node_runtime::disclosure::DisclosureClass::Operator;
    let mut bundles = Vec::new();
    for record in &state.node_config.bundles {
        let mut entry = serde_json::json!({
            "name": record.name,
            "path": record.path.display().to_string(),
            "status": "installed",
        });
        if let Some(lane) = &record.lane {
            entry["lane"] = serde_json::to_value(lane)?;
        }
        bundles.push(entry);
    }
    let node_key = state.identity.signing_key().verifying_key();
    let staged: Vec<Value> =
        match StagingArea::open_if_present(&state.config.app_root).map_err(anyhow::Error::new)? {
            Some(area) => area
                .list(&node_key)
                .map_err(anyhow::Error::new)?
                .iter()
                .map(|stage| staged_entry(stage, operator, &state.node_policy))
                .collect(),
            None => Vec::new(),
        };
    Ok(serde_json::json!({ "bundles": bundles, "staged": staged }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/list",
    endpoint: "bundle.list",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_node::bundle_staging::apply::{ApplyRefusalCode, ApplyRefusalMarker, REFUSAL_SCHEMA};
    use ryeos_node::bundle_staging::{DeclaredFacets, STAGING_SCHEMA, StagedSource, StagingRecord};

    fn stage(refusal: Option<ApplyRefusalMarker>) -> StagedBundle {
        StagedBundle {
            record: StagingRecord {
                schema: STAGING_SCHEMA.to_owned(),
                bundle_name: "demo".to_owned(),
                tree_digest: "a".repeat(64),
                source: StagedSource {
                    kind: "remote".to_owned(),
                    origin: "remote:test".to_owned(),
                    publication_record_hash: None,
                    pinned_node: None,
                    manifest_hash: None,
                },
                requested_by: "bb".to_owned(),
                policy_generation: "c".repeat(64),
                declared: DeclaredFacets::default(),
                file_count: 1,
                total_bytes: 1,
                evidence: Default::default(),
                staged_at: "2026-11-27T00:00:00Z".to_owned(),
            },
            dir: std::path::PathBuf::from("/nonexistent"),
            refusal,
        }
    }

    fn lane_policy() -> ryeos_node_runtime::node_policy::NodePolicySnapshot {
        ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(vec![
            ryeos_node_runtime::node_policy::shipped_publication_lane_record(),
        ])
    }

    #[test]
    fn a_publication_stage_shows_declared_and_a_labelled_prediction_to_an_operator_only() {
        let mut publication = stage(None);
        publication.record.source.kind = ryeos_node::bundle_staging::SOURCE_PUBLICATION.to_owned();
        let operator = staged_entry(&publication, true, &lane_policy());
        assert!(operator.get("lane").is_none(), "{operator:#}");
        assert!(operator.get("declared").is_some());
        assert_eq!(
            operator["predicted_lifts"]["note"],
            "prediction; policy is re-read at apply"
        );
        assert_eq!(operator["predicted_lifts"]["invoked"], true);
        let principal = staged_entry(&publication, false, &lane_policy());
        assert_eq!(
            principal,
            serde_json::json!({ "name": "demo", "status": "staged" })
        );
        // A stage that is not from a publication has no prediction.
        let native = staged_entry(&stage(None), true, &lane_policy());
        assert!(native.get("predicted_lifts").is_none());
    }

    fn marker() -> ApplyRefusalMarker {
        ApplyRefusalMarker {
            schema: REFUSAL_SCHEMA.to_owned(),
            tree_digest: "a".repeat(64),
            request_digest: "d".repeat(64),
            code: ApplyRefusalCode::TreeDigestMismatch,
            signer_fingerprint: "ee".to_owned(),
        }
    }

    #[test]
    fn an_operator_sees_the_refusal_code_and_everyone_else_sees_staged() {
        let refused = stage(Some(marker()));
        let operator = staged_entry(&refused, true, &lane_policy());
        assert_eq!(operator["status"], "apply_refused");
        assert_eq!(operator["apply_refusal"]["code"], "tree_digest_mismatch");
        assert_eq!(operator["apply_refusal"]["request_digest"], "d".repeat(64));

        let principal = staged_entry(&refused, false, &lane_policy());
        assert_eq!(
            principal,
            serde_json::json!({ "name": "demo", "status": "staged" })
        );
    }

    #[test]
    fn a_stage_with_no_marker_is_staged_for_an_operator_too() {
        let entry = staged_entry(&stage(None), true, &lane_policy());
        assert_eq!(entry["status"], "staged");
        assert!(entry.get("apply_refusal").is_none());
    }
}
