//! Accept one staged product offered through `external-content/product-offer`.
//!
//! The caller is re-authenticated exactly as the offer was. The acceptance
//! core runs under the authenticated operator as witness owner and the origin
//! key pinned for the authenticated site, then the durable upload stage is
//! settled with the acceptance as its receipt. The call is idempotent: a
//! repeat re-verifies and returns the recorded acceptance without publishing
//! a second one.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use serde::Deserialize;
use serde_json::Value;

use super::external_content_product_offer::{
    PushAuthority, authenticate_push_origin, is_canonical_hash, stage_request_digest,
};
use crate::registry::ServiceDescriptor;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub staging_id: String,
    pub witness_hash: String,
    pub origin_admission_hash: String,
    pub maximum_bytes: u64,
}

impl Request {
    fn validate(&self) -> Result<()> {
        if self.staging_id.is_empty() || self.staging_id.len() > 256 {
            bail!("product accept requires the staging_id issued by product-offer");
        }
        for hash in [&self.witness_hash, &self.origin_admission_hash] {
            if !is_canonical_hash(hash) {
                bail!("product accept requires exact canonical witness and admission hashes");
            }
        }
        if self.maximum_bytes == 0 {
            bail!("product accept requires a positive byte bound");
        }
        Ok(())
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let authority = authenticate_push_origin(&state, &ctx)?;
    req.validate()?;
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    if req.maximum_bytes > policy.limits.max_total_bytes {
        bail!("product accept byte bound exceeds current import policy");
    }
    tokio::task::spawn_blocking(move || accept_blocking(&state, &authority, &req))
        .await
        .context("product accept task stopped")?
}

fn accept_blocking(
    state: &Arc<AppState>,
    authority: &PushAuthority,
    req: &Request,
) -> Result<Value> {
    let digest = stage_request_digest(
        &req.witness_hash,
        &req.origin_admission_hash,
        &authority.owner_principal,
    )?;
    let publication_key = ryeos_state::DurableCasPublicationKey::external_content_import(&digest)?;
    let pinned = state.state_store.pinned_state_authority()?;

    // The stage lock is not held across the acceptance core: that core takes
    // the write permit itself, and permit-before-stage is the lock order the
    // other settlers use. This first look proves ownership and the contract
    // and reads any receipt a previous call recorded.
    let recorded = {
        let guard = pinned.acquire_shared_guard()?;
        let stage = pinned
            .require_recovery()?
            .open_durable_cas_upload_admitted(
                &guard,
                &req.staging_id,
                &authority.owner_principal,
            )?;
        stage.ensure_publication_contract(&publication_key, None)?;
        stage.admitted_target_hash().map(str::to_owned)
    };

    // Re-verifies everything on every call, so an already-admitted stage
    // never short-circuits to an unverified answer. The core reuses an
    // existing acceptance head rather than publishing again.
    let (accepted, product) =
        ryeos_app::operator_external_content::product_receipt::accept_product_for_owner(
            state,
            &authority.owner_principal,
            &req.origin_admission_hash,
            &req.witness_hash,
            &authority.origin_key,
            req.maximum_bytes,
        )?;
    if let Some(recorded) = recorded.as_deref()
        && recorded != accepted.attestation_hash
    {
        bail!(
            "upload stage recorded acceptance {recorded}, but re-verification produced {}",
            accepted.attestation_hash
        );
    }

    {
        let guard = pinned.acquire_shared_guard()?;
        let _permit = state
            .write_barrier
            .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|error| {
                anyhow::anyhow!("cannot acquire product accept write permit: {error}")
            })?;
        let mut stage = pinned
            .require_recovery()?
            .open_durable_cas_upload_admitted(
                &guard,
                &req.staging_id,
                &authority.owner_principal,
            )?;
        stage.ensure_publication_contract(&publication_key, None)?;
        let settled = stage.admitted_target_hash().map(str::to_owned);
        match settled.as_deref() {
            Some(target) if target == accepted.attestation_hash => {}
            Some(target) => bail!(
                "upload stage was settled by acceptance {target}, not {}",
                accepted.attestation_hash
            ),
            None => {
                stage.protect_cas_closure(
                    &guard,
                    [accepted.attestation_hash.as_str()],
                    std::iter::empty(),
                )?;
                stage.finish_admitted(&guard, &accepted.attestation_hash)?;
            }
        }
    }

    Ok(serde_json::json!({
        "witness_hash": product.attestation_hash,
        "origin_admission_hash": req.origin_admission_hash,
        "acceptance_hash": accepted.attestation_hash,
        "reused_existing": accepted.reused_existing,
        "manifest_hash": product.evidence.manifest_hash,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/product-accept",
    endpoint: "external-content.product-accept",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/product-accept"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { handle(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_service_schema_matches_the_typed_request() {
        let descriptor: serde_json::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/services/external-content/product-accept.yaml"
        )))
        .unwrap();
        assert_eq!(descriptor["endpoint"], DESCRIPTOR.endpoint);
        assert_eq!(
            descriptor["required_caps"],
            serde_json::json!(DESCRIPTOR.required_caps)
        );
        assert_eq!(
            descriptor["schema"],
            serde_json::json!({
                "staging_id": "string",
                "witness_hash": "string",
                "origin_admission_hash": "string",
                "maximum_bytes": "integer",
            })
        );
        // Every authored field is a field of the typed request: the request
        // refuses unknown fields, so a field missing from it would fail here.
        let mut coordinates = serde_json::Map::new();
        for (field, kind) in descriptor["schema"].as_object().unwrap() {
            let kind = kind.as_str().unwrap();
            if kind.ends_with('?') {
                continue;
            }
            let value = match kind.trim_end_matches('?') {
                "integer" => serde_json::json!(1),
                "object" => serde_json::json!({}),
                "object[]" => serde_json::json!([]),
                _ => serde_json::json!("x"),
            };
            coordinates.insert(field.clone(), value);
        }
        let mut with_unknown = coordinates.clone();
        with_unknown.insert("not_a_field".to_owned(), serde_json::json!(1));
        assert!(serde_json::from_value::<Request>(serde_json::Value::Object(coordinates)).is_ok());
        assert!(
            serde_json::from_value::<Request>(serde_json::Value::Object(with_unknown)).is_err()
        );
    }
    use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

    fn valid() -> Value {
        serde_json::json!({
            "staging_id": "upload-abc",
            "witness_hash": "a".repeat(64),
            "origin_admission_hash": "b".repeat(64),
            "maximum_bytes": 1024,
        })
    }

    #[test]
    fn descriptor_names_the_accept_service() {
        assert_eq!(
            DESCRIPTOR.service_ref,
            "service:external-content/product-accept"
        );
        assert_eq!(DESCRIPTOR.endpoint, "external-content.product-accept");
        assert_eq!(
            DESCRIPTOR.required_caps,
            ["ryeos.execute.service.external-content/product-accept"]
        );
        assert!(matches!(
            DESCRIPTOR.availability,
            ServiceAvailability::DaemonOnly
        ));
    }

    #[test]
    fn request_shape_is_exact() {
        serde_json::from_value::<Request>(valid())
            .unwrap()
            .validate()
            .unwrap();
        for field in [
            "staging_id",
            "witness_hash",
            "origin_admission_hash",
            "maximum_bytes",
        ] {
            let mut missing = valid();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<Request>(missing).is_err(),
                "{field}"
            );
        }
        let mut unknown = valid();
        unknown["signing_key"] = serde_json::json!("caller-selected-key");
        assert!(serde_json::from_value::<Request>(unknown).is_err());
        for (field, value) in [
            ("staging_id", serde_json::json!("")),
            ("witness_hash", serde_json::json!("A".repeat(64))),
            ("origin_admission_hash", serde_json::json!("b".repeat(63))),
            ("maximum_bytes", serde_json::json!(0)),
        ] {
            let mut invalid = valid();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<Request>(invalid)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn accept_authentication_refuses_every_non_remote_operator_shape() {
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let fingerprint = format!("fp:{}", "22".repeat(32));
        let refusals = [
            (Some(AuthorizedKeyPrincipalClass::LocalClient), None, true),
            (
                Some(AuthorizedKeyPrincipalClass::RemoteNode),
                Some("site:o"),
                true,
            ),
            (
                Some(AuthorizedKeyPrincipalClass::RemoteOperator),
                None,
                true,
            ),
            (
                Some(AuthorizedKeyPrincipalClass::RemoteOperator),
                Some("site:o"),
                false,
            ),
            (None, Some("site:o"), true),
        ];
        for (class, site, verified) in refusals {
            let ctx = HandlerContext::new_with_authority(
                fingerprint.clone(),
                Vec::new(),
                verified,
                class,
                site.map(str::to_owned),
            );
            assert!(authenticate_push_origin(&state, &ctx).is_err());
        }
    }
}
