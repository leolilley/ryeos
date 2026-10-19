//! The lease lifecycle operations over the facade: create-once allocation,
//! reconciliation by lease name, terminate-then-observe and termination
//! reconciliation.
//!
//! Every operation settles to a definite verdict only on an attested facade
//! reply that checks out; anything else is the operation's `Pending`. A
//! no-occurrence verdict needs proof that nothing was created: the request
//! was never sent, or the facade refused it before creating anything on a
//! first and only contact.

use std::time::Duration;

use ryeos_external_execution_contract::{
    AllocationReservation, BoundOccurrence, LifecycleAdapterResponse, LifecycleOperationCommon,
    TerminationIntent, canonical_json,
};
use ryeos_remote_lease_core::{EgressPolicy, NetworkPolicyType};
use serde::Serialize;
use serde_json::{Value, json};

use crate::facade::{
    CallError, CreateObservation, FacadeClient, FacadeTransport, GetObservation,
    NAME_TAKEN_UNTAGGED, PROVIDER_NOT_FOUND, Reply, expected_policy_record, sorted_compact_sha256,
};
use crate::provider_spec::{
    PROVIDER_MAXIMUM_LEASE_SECONDS, PROVIDER_MINIMUM_LEASE_SECONDS, SPEC_EVIDENCE_PROVIDER,
    valid_sandbox_id,
};

const LEASE_NAME_TAG: &str = "ryeos_lease_name";
const LEASE_SET_TAG: &str = "ryeos_lease_set";
const POLICY_TAG: &str = "ryeos_policy";

pub struct LeaseContext<'a> {
    /// The node's lease set, from its settings.
    pub lease_set: &'a str,
    /// How long to wait for a concurrent create to finish tagging a lease
    /// before concluding it will stay untagged.
    pub untagged_settle: Duration,
}

/// The lease name for one allocation request: unique per request digest, so
/// the facade's per-name uniqueness makes create idempotent and lets a lost
/// create reply be reconciled by name.
pub fn lease_name(request_digest: &str) -> String {
    format!("ryeos-{}", &request_digest[..32])
}

/// The facade's `network` object for `policy`.
pub fn network_object(policy: &EgressPolicy) -> Value {
    match policy.policy_type {
        NetworkPolicyType::DenyAll => json!({ "type": "block-network" }),
        NetworkPolicyType::AllowList => json!({
            "type": "domain-allow-list",
            "rules": policy.rules.iter().map(|rule| json!({
                "protocol": rule.protocol.as_str(),
                "host": rule.domain,
            })).collect::<Vec<_>>(),
        }),
    }
}

#[derive(Serialize)]
struct AllocationEvidence<'a> {
    schema: u32,
    provider: &'static str,
    operation_id: &'a str,
    binding_hash: &'a str,
    request_digest: &'a str,
    sandbox_id: &'a str,
    lease_name: &'a str,
    network_policy: &'a EgressPolicy,
    policy_sha256: &'a str,
    facade_sha256: &'a str,
    response_sha256: &'a str,
    /// True when the lease already existed under this request's name.
    existing_lease: bool,
}

#[derive(Serialize)]
struct ReconciledAllocationEvidence<'a> {
    schema: u32,
    provider: &'static str,
    operation_id: &'a str,
    binding_hash: &'a str,
    request_digest: &'a str,
    sandbox_id: &'a str,
    lease_name: &'a str,
    policy_tag_sha256: &'a str,
    facade_sha256: &'a str,
    response_sha256: &'a str,
}

#[derive(Serialize)]
struct NoOccurrenceEvidence<'a> {
    schema: u32,
    provider: &'static str,
    operation_id: &'a str,
    binding_hash: &'a str,
    request_digest: &'a str,
    lease_name: &'a str,
    basis: &'static str,
    refusal: Option<&'static str>,
}

#[derive(Serialize)]
struct TerminalEvidence<'a> {
    schema: u32,
    provider: &'static str,
    terminal_state: &'static str,
    operation_id: &'a str,
    binding_hash: &'a str,
    request_digest: &'a str,
    occurrence_id: &'a str,
    termination_request_digest: &'a str,
    returncode: i64,
    facade_sha256: &'a str,
    response_sha256: &'a str,
}

fn digest_of<T: Serialize>(evidence: &T) -> Option<String> {
    canonical_json(evidence)
        .ok()
        .map(|bytes| lillux::sha256_hex(&bytes))
}

fn allocation_pending(
    common: &LifecycleOperationCommon,
    reservation: &AllocationReservation,
) -> LifecycleAdapterResponse {
    LifecycleAdapterResponse::AllocationPending {
        operation_id: common.operation_id.clone(),
        request_digest: reservation.request_digest.clone(),
    }
}

fn no_occurrence(
    common: &LifecycleOperationCommon,
    reservation: &AllocationReservation,
    name: &str,
    basis: &'static str,
    refusal: Option<&'static str>,
) -> LifecycleAdapterResponse {
    let evidence = NoOccurrenceEvidence {
        schema: 1,
        provider: SPEC_EVIDENCE_PROVIDER,
        operation_id: &common.operation_id,
        binding_hash: &common.binding_hash,
        request_digest: &reservation.request_digest,
        lease_name: name,
        basis,
        refusal,
    };
    match digest_of(&evidence) {
        Some(digest) => LifecycleAdapterResponse::AllocationNoOccurrence {
            operation_id: common.operation_id.clone(),
            request_digest: reservation.request_digest.clone(),
            provider_observation_digest: digest,
        },
        None => allocation_pending(common, reservation),
    }
}

/// Create the lease once. A repeated create under the same name and policy
/// is the facade's own `name_taken` observation and binds the existing lease.
pub fn allocate<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    context: &LeaseContext<'_>,
    common: &LifecycleOperationCommon,
    reservation: &AllocationReservation,
    policy: &EgressPolicy,
) -> LifecycleAdapterResponse {
    let name = lease_name(&reservation.request_digest);
    let timeout = reservation.maximum_lifetime_seconds;
    if timeout < PROVIDER_MINIMUM_LEASE_SECONDS {
        return no_occurrence(
            common,
            reservation,
            &name,
            "lifetime below the facade minimum",
            None,
        );
    }
    if timeout > PROVIDER_MAXIMUM_LEASE_SECONDS {
        return allocation_pending(common, reservation);
    }
    let network = network_object(policy);
    let mut contacted = false;
    let mut recovered = false;
    let mut retried_missing = false;
    loop {
        let reply = match client.create(&name, context.lease_set, timeout, &network) {
            Ok(reply) => reply,
            Err(CallError::NoRequestSent) if !contacted => {
                return no_occurrence(
                    common,
                    reservation,
                    &name,
                    "transport_no_request_sent",
                    None,
                );
            }
            Err(_) => return allocation_pending(common, reservation),
        };
        if reply.observation::<CreateObservation>().is_some() {
            return bind_created(client, context, common, reservation, policy, &name, &reply);
        }
        match (reply.refusal(), reply.provider_error()) {
            (Some(NAME_TAKEN_UNTAGGED), _) if !recovered => {
                contacted = true;
                recovered = true;
                if !settle_untagged(client, context, &name) {
                    return allocation_pending(common, reservation);
                }
            }
            (Some(code), _) if code.starts_with("name_taken_") => {
                return allocation_pending(common, reservation);
            }
            (Some(code), _) if !contacted => {
                return no_occurrence(common, reservation, &name, "facade_refusal", Some(code));
            }
            (None, Some(PROVIDER_NOT_FOUND)) if !retried_missing => {
                contacted = true;
                retried_missing = true;
            }
            _ => return allocation_pending(common, reservation),
        }
    }
}

/// A lease of this name exists without its tags: a concurrent create may
/// still be tagging it. Get it by name and wait once; terminate it by name
/// only if it stays untagged. Returns whether create may be retried.
fn settle_untagged<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    context: &LeaseContext<'_>,
    name: &str,
) -> bool {
    for attempt in 0..2 {
        let Ok(reply) = client.get_by_name(name) else {
            return false;
        };
        let Some(observation) = reply.observation::<GetObservation>() else {
            return false;
        };
        if !observation.found || observation.tagged == Some(true) {
            return true;
        }
        if attempt == 0 {
            std::thread::sleep(context.untagged_settle);
        }
    }
    let _ = client.terminate_by_name(name);
    false
}

fn bind_created<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    context: &LeaseContext<'_>,
    common: &LifecycleOperationCommon,
    reservation: &AllocationReservation,
    policy: &EgressPolicy,
    name: &str,
    reply: &Reply,
) -> LifecycleAdapterResponse {
    let Some(observation) = reply.observation::<CreateObservation>() else {
        return allocation_pending(common, reservation);
    };
    let consistent = observation.created != observation.name_taken
        && observation.name == name
        && valid_sandbox_id(&observation.sandbox_id)
        && observation.state == "running"
        && observation.policy
            == expected_record(
                client.pin(),
                context.lease_set,
                reservation.maximum_lifetime_seconds,
                policy,
            )
        && sorted_compact_sha256(&observation.policy) == observation.policy_sha256;
    if !consistent {
        return allocation_pending(common, reservation);
    }
    let evidence = AllocationEvidence {
        schema: 1,
        provider: SPEC_EVIDENCE_PROVIDER,
        operation_id: &common.operation_id,
        binding_hash: &common.binding_hash,
        request_digest: &reservation.request_digest,
        sandbox_id: &observation.sandbox_id,
        lease_name: name,
        network_policy: policy,
        policy_sha256: &observation.policy_sha256,
        facade_sha256: &client.pin().sha256,
        response_sha256: &reply.body_sha256,
        existing_lease: observation.name_taken,
    };
    match digest_of(&evidence) {
        Some(digest) => LifecycleAdapterResponse::AllocationBound {
            operation_id: common.operation_id.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: observation.sandbox_id.clone(),
            provider_observation_digest: digest,
        },
        None => allocation_pending(common, reservation),
    }
}

/// The policy record the facade builds for this lease, recomputed from the
/// pinned fixed inputs.
fn expected_record(
    pin: &crate::provider_spec::FacadePin,
    lease_set: &str,
    timeout_seconds: u32,
    policy: &EgressPolicy,
) -> Value {
    let hosts: Vec<&str> = policy
        .rules
        .iter()
        .map(|rule| rule.domain.as_str())
        .collect();
    expected_policy_record(
        pin,
        lease_set,
        timeout_seconds,
        policy.policy_type == NetworkPolicyType::DenyAll,
        &hosts,
    )
}

/// Observation-only: find the lease the request named, by name. A missing
/// lease proves nothing (an ended lease is not found by name either), so it
/// stays pending.
pub fn reconcile_allocation<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    context: &LeaseContext<'_>,
    common: &LifecycleOperationCommon,
    reservation: &AllocationReservation,
    policy: &EgressPolicy,
) -> LifecycleAdapterResponse {
    let name = lease_name(&reservation.request_digest);
    let expected_policy_sha256 = sorted_compact_sha256(&expected_record(
        client.pin(),
        context.lease_set,
        reservation.maximum_lifetime_seconds,
        policy,
    ));
    for attempt in 0..2 {
        let Ok(reply) = client.get_by_name(&name) else {
            break;
        };
        let Some(observation) = reply.observation::<GetObservation>() else {
            break;
        };
        let (Some(sandbox_id), Some(policy_tag)) = (
            observation.sandbox_id.as_deref(),
            observation.tags.get(POLICY_TAG),
        ) else {
            if observation.found && observation.tagged == Some(false) && attempt == 0 {
                std::thread::sleep(context.untagged_settle);
                continue;
            }
            break;
        };
        let exact = observation.found
            && observation.state.as_deref() == Some("running")
            && observation.tagged == Some(true)
            && observation.name_verified == Some(true)
            && valid_sandbox_id(sandbox_id)
            && observation.tags.get(LEASE_NAME_TAG).map(String::as_str) == Some(name.as_str())
            && observation.tags.get(LEASE_SET_TAG).map(String::as_str) == Some(context.lease_set)
            && policy_tag == &expected_policy_sha256;
        if !exact {
            break;
        }
        let evidence = ReconciledAllocationEvidence {
            schema: 1,
            provider: SPEC_EVIDENCE_PROVIDER,
            operation_id: &common.operation_id,
            binding_hash: &common.binding_hash,
            request_digest: &reservation.request_digest,
            sandbox_id,
            lease_name: &name,
            policy_tag_sha256: policy_tag,
            facade_sha256: &client.pin().sha256,
            response_sha256: &reply.body_sha256,
        };
        return match digest_of(&evidence) {
            Some(digest) => LifecycleAdapterResponse::AllocationBound {
                operation_id: common.operation_id.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: sandbox_id.to_owned(),
                provider_observation_digest: digest,
            },
            None => allocation_pending(common, reservation),
        };
    }
    allocation_pending(common, reservation)
}

fn termination_pending(
    common: &LifecycleOperationCommon,
    termination: &TerminationIntent,
) -> LifecycleAdapterResponse {
    LifecycleAdapterResponse::TerminationPending {
        operation_id: common.operation_id.clone(),
        termination_request_digest: termination.termination_request_digest.clone(),
    }
}

/// Ask the facade to terminate once. The acknowledgement alone is never
/// terminal evidence; only the exact observation that follows settles.
pub fn terminate<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    common: &LifecycleOperationCommon,
    occurrence: &BoundOccurrence,
    termination: &TerminationIntent,
) -> LifecycleAdapterResponse {
    if !valid_sandbox_id(&occurrence.occurrence_id) {
        return termination_pending(common, termination);
    }
    let _acknowledgement = client.terminate_by_id(&occurrence.occurrence_id);
    observe_termination(client, common, occurrence, termination)
}

/// Observe the exact occurrence: terminal only when the facade reports this
/// very sandbox terminated with an exit status.
pub fn observe_termination<T: FacadeTransport>(
    client: &FacadeClient<'_, T>,
    common: &LifecycleOperationCommon,
    occurrence: &BoundOccurrence,
    termination: &TerminationIntent,
) -> LifecycleAdapterResponse {
    if !valid_sandbox_id(&occurrence.occurrence_id) {
        return termination_pending(common, termination);
    }
    let Ok(reply) = client.get_by_id(&occurrence.occurrence_id) else {
        return termination_pending(common, termination);
    };
    let Some(observation) = reply.observation::<GetObservation>() else {
        return termination_pending(common, termination);
    };
    let (true, Some(id), Some("terminated"), Some(returncode), Some(true)) = (
        observation.found,
        observation.sandbox_id.as_deref(),
        observation.state.as_deref(),
        observation.returncode,
        observation.tagged,
    ) else {
        return termination_pending(common, termination);
    };
    if id != occurrence.occurrence_id {
        return termination_pending(common, termination);
    }
    let evidence = TerminalEvidence {
        schema: 1,
        provider: SPEC_EVIDENCE_PROVIDER,
        terminal_state: "terminated",
        operation_id: &common.operation_id,
        binding_hash: &common.binding_hash,
        request_digest: &occurrence.request_digest,
        occurrence_id: &occurrence.occurrence_id,
        termination_request_digest: &termination.termination_request_digest,
        returncode,
        facade_sha256: &client.pin().sha256,
        response_sha256: &reply.body_sha256,
    };
    match digest_of(&evidence) {
        Some(digest) => LifecycleAdapterResponse::OccurrenceTerminal {
            operation_id: common.operation_id.clone(),
            termination_request_digest: termination.termination_request_digest.clone(),
            provider_observation_digest: digest,
        },
        None => termination_pending(common, termination),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ryeos_remote_lease_core::NetworkPolicyCapability;

    use super::*;
    use crate::facade::test_support::*;
    use crate::facade::{FacadeClient, TransportFailure};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SANDBOX: &str = "sb-abc123";
    const LEASE_SET: &str = "node-a";

    fn common() -> LifecycleOperationCommon {
        LifecycleOperationCommon {
            schema: 1,
            protocol: "ryeos.external-execution.lifecycle-adapter.v2".into(),
            operation_id: "op-1".into(),
            binding_hash: "b".repeat(64),
            settings_digest: "c".repeat(64),
        }
    }

    fn reservation(lifetime: u32) -> AllocationReservation {
        AllocationReservation {
            placement_thread_id: "t".into(),
            admitted_capsule_hash: "d".repeat(64),
            base_snapshot_hash: "e".repeat(64),
            request_digest: DIGEST.into(),
            maximum_lifetime_seconds: lifetime,
            contact_deadline_ms: 1,
        }
    }

    fn context() -> LeaseContext<'static> {
        LeaseContext {
            lease_set: LEASE_SET,
            untagged_settle: Duration::ZERO,
        }
    }

    fn deny_all() -> EgressPolicy {
        EgressPolicy::deny_all()
    }

    fn allow_list() -> EgressPolicy {
        let capability = NetworkPolicyCapability {
            creatable_types: vec![NetworkPolicyType::DenyAll, NetworkPolicyType::AllowList],
            allow_list: Some(ryeos_remote_lease_core::AllowListCapability {
                rule_protocol: ryeos_remote_lease_core::EgressProtocol::Https,
                max_rules: 4,
                host_syntax: ryeos_remote_lease_core::EgressHostSyntax::ExactDnsName,
            }),
        };
        let hosts: BTreeSet<String> =
            ["b.example.com".to_owned(), "a.example.com".to_owned()].into();
        capability.egress_policy("test", &hosts).unwrap()
    }

    fn record(policy: &EgressPolicy, lifetime: u32) -> Value {
        expected_record(&pin(), LEASE_SET, lifetime, policy)
    }

    fn created_fields(policy: &EgressPolicy, lifetime: u32, created: bool) -> Value {
        let record = record(policy, lifetime);
        let mut fields = json!({
            "ok": true,
            "created": created,
            "name": lease_name(DIGEST),
            "policy_sha256": sorted_compact_sha256(&record),
            "policy": record,
            "sandbox_id": SANDBOX,
            "state": "running",
            "returncode": null,
            "tags": {},
        });
        if !created {
            fields["name_taken"] = true.into();
        }
        fields
    }

    fn run_allocate(
        steps: Vec<Step>,
        policy: &EgressPolicy,
        lifetime: u32,
    ) -> (LifecycleAdapterResponse, Scripted) {
        let fake = Scripted::new(steps);
        let pin = pin();
        let response = allocate(
            &FacadeClient::new(&fake, &pin),
            &context(),
            &common(),
            &reservation(lifetime),
            policy,
        );
        (response, fake)
    }

    fn is_pending(response: &LifecycleAdapterResponse) -> bool {
        matches!(response, LifecycleAdapterResponse::AllocationPending { .. })
    }

    fn get_fields(tagged: bool, state: &str, code: Option<i64>) -> Value {
        get_fields_with(
            tagged,
            state,
            code,
            &sorted_compact_sha256(&record(&deny_all(), 600)),
        )
    }

    fn get_fields_with(tagged: bool, state: &str, code: Option<i64>, policy_tag: &str) -> Value {
        let name = lease_name(DIGEST);
        json!({
            "ok": true, "found": true, "sandbox_id": SANDBOX, "state": state,
            "returncode": code, "tagged": tagged, "name_verified": state == "running",
            "tags": if tagged { json!({
                (LEASE_NAME_TAG): name, (LEASE_SET_TAG): LEASE_SET, (POLICY_TAG): policy_tag,
            }) } else { json!({}) },
        })
    }

    #[test]
    fn create_binds_and_sends_the_exact_request() {
        for policy in [deny_all(), allow_list()] {
            let (response, fake) = run_allocate(
                vec![envelope("create", 200, created_fields(&policy, 600, true))],
                &policy,
                600,
            );
            let LifecycleAdapterResponse::AllocationBound {
                occurrence_id,
                provider_observation_digest,
                ..
            } = response
            else {
                panic!("not bound: {response:?}");
            };
            assert_eq!(occurrence_id, SANDBOX);
            assert_eq!(provider_observation_digest.len(), 64);
            let requests = fake.requests.borrow();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].0, crate::provider_spec::RouteName::Create);
            assert_eq!(requests[0].1["name"], lease_name(DIGEST));
            assert_eq!(requests[0].1["lease_set"], LEASE_SET);
            assert_eq!(requests[0].1["timeout_seconds"], 600);
            assert_eq!(requests[0].1["network"], network_object(&policy));
        }
        let network = network_object(&allow_list());
        assert_eq!(
            network,
            json!({"type": "domain-allow-list", "rules": [
                {"protocol": "https", "host": "a.example.com"},
                {"protocol": "https", "host": "b.example.com"},
            ]})
        );
        assert_eq!(
            network_object(&deny_all()),
            json!({"type": "block-network"})
        );
    }

    #[test]
    fn name_taken_with_the_same_policy_binds_the_existing_lease() {
        let policy = allow_list();
        let (response, _) = run_allocate(
            vec![envelope("create", 200, created_fields(&policy, 600, false))],
            &policy,
            600,
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::AllocationBound { .. }
        ));
    }

    #[test]
    fn bound_evidence_differs_for_an_existing_lease() {
        let policy = deny_all();
        let digest = |created: bool| {
            let (response, _) = run_allocate(
                vec![envelope(
                    "create",
                    200,
                    created_fields(&policy, 600, created),
                )],
                &policy,
                600,
            );
            match response {
                LifecycleAdapterResponse::AllocationBound {
                    provider_observation_digest,
                    ..
                } => provider_observation_digest,
                other => panic!("{other:?}"),
            }
        };
        assert_ne!(digest(true), digest(false));
    }

    #[test]
    fn policy_digest_and_record_mismatches_stay_pending() {
        let policy = allow_list();
        let mut digest_off = created_fields(&policy, 600, true);
        digest_off["policy_sha256"] = "0".repeat(64).into();
        let mut other_hosts = created_fields(&policy, 600, true);
        other_hosts["policy"]["outbound_domain_allowlist"] = json!(["a.example.com"]);
        other_hosts["policy_sha256"] = sorted_compact_sha256(&other_hosts["policy"]).into();
        let mut other_lifetime = created_fields(&policy, 700, true);
        other_lifetime["policy_sha256"] = sorted_compact_sha256(&other_lifetime["policy"]).into();
        let mut other_set = created_fields(&policy, 600, true);
        other_set["policy"]["lease_set"] = "node-b".into();
        other_set["policy_sha256"] = sorted_compact_sha256(&other_set["policy"]).into();
        let mut blocked = created_fields(&policy, 600, true);
        blocked["policy"]["block_network"] = true.into();
        blocked["policy_sha256"] = sorted_compact_sha256(&blocked["policy"]).into();
        let mut other_name = created_fields(&policy, 600, true);
        other_name["name"] = "ryeos-other".into();
        let mut bad_id = created_fields(&policy, 600, true);
        bad_id["sandbox_id"] = "sbx-1".into();
        let mut not_running = created_fields(&policy, 600, true);
        not_running["state"] = "terminated".into();
        let mut both = created_fields(&policy, 600, true);
        both["name_taken"] = true.into();
        for fields in [
            digest_off,
            other_hosts,
            other_lifetime,
            other_set,
            blocked,
            other_name,
            bad_id,
            not_running,
            both,
        ] {
            let (response, _) =
                run_allocate(vec![envelope("create", 200, fields.clone())], &policy, 600);
            assert!(is_pending(&response), "{fields}");
        }
    }

    #[test]
    fn pin_mismatch_and_unattested_replies_stay_pending() {
        let policy = deny_all();
        let (response, _) = run_allocate(
            vec![envelope_with(
                "create",
                200,
                &"9".repeat(64),
                created_fields(&policy, 600, true),
            )],
            &policy,
            600,
        );
        assert!(is_pending(&response));
        let (response, _) = run_allocate(
            vec![Step::Reply(crate::facade::RawReply {
                status: 401,
                json_content_type: true,
                body: b"{\"error\":\"proxy\"}".to_vec(),
            })],
            &policy,
            600,
        );
        assert!(is_pending(&response));
    }

    #[test]
    fn transport_contact_state_decides_no_occurrence() {
        let policy = deny_all();
        let (response, _) = run_allocate(
            vec![Step::Fail(TransportFailure::NoRequestSent)],
            &policy,
            600,
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::AllocationNoOccurrence { .. }
        ));
        let (response, _) = run_allocate(
            vec![Step::Fail(TransportFailure::MayHaveBeenSent)],
            &policy,
            600,
        );
        assert!(is_pending(&response));
    }

    #[test]
    fn lifetimes_the_facade_refuses_never_reach_it() {
        let policy = deny_all();
        for lifetime in [1, 59] {
            let (response, fake) = run_allocate(vec![], &policy, lifetime);
            assert!(matches!(
                response,
                LifecycleAdapterResponse::AllocationNoOccurrence { .. }
            ));
            assert!(fake.requests.borrow().is_empty());
        }
        let (response, _) = run_allocate(
            vec![envelope("create", 200, created_fields(&policy, 60, true))],
            &policy,
            60,
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::AllocationBound { .. }
        ));
    }

    #[test]
    fn refusals_map_to_no_occurrence_or_pending() {
        let policy = deny_all();
        for code in [
            "invalid_host",
            "too_many_rules",
            "timeout_out_of_range",
            "unknown_field",
        ] {
            let (response, _) = run_allocate(vec![refusal("create", code)], &policy, 600);
            assert!(
                matches!(
                    response,
                    LifecycleAdapterResponse::AllocationNoOccurrence { .. }
                ),
                "{code}"
            );
        }
        for code in ["name_taken_policy_mismatch", "name_taken_tag_mismatch"] {
            let (response, fake) = run_allocate(vec![refusal("create", code)], &policy, 600);
            assert!(is_pending(&response), "{code}");
            assert_eq!(fake.requests.borrow().len(), 1);
        }
    }

    #[test]
    fn untagged_name_is_got_and_retried_before_any_terminate() {
        let policy = deny_all();
        // A concurrent create finished tagging: the retried create is the
        // facade's name_taken observation.
        let (response, fake) = run_allocate(
            vec![
                refusal("create", NAME_TAKEN_UNTAGGED),
                envelope("get", 200, get_fields(true, "running", None)),
                envelope("create", 200, created_fields(&policy, 600, false)),
            ],
            &policy,
            600,
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::AllocationBound { .. }
        ));
        assert_eq!(
            fake.routes(),
            [
                crate::provider_spec::RouteName::Create,
                crate::provider_spec::RouteName::GetByName,
                crate::provider_spec::RouteName::Create,
            ]
        );
        // Tagged only on the second get, after the settle wait.
        let (response, fake) = run_allocate(
            vec![
                refusal("create", NAME_TAKEN_UNTAGGED),
                envelope("get", 200, get_fields(false, "running", None)),
                envelope("get", 200, get_fields(true, "running", None)),
                envelope("create", 200, created_fields(&policy, 600, false)),
            ],
            &policy,
            600,
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::AllocationBound { .. }
        ));
        assert_eq!(fake.requests.borrow().len(), 4);
    }

    #[test]
    fn a_lease_that_stays_untagged_is_terminated_by_name_after_two_gets() {
        let policy = deny_all();
        let (response, fake) = run_allocate(
            vec![
                refusal("create", NAME_TAKEN_UNTAGGED),
                envelope("get", 200, get_fields(false, "running", None)),
                envelope("get", 200, get_fields(false, "running", None)),
                envelope(
                    "terminate",
                    200,
                    json!({"ok": true, "found": true, "tagged": false}),
                ),
            ],
            &policy,
            600,
        );
        assert!(is_pending(&response));
        let requests = fake.requests.borrow();
        assert_eq!(
            requests.iter().map(|(route, _)| *route).collect::<Vec<_>>(),
            [
                crate::provider_spec::RouteName::Create,
                crate::provider_spec::RouteName::GetByName,
                crate::provider_spec::RouteName::GetByName,
                crate::provider_spec::RouteName::Terminate,
            ]
        );
        assert_eq!(requests[3].1, json!({"name": lease_name(DIGEST)}));
    }

    #[test]
    fn untagged_recovery_gives_up_after_one_round_and_never_claims_no_occurrence() {
        let policy = deny_all();
        let (response, _) = run_allocate(
            vec![
                refusal("create", NAME_TAKEN_UNTAGGED),
                envelope("get", 200, get_fields(true, "running", None)),
                refusal("create", NAME_TAKEN_UNTAGGED),
            ],
            &policy,
            600,
        );
        assert!(is_pending(&response));
        // A refusal after earlier contact is not proof of no occurrence.
        let (response, _) = run_allocate(
            vec![
                refusal("create", NAME_TAKEN_UNTAGGED),
                envelope("get", 200, get_fields(true, "running", None)),
                refusal("create", "invalid_host"),
            ],
            &policy,
            600,
        );
        assert!(is_pending(&response));
        let (response, _) = run_allocate(
            vec![
                refusal("create", NAME_TAKEN_UNTAGGED),
                envelope("get", 200, get_fields(true, "running", None)),
                Step::Fail(TransportFailure::NoRequestSent),
            ],
            &policy,
            600,
        );
        assert!(is_pending(&response));
    }

    #[test]
    fn a_lease_that_ended_between_duplicate_error_and_lookup_is_retried_once() {
        let policy = deny_all();
        let (response, _) = run_allocate(
            vec![
                provider_error("create", PROVIDER_NOT_FOUND),
                envelope("create", 200, created_fields(&policy, 600, true)),
            ],
            &policy,
            600,
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::AllocationBound { .. }
        ));
        let (response, fake) = run_allocate(
            vec![
                provider_error("create", PROVIDER_NOT_FOUND),
                provider_error("create", PROVIDER_NOT_FOUND),
            ],
            &policy,
            600,
        );
        assert!(is_pending(&response));
        assert_eq!(fake.requests.borrow().len(), 2);
        let (response, _) = run_allocate(
            vec![provider_error("create", "InternalError")],
            &policy,
            600,
        );
        assert!(is_pending(&response));
    }

    fn run_reconcile(steps: Vec<Step>) -> (LifecycleAdapterResponse, Scripted) {
        let fake = Scripted::new(steps);
        let pin = pin();
        let response = reconcile_allocation(
            &FacadeClient::new(&fake, &pin),
            &context(),
            &common(),
            &reservation(600),
            &deny_all(),
        );
        (response, fake)
    }

    #[test]
    fn reconcile_binds_only_an_exact_running_tagged_lease() {
        let (response, fake) = run_reconcile(vec![envelope(
            "get",
            200,
            get_fields(true, "running", None),
        )]);
        let LifecycleAdapterResponse::AllocationBound { occurrence_id, .. } = response else {
            panic!("{response:?}");
        };
        assert_eq!(occurrence_id, SANDBOX);
        assert_eq!(
            fake.requests.borrow()[0].1,
            json!({"name": lease_name(DIGEST)})
        );
        let mutations: [fn(&mut Value); 6] = [
            |f| f["found"] = false.into(),
            |f| f["state"] = "terminated".into(),
            |f| f["name_verified"] = false.into(),
            |f| f["tags"][LEASE_SET_TAG] = "other".into(),
            |f| f["tags"][LEASE_NAME_TAG] = "ryeos-other".into(),
            |f| f["sandbox_id"] = "sbx-1".into(),
        ];
        for mutate in mutations {
            let mut fields = get_fields(true, "running", None);
            mutate(&mut fields);
            let (response, _) = run_reconcile(vec![envelope("get", 200, fields.clone())]);
            assert!(is_pending(&response), "{fields}");
        }
    }

    #[test]
    fn reconcile_refuses_a_lease_whose_policy_tag_is_not_the_expected_digest() {
        // Another network, another lifetime, or a tag the facade's fixed
        // inputs cannot produce all differ from the recomputed digest.
        let other_network = sorted_compact_sha256(&record(&allow_list(), 600));
        let other_lifetime = sorted_compact_sha256(&record(&deny_all(), 601));
        for tag in [other_network, other_lifetime, "f".repeat(64)] {
            let (response, _) = run_reconcile(vec![envelope(
                "get",
                200,
                get_fields_with(true, "running", None, &tag),
            )]);
            assert!(is_pending(&response), "{tag}");
        }
    }

    #[test]
    fn create_refuses_a_record_built_from_other_fixed_inputs() {
        let policy = deny_all();
        let mut fields = created_fields(&policy, 600, true);
        fields["policy"]["memory_mib"] = 2048.into();
        fields["policy_sha256"] = sorted_compact_sha256(&fields["policy"]).into();
        let (response, _) = run_allocate(vec![envelope("create", 200, fields)], &policy, 600);
        assert!(is_pending(&response));
    }

    #[test]
    fn reconcile_not_found_and_failures_stay_pending() {
        let (response, _) = run_reconcile(vec![envelope(
            "get",
            200,
            json!({"ok": true, "found": false}),
        )]);
        assert!(is_pending(&response));
        let (response, _) = run_reconcile(vec![Step::Fail(TransportFailure::NoRequestSent)]);
        assert!(is_pending(&response));
        let (response, _) = run_reconcile(vec![provider_error("get", "InternalError")]);
        assert!(is_pending(&response));
        let (response, _) = run_reconcile(vec![refusal("get", "invalid_name")]);
        assert!(is_pending(&response));
    }

    #[test]
    fn reconcile_waits_once_for_a_lease_still_being_tagged() {
        let (response, fake) = run_reconcile(vec![
            envelope("get", 200, get_fields(false, "running", None)),
            envelope("get", 200, get_fields(true, "running", None)),
        ]);
        assert!(matches!(
            response,
            LifecycleAdapterResponse::AllocationBound { .. }
        ));
        assert_eq!(fake.requests.borrow().len(), 2);
        let (response, fake) = run_reconcile(vec![
            envelope("get", 200, get_fields(false, "running", None)),
            envelope("get", 200, get_fields(false, "running", None)),
        ]);
        assert!(is_pending(&response));
        assert_eq!(fake.requests.borrow().len(), 2);
        assert!(fake.exhausted());
    }

    fn occurrence() -> BoundOccurrence {
        BoundOccurrence {
            request_digest: DIGEST.into(),
            occurrence_id: SANDBOX.into(),
        }
    }

    fn intent() -> TerminationIntent {
        TerminationIntent {
            termination_request_digest: "a".repeat(64),
        }
    }

    #[test]
    fn terminate_then_exact_observation_settles() {
        let fake = Scripted::new(vec![
            envelope(
                "terminate",
                200,
                json!({"ok": true, "found": true, "tagged": true, "already_terminal": false}),
            ),
            envelope("get", 200, get_fields(true, "terminated", Some(137))),
        ]);
        let pin = pin();
        let response = terminate(
            &FacadeClient::new(&fake, &pin),
            &common(),
            &occurrence(),
            &intent(),
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::OccurrenceTerminal { .. }
        ));
        assert_eq!(
            fake.routes(),
            [
                crate::provider_spec::RouteName::Terminate,
                crate::provider_spec::RouteName::GetById
            ]
        );
        assert_eq!(fake.requests.borrow()[1].1, json!({"sandbox_id": SANDBOX}));
    }

    #[test]
    fn terminate_acknowledgement_alone_never_settles() {
        for ack in [
            envelope(
                "terminate",
                200,
                json!({"ok": true, "found": true, "state": "terminated", "returncode": 0}),
            ),
            Step::Fail(TransportFailure::MayHaveBeenSent),
            provider_error("terminate", "terminal_state_not_observed"),
        ] {
            let fake = Scripted::new(vec![
                ack,
                envelope("get", 200, get_fields(true, "running", None)),
            ]);
            let pin = pin();
            let response = terminate(
                &FacadeClient::new(&fake, &pin),
                &common(),
                &occurrence(),
                &intent(),
            );
            assert!(matches!(
                response,
                LifecycleAdapterResponse::TerminationPending { .. }
            ));
        }
    }

    #[test]
    fn terminal_observation_must_be_exact() {
        let mutations: [fn(&mut Value); 6] = [
            |f| f["found"] = false.into(),
            |f| f["state"] = "running".into(),
            |f| f["returncode"] = Value::Null,
            |f| f["tagged"] = false.into(),
            |f| f["sandbox_id"] = "sb-other".into(),
            |f| f["returncode"] = "0".into(),
        ];
        for mutate in mutations {
            let mut fields = get_fields(true, "terminated", Some(0));
            mutate(&mut fields);
            let fake = Scripted::new(vec![envelope("get", 200, fields.clone())]);
            let pin = pin();
            let response = observe_termination(
                &FacadeClient::new(&fake, &pin),
                &common(),
                &occurrence(),
                &intent(),
            );
            assert!(
                matches!(
                    response,
                    LifecycleAdapterResponse::TerminationPending { .. }
                ),
                "{fields}"
            );
        }
        for step in [
            Step::Fail(TransportFailure::NoRequestSent),
            refusal("get", "not_facade_sandbox"),
            provider_error("get", "NotFoundError"),
            envelope_with(
                "get",
                200,
                &"1".repeat(64),
                get_fields(true, "terminated", Some(0)),
            ),
        ] {
            let fake = Scripted::new(vec![step]);
            let pin = pin();
            let response = observe_termination(
                &FacadeClient::new(&fake, &pin),
                &common(),
                &occurrence(),
                &intent(),
            );
            assert!(matches!(
                response,
                LifecycleAdapterResponse::TerminationPending { .. }
            ));
        }
        let mut malformed = occurrence();
        malformed.occurrence_id = "sbx-1".into();
        let fake = Scripted::new(vec![]);
        let pin = pin();
        let response = terminate(
            &FacadeClient::new(&fake, &pin),
            &common(),
            &malformed,
            &intent(),
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::TerminationPending { .. }
        ));
        assert!(fake.requests.borrow().is_empty());
    }

    #[test]
    fn reconcile_termination_observes_without_a_mutation() {
        let fake = Scripted::new(vec![envelope(
            "get",
            200,
            get_fields(true, "terminated", Some(0)),
        )]);
        let pin = pin();
        let response = observe_termination(
            &FacadeClient::new(&fake, &pin),
            &common(),
            &occurrence(),
            &intent(),
        );
        assert!(matches!(
            response,
            LifecycleAdapterResponse::OccurrenceTerminal { .. }
        ));
        assert_eq!(fake.routes(), [crate::provider_spec::RouteName::GetById]);
    }

    #[test]
    fn lease_names_follow_the_facade_name_rule() {
        let name = lease_name(DIGEST);
        assert_eq!(name, "ryeos-0123456789abcdef0123456789abcdef");
        assert!(name.len() <= 63);
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        );
    }

    #[test]
    fn upload_and_run_once_are_wired_through_the_client() {
        let fake = Scripted::new(vec![
            envelope(
                "upload",
                200,
                json!({"ok": true, "sandbox_id": SANDBOX, "bytes": 3, "sha256": lillux::sha256_hex(b"abc")}),
            ),
            refusal("run_once", "upload_not_complete"),
        ]);
        let pin = pin();
        let client = FacadeClient::new(&fake, &pin);
        assert!(
            client
                .upload(SANDBOX, b"abc")
                .unwrap()
                .observation::<crate::facade::UploadObservation>()
                .is_some()
        );
        assert_eq!(
            client.run_once(SANDBOX, b"x").unwrap().refusal(),
            Some("upload_not_complete")
        );
    }
}
