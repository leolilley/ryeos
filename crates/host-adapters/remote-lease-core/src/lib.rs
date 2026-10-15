//! Provider-neutral core of remote lease lifecycle adapters.
//!
//! A remote lease is one disposable guest a provider runs for a durable
//! controller. This crate owns what every such adapter shares: stage-one
//! admission of a workload's requirement entries against the evidence a
//! signed provider spec yields, guest Ready verification over the lease's
//! `ExecutionChannelBinding`, stage-two admission of the guest's capability
//! report, resume state, the relayed guest-frame transport, the guest egress
//! policy shapes, and the lease operation entry points with their sealed
//! descriptor helpers.
//!
//! What differs by provider is asked through `LeaseProviderFacts`, which each
//! adapter implements over its own parsed signed spec.

mod egress;
mod facts;
mod lease;
mod operation;

pub use egress::{
    AllowListCapability, EgressHostSyntax, EgressPolicy, EgressProtocol, EgressRule,
    MAX_EGRESS_RULES, NetworkPolicyCapability, NetworkPolicyType, valid_egress_host,
};
pub use facts::{
    GUEST_READY_AUTHENTICATED, GuestChannelProfile, LEASE_CREATE_BOUND, LEASE_LIFETIME_BOUND,
    LEASE_NETWORK_POLICY, LEASE_TERMINATE_OBSERVED, LeaseProviderFacts,
};
pub use lease::{
    LEASE_ADMISSION_FD_ENV, LEASE_SCHEMA, LeaseAdmissionRequest, LeaseAdmissionResponse,
    RequirementMet, admit_before_spend, read_lease_admission,
};
pub use operation::{
    admit_lease, await_lease_ready, operation_deadline, read_credential, read_sealed_env,
    request_deadline, write_response, write_response_bounded,
};
