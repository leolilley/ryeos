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
    source: RuntimeSnapshotSourceRequest,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RuntimeSnapshotSourceRequest {
    CapturedProduct {
        witness_hash: String,
        witness_source: ProductWitnessSource,
        source_occurrence_id: String,
    },
    BundleMaterialization {
        materialization_binding_id: String,
        coordinate_digest: String,
        attestation_hash: String,
        bootstrap_operation_id: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRuntimeSnapshotRequest {
    operation_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapRuntimeSourceRequest {
    binding_id: String,
    materialization_binding_id: String,
    coordinate_digest: String,
    attestation_hash: String,
    maximum_lifetime_seconds: u32,
}

pub async fn bootstrap_runtime_source(
    req: BootstrapRuntimeSourceRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let result = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_runtime_snapshot::bootstrap_source(
            &state,
            &ctx,
            ryeos_app::operator_runtime_snapshot::SnapshotBootstrapRequest {
                binding_id: req.binding_id,
                materialization_binding_id: req.materialization_binding_id,
                coordinate_digest: req.coordinate_digest,
                attestation_hash: req.attestation_hash,
                maximum_lifetime_seconds: req.maximum_lifetime_seconds,
            },
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("bootstrap source join failed: {error}"))??;
    Ok(serde_json::to_value(result)?)
}

pub async fn get_bootstrap_runtime_source(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_runtime_snapshot::get_bootstrap_source(
            &state,
            &ctx,
            &req.operation_id,
        )?,
    )?)
}

pub async fn observe_bootstrap_runtime_source(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let result = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_runtime_snapshot::observe_bootstrap_source(
            &state,
            &ctx,
            &req.operation_id,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("bootstrap readiness join failed: {error}"))??;
    Ok(serde_json::to_value(result)?)
}

pub async fn terminate_bootstrap_runtime_source(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let result = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_runtime_snapshot::terminate_bootstrap_source(
            &state,
            &ctx,
            &req.operation_id,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("bootstrap termination join failed: {error}"))??;
    Ok(serde_json::to_value(result)?)
}

pub async fn get_bootstrap_runtime_source_termination(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_runtime_snapshot::get_bootstrap_source_termination(
            &state,
            &ctx,
            &req.operation_id,
        )?,
    )?)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRuntimeSnapshotQualificationRequest {
    qualification_binding_id: String,
    snapshot_operation_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotQualificationRequest {
    qualification_operation_id: String,
}

pub async fn create_runtime_snapshot_qualification(
    req: CreateRuntimeSnapshotQualificationRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let result = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_runtime_snapshot::create_qualification_occurrence(
            &state,
            &ctx,
            &req.qualification_binding_id,
            &req.snapshot_operation_id,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("qualification creation join failed: {error}"))??;
    Ok(serde_json::to_value(result)?)
}

pub async fn verify_runtime_snapshot_qualification(
    req: RuntimeSnapshotQualificationRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let result = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_runtime_snapshot::verify_qualification_occurrence(
            &state,
            &ctx,
            &req.qualification_operation_id,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("qualification verifier join failed: {error}"))??;
    Ok(serde_json::to_value(result)?)
}

pub async fn terminate_runtime_snapshot_qualification(
    req: RuntimeSnapshotQualificationRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let result = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_runtime_snapshot::terminate_qualification_occurrence(
            &state,
            &ctx,
            &req.qualification_operation_id,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("qualification termination join failed: {error}"))??;
    Ok(serde_json::to_value(result)?)
}

pub async fn get_runtime_snapshot_qualification(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_runtime_snapshot::get_qualification_operation(
            &state,
            &ctx,
            &req.operation_id,
        )?,
    )?)
}

pub async fn get_runtime_snapshot_qualification_termination(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_runtime_snapshot::get_qualification_termination(
            &state,
            &ctx,
            &req.operation_id,
        )?,
    )?)
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

pub async fn continue_runtime_snapshot_create(
    req: GetRuntimeSnapshotRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let result = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_runtime_snapshot::continue_staged_create(
            &state,
            &ctx,
            &req.operation_id,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("snapshot create continuation join failed: {error}"))??;
    Ok(serde_json::to_value(result)?)
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
        let (staged_identity, staged_root, source) = match req.source {
            RuntimeSnapshotSourceRequest::CapturedProduct {
                witness_hash,
                witness_source,
                source_occurrence_id,
            } => {
                let staged = ryeos_executor::execution::external_guest_runtime_product::stage_current_guest_owner_runtime_product(
                    &state, &ctx, &witness_hash, &witness_source, ceiling,
                    stage.root(), stage.product_name(),
                )?;
                staged.ensure_current()?;
                (
                    staged.identity().clone(),
                    staged.root().try_clone()?,
                    ryeos_app::operator_runtime_snapshot::SnapshotProductionSource::CapturedProduct {
                        witness_hash,
                        source: witness_source,
                        source_occurrence_id,
                    },
                )
            }
            RuntimeSnapshotSourceRequest::BundleMaterialization {
                materialization_binding_id,
                coordinate_digest,
                attestation_hash,
                bootstrap_operation_id,
            } => {
                let staged = ryeos_executor::execution::external_guest_runtime_product::stage_current_guest_owner_runtime_materialization(
                    &state, &ctx, &materialization_binding_id, &coordinate_digest,
                    &attestation_hash, ceiling, stage.root(), stage.product_name(),
                )?;
                staged.ensure_current_authority(&state, &ctx)?;
                staged.ensure_current()?;
                (
                    staged.identity().clone(),
                    staged.root().try_clone()?,
                    ryeos_app::operator_runtime_snapshot::SnapshotProductionSource::BundleMaterialization {
                        materialization_binding_id,
                        coordinate_digest,
                        attestation_hash,
                        bootstrap_operation_id,
                    },
                )
            }
        };
        let result = ryeos_app::operator_runtime_snapshot::produce(
            &state,
            &ctx,
            ryeos_app::operator_runtime_snapshot::SnapshotProductionRequest {
                binding_id: req.binding_id,
                source,
                staged_identity,
                staged_root,
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

pub const CONTINUE_RUNTIME_SNAPSHOT_CREATE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/continue-runtime-snapshot-create",
    endpoint: "external-content.continue-runtime-snapshot-create",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/continue-runtime-snapshot-create"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            continue_runtime_snapshot_create(
                crate::handler_error::parse_request(params)?,
                ctx,
                state,
            )
            .await
        })
    },
};

pub const BOOTSTRAP_RUNTIME_SOURCE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/bootstrap-runtime-source",
    endpoint: "external-content.bootstrap-runtime-source",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/bootstrap-runtime-source"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            bootstrap_runtime_source(crate::handler_error::parse_request(params)?, ctx, state).await
        })
    },
};

pub const GET_BOOTSTRAP_RUNTIME_SOURCE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/bootstrap-runtime-source-operation",
    endpoint: "external-content.bootstrap-runtime-source-operation",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/bootstrap-runtime-source-operation"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            get_bootstrap_runtime_source(crate::handler_error::parse_request(params)?, ctx, state)
                .await
        })
    },
};

pub const OBSERVE_BOOTSTRAP_RUNTIME_SOURCE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/observe-bootstrap-runtime-source",
    endpoint: "external-content.observe-bootstrap-runtime-source",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/observe-bootstrap-runtime-source"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            observe_bootstrap_runtime_source(
                crate::handler_error::parse_request(params)?,
                ctx,
                state,
            )
            .await
        })
    },
};

pub const TERMINATE_BOOTSTRAP_RUNTIME_SOURCE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/terminate-bootstrap-runtime-source",
    endpoint: "external-content.terminate-bootstrap-runtime-source",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/terminate-bootstrap-runtime-source"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            terminate_bootstrap_runtime_source(
                crate::handler_error::parse_request(params)?,
                ctx,
                state,
            )
            .await
        })
    },
};

pub const GET_BOOTSTRAP_RUNTIME_SOURCE_TERMINATION_DESCRIPTOR: ServiceDescriptor =
    ServiceDescriptor {
        service_ref: "service:external-content/bootstrap-runtime-source-termination",
        endpoint: "external-content.bootstrap-runtime-source-termination",
        availability: ServiceAvailability::DaemonOnly,
        required_caps: &[
            "ryeos.execute.service.external-content/bootstrap-runtime-source-termination",
        ],
        handler: |params, ctx, state| {
            Box::pin(async move {
                get_bootstrap_runtime_source_termination(
                    crate::handler_error::parse_request(params)?,
                    ctx,
                    state,
                )
                .await
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

pub const CREATE_RUNTIME_SNAPSHOT_QUALIFICATION_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/create-runtime-snapshot-qualification",
    endpoint: "external-content.create-runtime-snapshot-qualification",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.service.external-content/create-runtime-snapshot-qualification",
    ],
    handler: |params, ctx, state| {
        Box::pin(async move {
            create_runtime_snapshot_qualification(
                crate::handler_error::parse_request(params)?,
                ctx,
                state,
            )
            .await
        })
    },
};

pub const VERIFY_RUNTIME_SNAPSHOT_QUALIFICATION_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/verify-runtime-snapshot-qualification",
    endpoint: "external-content.verify-runtime-snapshot-qualification",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.service.external-content/verify-runtime-snapshot-qualification",
    ],
    handler: |params, ctx, state| {
        Box::pin(async move {
            verify_runtime_snapshot_qualification(
                crate::handler_error::parse_request(params)?,
                ctx,
                state,
            )
            .await
        })
    },
};

pub const TERMINATE_RUNTIME_SNAPSHOT_QUALIFICATION_DESCRIPTOR: ServiceDescriptor =
    ServiceDescriptor {
        service_ref: "service:external-content/terminate-runtime-snapshot-qualification",
        endpoint: "external-content.terminate-runtime-snapshot-qualification",
        availability: ServiceAvailability::DaemonOnly,
        required_caps: &[
            "ryeos.execute.service.external-content/terminate-runtime-snapshot-qualification",
        ],
        handler: |params, ctx, state| {
            Box::pin(async move {
                terminate_runtime_snapshot_qualification(
                    crate::handler_error::parse_request(params)?,
                    ctx,
                    state,
                )
                .await
            })
        },
    };

pub const GET_RUNTIME_SNAPSHOT_QUALIFICATION_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/runtime-snapshot-qualification",
    endpoint: "external-content.runtime-snapshot-qualification",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/runtime-snapshot-qualification"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            get_runtime_snapshot_qualification(
                crate::handler_error::parse_request(params)?,
                ctx,
                state,
            )
            .await
        })
    },
};

pub const GET_RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_DESCRIPTOR: ServiceDescriptor =
    ServiceDescriptor {
        service_ref: "service:external-content/runtime-snapshot-qualification-termination",
        endpoint: "external-content.runtime-snapshot-qualification-termination",
        availability: ServiceAvailability::DaemonOnly,
        required_caps: &[
            "ryeos.execute.service.external-content/runtime-snapshot-qualification-termination",
        ],
        handler: |params, ctx, state| {
            Box::pin(async move {
                get_runtime_snapshot_qualification_termination(
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
            "source": {
                "kind": "captured_product",
                "witness_hash": "a".repeat(64),
                "witness_source": {"kind": "local_capture"},
                "source_occurrence_id": "sbx-exact"
            }
        });
        let captured =
            serde_json::from_value::<super::ProduceRuntimeSnapshotRequest>(required.clone())
                .unwrap();
        assert!(matches!(
            captured.source,
            super::RuntimeSnapshotSourceRequest::CapturedProduct { .. }
        ));
        let materialized = json!({
            "binding_id": "signed-render-producer",
            "source": {
                "kind": "bundle_materialization",
                "materialization_binding_id": "signed-owner-materializer",
                "coordinate_digest": "b".repeat(64),
                "attestation_hash": "c".repeat(64),
                "bootstrap_operation_id": "d".repeat(64)
            }
        });
        let materialized =
            serde_json::from_value::<super::ProduceRuntimeSnapshotRequest>(materialized).unwrap();
        assert!(matches!(
            materialized.source,
            super::RuntimeSnapshotSourceRequest::BundleMaterialization { .. }
        ));
        let mut substituted = required.clone();
        substituted["source"]["bootstrap_operation_id"] = json!("d".repeat(64));
        assert!(
            serde_json::from_value::<super::ProduceRuntimeSnapshotRequest>(substituted).is_err()
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
    fn qualification_service_requests_select_only_retained_coordinates() {
        let create = json!({
            "qualification_binding_id": "signed-qualification",
            "snapshot_operation_id": "a".repeat(64),
        });
        assert!(
            serde_json::from_value::<super::CreateRuntimeSnapshotQualificationRequest>(
                create.clone()
            )
            .is_ok()
        );
        let step = json!({"qualification_operation_id": "b".repeat(64)});
        assert!(
            serde_json::from_value::<super::RuntimeSnapshotQualificationRequest>(step.clone())
                .is_ok()
        );
        for (key, value) in [
            ("credential", json!("secret")),
            ("provider_group_id", json!("unselected-group")),
            ("contact_timeout_seconds", json!(600)),
            ("occurrence_id", json!("sbx-ambient")),
        ] {
            let mut changed_create = create.clone();
            changed_create[key] = value.clone();
            assert!(
                serde_json::from_value::<super::CreateRuntimeSnapshotQualificationRequest>(
                    changed_create
                )
                .is_err()
            );
            let mut changed_step = step.clone();
            changed_step[key] = value;
            assert!(
                serde_json::from_value::<super::RuntimeSnapshotQualificationRequest>(changed_step)
                    .is_err()
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
