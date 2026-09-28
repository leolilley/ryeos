//! Exact operator-owned product capture and durable lookup.

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use anyhow::Result;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub type Request = ryeos_app::operator_external_content::products::ProductRequest;

/// The caller selects retained authority, never bytes, a path, or a contact
/// budget. The installed producer binding supplies all provider authority.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProduceRuntimeSnapshotRequest {
    binding_id: String,
    witness_hash: String,
    source: ProductWitnessSource,
    source_occurrence_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRuntimeSnapshotRequest {
    operation_id: String,
}

pub async fn get_runtime_snapshot(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_runtime_snapshot::get_operation(&state, &ctx, &req.operation_id)?,
    )?)
}

pub async fn observe_runtime_snapshot_readiness(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let observed = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_runtime_snapshot::observe_readiness(&state, &ctx, &req.operation_id)
    })
    .await
    .map_err(|error| anyhow::anyhow!("runtime snapshot readiness join failed: {error}"))??;
    Ok(serde_json::to_value(observed)?)
}

pub async fn produce_runtime_snapshot(
    req: ProduceRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ryeos_app::operator_authority::require_admitted_operator(&state, &ctx)?;
    let result = tokio::task::spawn_blocking(move || {
        let ceiling = ryeos_app::operator_runtime_snapshot::staging_source_ceiling(
            &state,
            &ctx,
            &req.binding_id,
        )?;
        let stage = state.state_store.begin_runtime_snapshot_stage()?;
        let staged =
            ryeos_executor::execution::external_guest_runtime_product::stage_current_guest_owner_runtime_product(
                &state,
                &ctx,
                &req.witness_hash,
                &req.source,
                ceiling,
                stage.root(),
                stage.product_name(),
            )?;
        staged.ensure_current()?;
        let result = ryeos_app::operator_runtime_snapshot::produce(
            &state,
            &ctx,
            ryeos_app::operator_runtime_snapshot::SnapshotProductionRequest {
                binding_id: req.binding_id,
                witness_hash: req.witness_hash,
                source: req.source,
                source_occurrence_id: req.source_occurrence_id,
                staged_identity: staged.identity().clone(),
                staged_root: staged.root().try_clone()?,
            },
        )?;
        anyhow::Ok(result)
    })
    .await
    .map_err(|error| anyhow::anyhow!("runtime snapshot production join failed: {error}"))??;
    Ok(serde_json::to_value(result)?)
}

pub async fn capture(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::products::capture(state, ctx, req).await?,
    )?)
}

pub async fn get(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::products::get(state, ctx, req).await?,
    )?)
}

pub async fn qualify(
    req: ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let project_context_resolver = super::qualification_project_context::resolver(state.as_ref());
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::product_qualification::qualify_with_project_context_resolver(
            state,
            ctx,
            req,
            Some(project_context_resolver),
        )
        .await?,
    )?)
}

pub async fn compose(
    req: ryeos_app::operator_external_content::product_composition::ComposeRetainedProductsRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    req.validate()?;
    ryeos_app::operator_authority::require_admitted_operator(&state, &ctx)?;
    let qualification_project_context_resolver =
        super::qualification_project_context::resolver(state.as_ref());
    let preparation_request = req.clone();
    let preparation_state = Arc::clone(&state);
    let preparation_context = ctx.clone();
    let preparation_qualification_resolver = Arc::clone(&qualification_project_context_resolver);
    let (prepared, imported) = tokio::task::spawn_blocking(move || {
        let mut prepared =
            ryeos_executor::execution::project_source::prepare_external_product_consumer(
                &preparation_state,
                &preparation_request,
                &preparation_context,
                &format!("product-composition-{}", uuid::Uuid::new_v4()),
            )?;
        let imported = prepared.select_and_import_products(
            preparation_state,
            preparation_context,
            &preparation_request,
            Some(preparation_qualification_resolver),
        )?;
        anyhow::Ok((prepared, imported))
    })
    .await
    .map_err(|error| anyhow::anyhow!("product consumer preparation failed: {error}"))??;
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::product_composition::compose_selected_products(
            state,
            ctx,
            req,
            prepared.resolution(),
            imported,
        )
        .await?,
    )?)
}

pub const CAPTURE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/capture-product",
    endpoint: "external-content.capture-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/capture-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { capture(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

pub const PRODUCE_RUNTIME_SNAPSHOT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/produce-runtime-snapshot",
    endpoint: "external-content.produce-runtime-snapshot",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/produce-runtime-snapshot"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            produce_runtime_snapshot(crate::handler_error::parse_request(params)?, ctx, state).await
        })
    },
};

pub const GET_RUNTIME_SNAPSHOT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/runtime-snapshot",
    endpoint: "external-content.runtime-snapshot",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/runtime-snapshot"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            get_runtime_snapshot(crate::handler_error::parse_request(params)?, ctx, state).await
        })
    },
};

pub const OBSERVE_RUNTIME_SNAPSHOT_READINESS_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/observe-runtime-snapshot-readiness",
    endpoint: "external-content.observe-runtime-snapshot-readiness",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/observe-runtime-snapshot-readiness"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            observe_runtime_snapshot_readiness(
                crate::handler_error::parse_request(params)?,
                ctx,
                state,
            )
            .await
        })
    },
};

pub const GET_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/product",
    endpoint: "external-content.product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/product"],
    handler: |params, ctx, state| {
        Box::pin(async move { get(crate::handler_error::parse_request(params)?, ctx, state).await })
    },
};

pub const COMPOSE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/compose-product",
    endpoint: "external-content.compose-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/compose-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { compose(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

#[cfg(test)]
mod tests {
    use ryeos_app::operator_external_content::product_composition::ComposeRetainedProductsRequest;
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use serde_json::json;

    #[test]
    fn runtime_snapshot_request_cannot_supply_upload_or_filesystem_authority() {
        let required = json!({
            "binding_id": "signed-render-producer",
            "witness_hash": "a".repeat(64),
            "source": {"kind": "local_capture"},
            "source_occurrence_id": "sbx-exact"
        });
        assert!(
            serde_json::from_value::<super::ProduceRuntimeSnapshotRequest>(required.clone())
                .is_ok()
        );
        for (key, value) in [
            ("upload_path", json!("/tmp/ambient")),
            ("upload_bytes", json!(123)),
            ("contact_timeout_seconds", json!(600)),
            ("credential", json!("secret")),
            ("provider_group_id", json!("another-group")),
        ] {
            let mut changed = required.clone();
            changed[key] = value;
            assert!(
                serde_json::from_value::<super::ProduceRuntimeSnapshotRequest>(changed).is_err(),
                "unadmitted field {key} was accepted"
            );
        }
    }

    #[test]
    fn signed_compose_contract_preserves_required_nullable_project_authority() {
        let root = ryeos_engine::test_support::workspace_root();
        let service: serde_yaml::Value = serde_yaml::from_slice(
            &std::fs::read(
                root.join("bundles/core/.ai/services/external-content/compose-product.yaml"),
            )
            .unwrap(),
        )
        .unwrap();
        let command: serde_yaml::Value = serde_yaml::from_slice(
            &std::fs::read(
                root.join("bundles/core/.ai/node/commands/external-content-compose-product.yaml"),
            )
            .unwrap(),
        )
        .unwrap();
        let schema = serde_json::to_value(&service["schema"]).unwrap();
        let contract =
            ryeos_runtime::InvocationInputContract::from_lightweight_schema_value(&schema)
                .unwrap()
                .unwrap();
        let project_field = &contract.fields["project_context"];
        assert!(project_field.required);
        assert!(project_field.nullable);
        assert!(command["help"]["usage"].as_str().unwrap().contains(
            "\"project_context\":{\"snapshot_hash\":\"<exact-lowercase-project-snapshot-hash>\"}"
        ));

        let projectless_wire = json!({
            "consumer_ref": "tool:example/verify",
            "project_context": null,
            "selections": [{
                "declaration_id": "runtime",
                "witness_hash": "a".repeat(64),
                "witness_source": {"kind": "local_capture"},
                "qualification_hash": null
            }],
            "maximum_bytes": 1024
        });
        let normalized = ryeos_runtime::arg_binder::normalize_params_with_contract(
            projectless_wire.clone(),
            Some(&contract),
        )
        .unwrap();
        let projectless: ComposeRetainedProductsRequest =
            serde_json::from_value(normalized).unwrap();
        projectless.validate().unwrap();
        assert_eq!(
            projectless.subject_resolution_authority(),
            SubjectResolutionAuthority::Projectless
        );

        let mut pinned_wire = projectless_wire.clone();
        let snapshot_hash = "b".repeat(64);
        pinned_wire["project_context"] = json!({"snapshot_hash": snapshot_hash});
        let normalized =
            ryeos_runtime::arg_binder::normalize_params_with_contract(pinned_wire, Some(&contract))
                .unwrap();
        let pinned: ComposeRetainedProductsRequest = serde_json::from_value(normalized).unwrap();
        pinned.validate().unwrap();
        assert_eq!(
            pinned.subject_resolution_authority(),
            SubjectResolutionAuthority::PinnedGeneration { snapshot_hash }
        );

        let missing = json!({
            "consumer_ref": "tool:example/verify",
            "selections": [{
                "declaration_id": "runtime",
                "witness_hash": "a".repeat(64),
                "witness_source": {"kind": "local_capture"},
                "qualification_hash": null
            }],
            "maximum_bytes": 1024
        });
        assert!(
            ryeos_runtime::arg_binder::normalize_params_with_contract(missing, Some(&contract))
                .unwrap_err()
                .contains("--project-context is required")
        );
    }
}

pub const QUALIFY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/qualify-product",
    endpoint: "external-content.qualify-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/qualify-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { qualify(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};
