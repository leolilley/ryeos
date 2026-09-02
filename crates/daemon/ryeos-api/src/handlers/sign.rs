//! `sign` — local-operator project item authoring.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
    pub item_refs: Vec<String>,
    pub project_path: String,
}

pub async fn handle(
    request: Request,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let admitted_operator =
        ryeos_app::operator_authority::require_local_configured_operator(&state, &context)
            .context("sign requires the configured local operator")?;
    if request.item_refs.is_empty() {
        bail!("sign requires at least one item ref");
    }
    let signing_key =
        ryeos_core_tools::actions::sign::load_operator_signing_key(&state.config.app_root)
            .context("load daemon-owned operator signing key")?;
    let signing_fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
    if signing_fingerprint != admitted_operator {
        bail!("configured local operator and authoring signing key disagree");
    }

    let engine = Arc::clone(&state.engine);
    let project_path = PathBuf::from(request.project_path);
    let item_refs = request.item_refs;
    let report = tokio::task::spawn_blocking(move || {
        let mut batch = ryeos_core_tools::actions::sign::BatchReport::default();
        let batch_mode = item_refs.len() > 1;
        for item_ref in item_refs {
            match ryeos_core_tools::actions::sign::run_sign_online(
                &item_ref,
                &project_path,
                &engine,
                &signing_key,
            ) {
                Ok(report) => batch.extend(report),
                Err(error) if batch_mode => {
                    batch
                        .failed
                        .push(ryeos_core_tools::actions::sign::ItemOutcome {
                            item_ref,
                            signature: None,
                            error: Some(format!("{error:#}")),
                            warnings: Vec::new(),
                        });
                }
                Err(error) => return Err(error),
            }
        }
        Ok::<_, anyhow::Error>(batch)
    })
    .await
    .context("sign authoring worker stopped")??;

    if !report.is_total_success() {
        let failures = report
            .failed
            .iter()
            .map(|item| {
                format!(
                    "{}: {}",
                    item.item_ref,
                    item.error.as_deref().unwrap_or("signing failed")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        bail!(
            "{}/{} items failed validation or signing: {failures}",
            report.failed.len(),
            report.total()
        );
    }
    serde_json::to_value(report).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:sign",
    endpoint: "sign",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.sign"],
    handler: |params, context, state| {
        Box::pin(async move {
            let request = crate::handler_error::parse_request(params)?;
            handle(request, context, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_accepts_one_or_many_item_refs() {
        let one: Request = serde_json::from_value(serde_json::json!({
            "item_refs": "graph:one",
            "project_path": "/project"
        }))
        .unwrap();
        assert_eq!(one.item_refs, ["graph:one"]);

        let many: Request = serde_json::from_value(serde_json::json!({
            "item_refs": ["graph:one", "tool:two"],
            "project_path": "/project"
        }))
        .unwrap();
        assert_eq!(many.item_refs, ["graph:one", "tool:two"]);
    }

    #[test]
    fn descriptor_is_dual_mode_and_non_threaded_by_contract() {
        assert_eq!(DESCRIPTOR.service_ref, "service:sign");
        assert_eq!(DESCRIPTOR.endpoint, "sign");
        assert_eq!(DESCRIPTOR.availability, ServiceAvailability::Both);
        assert_eq!(DESCRIPTOR.required_caps, ["ryeos.execute.service.sign"]);
    }
}
