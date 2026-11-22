//! What a lease provider's signed spec establishes, as the shared lease code
//! reads it.
//!
//! The shared code judges requirement entries, verifies guest Ready and runs
//! the lease operations without knowing which provider backs a lease. Each
//! adapter answers these questions from its own signed spec; everything the
//! answers depend on (routes, proof profiles, wire shapes) stays in that
//! adapter.

use std::collections::BTreeSet;

use anyhow::Result;

use crate::egress::{EgressPolicy, NetworkPolicyCapability};

/// Provider-attested lease guarantee names. Requirement sets are open strings
/// carried by workload data; these are only the names a provider spec can
/// derive from its implemented operations.
pub const LEASE_CREATE_BOUND: &str = "lease.create_bound";
pub const LEASE_LIFETIME_BOUND: &str = "lease.lifetime_bound";
pub const LEASE_NETWORK_POLICY: &str = "lease.network_policy";
pub const LEASE_TERMINATE_OBSERVED: &str = "lease.terminate_observed";
pub const GUEST_READY_AUTHENTICATED: &str = "guest.ready_authenticated";

/// Closed set of implemented guest transports. Workload: a durable
/// controller that leases a remote guest and must receive the guest's Ready
/// and probe report before any model contact. A transport is mechanism, so a
/// new one is a reviewed code change plus a signed selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestChannelProfile {
    UnsupportedPending,
    /// The guest connects to its controller; the controller relays the
    /// guest's frames to the adapter, one length-prefixed message each, on
    /// the inherited guest-frames descriptor. The adapter never contacts the
    /// guest or the network itself.
    ControllerRelayedFrames,
}

/// The provider-specific facts lease admission and Ready verification need,
/// answered from one parsed and digest-checked signed provider spec.
pub trait LeaseProviderFacts {
    /// Provider id on the evidence derived from the signed spec, so a
    /// requirement entry can accept or refuse it by `providers`. Refusals
    /// name the spec by it.
    fn evidence_provider(&self) -> &str;

    /// The provider's hard lease lifetime. A lease asking for more cannot be
    /// backed.
    fn maximum_lease_seconds(&self) -> u32;

    /// How many node lease-budget seconds one second of this provider's
    /// lease costs, as the signed spec declares it. Admission reports it on
    /// the admitted verdict, bound to the spec's digest, so the controller
    /// charges its node budget by the weight of the exact spec that judged
    /// the lease. A spec whose weight is outside the contract's bound is
    /// refused when it is parsed.
    fn budget_weight(&self) -> u32;

    /// The guarantee names the signed spec attests, derived only from its
    /// implemented operation kinds and proof profiles.
    fn attested_lease_guarantees(&self) -> BTreeSet<&'static str>;

    /// The guest transport the signed spec selects.
    fn guest_channel_profile(&self) -> Result<GuestChannelProfile>;

    /// Whether the signed spec declares a one-shot guest activation, without
    /// which no guest can send an authenticated Ready.
    fn guest_activation_declared(&self) -> bool;

    /// The network policies the signed spec declares the provider can
    /// create. The shared code derives each lease's exact policy from it.
    fn network_policy_capability(&self) -> &NetworkPolicyCapability;

    /// The provider-attested basis of `lease.network_policy` for this exact
    /// policy, including what the provider's measured behaviour leaves open.
    fn egress_evidence_basis(&self, policy: &EgressPolicy) -> String;

    /// Whether `id` is a well-formed provider occurrence id for a lease.
    fn valid_lease_id(&self, id: &str) -> bool;
}
