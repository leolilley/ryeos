//! Application composition for provider-owned durable CAS object contracts.
//!
//! `ryeos-state` supplies only the registration and traversal mechanics. The
//! provider layer owns strict decoding and the meaning of every emitted edge.

use ryeos_state::object_closure::{
    ObjectContractRegistration, RegisteredObjectEdge, RegisteredObjectExpectation,
    RegisteredObjectLinks,
};
use serde_json::Value;
use std::sync::OnceLock;

/// Install the provider-owned object contracts for this application process.
///
/// Daemon startup and standalone maintenance call the same composition
/// boundary. Repeated application-level calls fold to the one exact registry;
/// the underlying state registry still rejects independent reinstallation.
pub fn install() -> anyhow::Result<()> {
    static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| install_once().map_err(|error| error.to_string()))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn install_once() -> anyhow::Result<()> {
    ryeos_state::object_closure::install_object_contracts(vec![
        ObjectContractRegistration {
            kind: ryeos_provider_contract::PROVIDER_CALL_RECORD_KIND,
            validate: validate_provider_call,
            links: provider_call_links,
        },
        ObjectContractRegistration {
            kind: ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_KIND,
            validate: validate_local_worker_observation,
            links: local_worker_observation_links,
        },
    ])
}

fn validate_provider_call(value: &Value) -> anyhow::Result<()> {
    ryeos_provider_contract::ProviderCallRecord::from_current_value(value).map(|_| ())
}

fn validate_local_worker_observation(value: &Value) -> anyhow::Result<()> {
    ryeos_provider_contract::LocalWorkerObservation::from_current_value(value).map(|_| ())
}

fn provider_call_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let record = ryeos_provider_contract::ProviderCallRecord::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    if let ryeos_provider_contract::TransportCoordinate::AdmittedLocalWorker {
        capsule_hash,
        execution_realization_hash,
        ..
    } = &record.coordinate.transport
    {
        push_kind(
            &mut links,
            capsule_hash,
            ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_KIND,
        );
        push_kind(
            &mut links,
            execution_realization_hash,
            ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND,
        );
    }
    if let Some(hash) = &record.first_observation.execution_identity_attestation_hash {
        push_kind(&mut links, hash, "attestation");
    }
    if let Some(hash) = &record.first_observation.admitted_execution_realization_hash {
        push_kind(
            &mut links,
            hash,
            ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND,
        );
    }
    if let Some(hash) = &record.first_observation.observed_execution_realization_hash {
        push_kind(
            &mut links,
            hash,
            ryeos_state::objects::OBSERVED_EXECUTION_REALIZATION_KIND,
        );
    }
    Ok(links)
}

fn local_worker_observation_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let observation = ryeos_provider_contract::LocalWorkerObservation::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    push_kind(
        &mut links,
        &observation.capsule_hash,
        ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_KIND,
    );
    push_kind(
        &mut links,
        &observation.admitted_execution_realization_hash,
        ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND,
    );
    if let Some(hash) = &observation.observed_execution_realization_hash {
        push_kind(
            &mut links,
            hash,
            ryeos_state::objects::OBSERVED_EXECUTION_REALIZATION_KIND,
        );
    }
    push_kind(
        &mut links,
        &observation.execution_identity_attestation_hash,
        "attestation",
    );
    Ok(links)
}

fn push_kind(links: &mut RegisteredObjectLinks, hash: &str, kind: &'static str) {
    links.object_edges.push(RegisteredObjectEdge {
        hash: hash.to_owned(),
        expected: RegisteredObjectExpectation::Kind(kind),
    });
}
