//! `content.pin` — daemon-owned project content-pin authoring.

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
    pub item_ref: String,
    pub project_path: String,
    #[serde(
        default,
        rename = "id",
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    pub ids: Vec<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub update: bool,
}

pub async fn handle(
    request: Request,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let admitted_operator =
        ryeos_app::operator_authority::require_local_configured_operator(&state, &context)
            .context("content pin requires the configured local operator")?;
    let signing_key =
        ryeos_core_tools::actions::sign::load_operator_signing_key(&state.config.app_root)
            .context("load daemon-owned operator signing key")?;
    let signing_fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
    if signing_fingerprint != admitted_operator {
        bail!("configured local operator and authoring signing key disagree");
    }

    let engine = Arc::clone(&state.engine);
    let ignore_matcher = Arc::clone(&state.ignore_matcher);
    let item_ref = request.item_ref;
    let project_path = std::path::PathBuf::from(request.project_path);
    let options = ryeos_core_tools::actions::content_pin::ContentPinOptions {
        ids: request.ids,
        all: request.all,
        update: request.update,
    };
    let report = tokio::task::spawn_blocking(move || {
        ryeos_core_tools::actions::content_pin::run_content_pin_online(
            &item_ref,
            &project_path,
            &options,
            &engine,
            ignore_matcher.as_ref(),
            &signing_key,
        )
    })
    .await
    .context("content-pin authoring worker stopped")??;
    serde_json::to_value(report).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:content/pin",
    endpoint: "content.pin",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.content/pin"],
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
    fn request_accepts_one_or_repeated_declaration_ids() {
        let one: Request = serde_json::from_value(serde_json::json!({
            "item_ref": "graph:training/run",
            "project_path": "/project",
            "id": "dataset"
        }))
        .unwrap();
        assert_eq!(one.ids, ["dataset"]);

        let many: Request = serde_json::from_value(serde_json::json!({
            "item_ref": "graph:training/run",
            "project_path": "/project",
            "id": ["dataset", "runtime"]
        }))
        .unwrap();
        assert_eq!(many.ids, ["dataset", "runtime"]);
    }

    #[test]
    fn descriptor_is_dual_mode_and_non_threaded_by_contract() {
        assert_eq!(DESCRIPTOR.service_ref, "service:content/pin");
        assert_eq!(DESCRIPTOR.endpoint, "content.pin");
        assert_eq!(DESCRIPTOR.availability, ServiceAvailability::Both);
        assert_eq!(
            DESCRIPTOR.required_caps,
            ["ryeos.execute.service.content/pin"]
        );
    }
}
