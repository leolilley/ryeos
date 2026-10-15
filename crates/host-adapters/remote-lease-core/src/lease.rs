//! Lease admission and authenticated guest Ready for remote leases.
//!
//! Workload: a durable controller keeps its application server and its
//! credential profile. Each remote guest is a disposable lease that runs only
//! an execution server, which executes model-directed commands and file
//! effects inside the guest. The credential home never enters the guest.
//!
//! Requirements are Lillux capability floor entries: a bare name is met only
//! by a measured proof, an object entry may widen the grades it accepts (for
//! example `provider_attested`), narrow the providers it accepts, or list
//! `any_of` alternatives. Each entry is judged by Lillux `satisfies`, and
//! admission records which name, grade and provider met it.
//!
//! Admission has two stages, and each refuses with a plain reason:
//! - before any spend, the workload's lease entries are judged against the
//!   provider-attested evidence derived from the signed provider spec;
//! - after activation and before any model contact, the guest's own
//!   capability report must meet the guest entries of the exact request that
//!   stage one admitted. A refusal at this stage terminates the lease; the
//!   turn never runs on a weaker guest.
//!
//! The capability report is trusted only because it precedes any workload
//! code. The per-lease supervisor key lives in the guest next to the
//! model-directed commands the execution server runs, so once workload code
//! has run, the guest can sign anything under that key, including a weaker
//! report. The report is therefore accepted exactly once, as the first frame
//! after Ready; its stage-two result is retained in the lease's channel state,
//! and a resumed admission carries that result back instead of reading a new
//! report. A capability report arriving after the first terminates the lease.
//!
//! The guest channel is the external-execution channel. The controller forms
//! its `ExecutionChannelBinding`, whose supervisor key is a per-lease guest
//! key the controller minted and whose public half it retained before any
//! contact; the private half reaches only the exact lease through the
//! one-shot activation upload. Ready is the channel's first supervisor frame
//! and carries the guest boot id, the activation inputs the guest verified
//! and its ready time, which may lead the controller's clock by at most the
//! bounded skew the lease request declares. Every later guest frame continues
//! the frame chain (sequence and predecessor digest) from Ready, and the
//! capability report names the same boot. A changed boot, a second Ready or a
//! frame off the chain ends the lease. Each admission returns the chain state
//! with the retained stage-two result, and a later admission of the same
//! lease resumes from it, so the chain never restarts.
//!
//! The resumed `GuestChannelState` is consistency-checked, not
//! authenticated: admission checks that its boot id and digests are well
//! formed, that its report frame is not its Ready, that its next sequence
//! lies within the channel's bounds, and that its retained stage-two result
//! covers exactly the request's guest entries and meets each, but no
//! signature binds the state itself. The controller is its trust root: the
//! state arrives only in the controller's own admission request, never from
//! the guest or the provider, so a controller that altered it could equally
//! have altered the request it rides in.
//!
//! This module owns no signature domain: Ready and every guest frame are
//! signed under the channel's frame domain. What remains provider-specific is
//! asked through `LeaseProviderFacts`: which evidence the signed spec yields,
//! the provider's lifetime and egress, and the lease id's form.
//!
//! The adapter never contacts the guest. `GuestFrameTransport` is the seam a
//! transport plugs into, and the signed spec selects which one runs. The
//! implemented transport is controller-relayed: the guest dials its
//! controller, and the controller hands the adapter the guest's frames on an
//! inherited connected byte stream, one length-prefixed message per frame.
//! The adapter reads exactly Ready and the capability report from it, each
//! within the frame bound and before the admission's receive deadline, and
//! never reads past them. Under a spec that declares no transport,
//! `open_guest_channel` refuses.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;

use anyhow::{Context as _, Result, bail, ensure};
use lillux::process_control::{CapabilityEvidence, CapabilityRequirement, ProofSource, satisfies};
use lillux::time::MonotonicDeadline;
pub use ryeos_external_execution_contract::LEASE_ADMISSION_FD_ENV;
use ryeos_external_execution_contract::{
    LIFECYCLE_GUEST_FRAMES_FD_ENV, MAX_LIFECYCLE_GUEST_FRAME_MESSAGE_BYTES, canonical_json,
    length_prefixed,
};
use ryeos_state::external_execution::{
    ChannelDirection, ExecutionChannelBinding, ExecutionChannelPayload, SignedExecutionFrame,
    SupervisorReadyBoot,
};
use serde::{Deserialize, Serialize};

use crate::egress::{EgressPolicy, MAX_EGRESS_RULES, valid_egress_host};
use crate::facts::{
    GUEST_READY_AUTHENTICATED, GuestChannelProfile, LEASE_LIFETIME_BOUND, LEASE_NETWORK_POLICY,
    LeaseProviderFacts,
};

pub const LEASE_SCHEMA: u32 = 3;
pub(crate) const MAX_LEASE_REQUEST_BYTES: usize = 64 * 1024;
/// How far a guest Ready time may lead the controller's clock when the lease
/// request declares no skew of its own.
const DEFAULT_READY_CLOCK_SKEW_MS: u32 = 5_000;
/// The most skew a lease request may declare. Beyond it a Ready time is a
/// claim about the future, not clock drift.
const MAX_READY_CLOCK_SKEW_MS: u32 = 30_000;
const MAX_GUEST_FRAME_BYTES: usize = 64 * 1024;
const MAX_REQUIREMENTS: usize = 64;
const MAX_NAME_BYTES: usize = 128;
const MAX_TERMINATE_REASON_BYTES: usize = 2048;

/// Stage-one input: one workload's requirement set, as carried by its signed
/// worker or launch data.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseAdmissionRequest {
    pub schema: u32,
    pub adapter_id: String,
    pub provider_spec_sha256: String,
    /// Workload class id from signed data, used only in refusal reasons.
    pub workload: String,
    /// Entries the provider must meet before the lease is created.
    pub requires_before_spend: Vec<CapabilityRequirement>,
    /// Entries only the activated guest can meet with its capability report.
    /// Reaching them needs an authenticated guest Ready, so stage one checks
    /// that too.
    pub requires_from_guest: Vec<CapabilityRequirement>,
    /// Hosts the guest may reach over HTTPS, as exact lowercase DNS names
    /// with no scheme, port or path; a `*.` wildcard only as the leftmost
    /// label, and only where the signed spec accepts wildcards. A remote
    /// lease names exactly its controller's hostname. Empty means deny-all.
    pub guest_network_allow: BTreeSet<String>,
    pub maximum_lifetime_seconds: u32,
    pub activation_deadline_seconds: u32,
    /// How far, in milliseconds, the guest's Ready time may lead the
    /// controller's clock, at most 30 s. A remote lease's guest clock
    /// drifts independently of its controller's; absent means 5 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_clock_skew_ms: Option<u32>,
}

impl LeaseAdmissionRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == LEASE_SCHEMA,
            "unsupported lease admission schema"
        );
        ensure!(
            canonical_digest(&self.provider_spec_sha256),
            "lease admission provider spec digest is invalid"
        );
        ensure!(
            valid_name(&self.workload),
            "lease admission workload id is invalid"
        );
        validate_requirements(&self.requires_before_spend)?;
        validate_requirements(&self.requires_from_guest)?;
        ensure!(
            u32::try_from(self.guest_network_allow.len())
                .is_ok_and(|count| count <= MAX_EGRESS_RULES)
                && self
                    .guest_network_allow
                    .iter()
                    .all(|host| valid_egress_host(host, true)),
            "lease admission guest network allow-list is invalid"
        );
        ensure!(
            self.maximum_lifetime_seconds > 0 && self.activation_deadline_seconds > 0,
            "lease admission lifetime or activation deadline is zero"
        );
        ensure!(
            self.ready_clock_skew_ms
                .is_none_or(|skew| skew <= MAX_READY_CLOCK_SKEW_MS),
            "lease admission declares a Ready clock skew above {MAX_READY_CLOCK_SKEW_MS} ms"
        );
        Ok(())
    }

    fn ready_clock_skew_ms(&self) -> i64 {
        i64::from(
            self.ready_clock_skew_ms
                .unwrap_or(DEFAULT_READY_CLOCK_SKEW_MS),
        )
    }
}

/// Which name, grade and provider met one requirement entry.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequirementMet {
    pub name: String,
    pub source: ProofSource,
    pub provider: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum LeaseAdmissionResponse {
    Admitted {
        schema: u32,
        workload: String,
        request_sha256: String,
        provider_spec_sha256: String,
        /// Keyed by entry label.
        met_by: BTreeMap<String, RequirementMet>,
        /// The exact network policy the lease's create will request and its
        /// echo must repeat.
        network_policy: EgressPolicy,
        /// The provider-attested `lease.network_policy` evidence for it.
        network_policy_evidence: CapabilityEvidence,
    },
    Refused {
        schema: u32,
        workload: String,
        request_sha256: String,
        provider_spec_sha256: String,
        /// Each unmet entry label with its plain reason.
        unmet: BTreeMap<String, String>,
        reason: String,
    },
}

/// A lifecycle operation for a remote lease is handed the lease's admitted
/// request on its own sealed descriptor: create and every read of the lease
/// take its guest egress list. An operation without one serves no
/// lease, so its egress is deny-all.
pub fn read_lease_admission() -> Result<Option<LeaseAdmissionRequest>> {
    if std::env::var_os(LEASE_ADMISSION_FD_ENV).is_none() {
        return Ok(None);
    }
    let bytes = crate::operation::read_sealed_env(LEASE_ADMISSION_FD_ENV, MAX_LEASE_REQUEST_BYTES)?;
    let admission: LeaseAdmissionRequest =
        ryeos_external_execution_contract::from_json_slice_strict(&bytes, MAX_LEASE_REQUEST_BYTES)?;
    ensure!(
        canonical_json(&admission)? == bytes,
        "lease admission descriptor is noncanonical"
    );
    admission.validate()?;
    Ok(Some(admission))
}

/// The provider-attested evidence the signed spec yields, one entry per
/// guarantee derived from an implemented operation kind. `lease.network_policy`
/// is present only when the spec can create the lease's exact `egress`, and
/// its basis names that policy.
fn spec_evidence(
    spec: &dyn LeaseProviderFacts,
    egress: Option<&EgressPolicy>,
) -> BTreeMap<String, CapabilityEvidence> {
    spec.attested_lease_guarantees()
        .into_iter()
        .filter_map(|name| {
            let basis = if name == LEASE_NETWORK_POLICY {
                spec.egress_evidence_basis(egress?)
            } else {
                "derived from an implemented operation kind of the signed provider spec".to_owned()
            };
            Some((
                name.to_owned(),
                CapabilityEvidence {
                    source: ProofSource::ProviderAttested,
                    provider: spec.evidence_provider().to_owned(),
                    basis,
                },
            ))
        })
        .collect()
}

/// Stage one. Runs offline, before any reservation reaches the provider, so a
/// refusal costs no contact and no spend.
pub fn admit_before_spend(
    spec: &dyn LeaseProviderFacts,
    request: &LeaseAdmissionRequest,
    request_sha256: &str,
) -> LeaseAdmissionResponse {
    let egress = spec
        .network_policy_capability()
        .egress_policy(spec.evidence_provider(), &request.guest_network_allow);
    let egress_refusal = egress.as_ref().err().map(|error| format!("{error:#}"));
    let evidence = spec_evidence(spec, egress.as_ref().ok());
    let FloorOutcome {
        met_by,
        unmet: mut refusals,
    } = evaluate(&request.requires_before_spend, &evidence, &|name| match (
        &egress_refusal,
        name == LEASE_NETWORK_POLICY,
    ) {
        (Some(refusal), true) => refusal.clone(),
        _ => unattested_reason(spec, name),
    });
    if !request.requires_from_guest.is_empty() && !evidence.contains_key(GUEST_READY_AUTHENTICATED)
    {
        refuse(
            &mut refusals,
            GUEST_READY_AUTHENTICATED,
            format!(
                "guest-reported requirements [{}] need an authenticated guest Ready; {}",
                request
                    .requires_from_guest
                    .iter()
                    .map(CapabilityRequirement::label)
                    .collect::<Vec<_>>()
                    .join(", "),
                unattested_reason(spec, GUEST_READY_AUTHENTICATED)
            ),
        );
    }
    // A lease whose egress the spec cannot create is refused whether or not
    // the workload lists `lease.network_policy`: creating it deny-all instead
    // would silently drop the controller it must reach.
    if let Some(refusal) = &egress_refusal {
        refuse(&mut refusals, LEASE_NETWORK_POLICY, refusal.clone());
    }
    let maximum_lease_seconds = spec.maximum_lease_seconds();
    if request.maximum_lifetime_seconds > maximum_lease_seconds {
        refuse(
            &mut refusals,
            LEASE_LIFETIME_BOUND,
            format!(
                "the workload asks for a {}s lease, above the provider's {}s hard lifetime",
                request.maximum_lifetime_seconds, maximum_lease_seconds
            ),
        );
    } else if request.activation_deadline_seconds >= request.maximum_lifetime_seconds {
        refuse(
            &mut refusals,
            LEASE_LIFETIME_BOUND,
            format!(
                "the {}s activation deadline leaves no lease time after Ready within the {}s \
                 lifetime",
                request.activation_deadline_seconds, request.maximum_lifetime_seconds
            ),
        );
    }
    let admitted_policy = match (egress, evidence.get(LEASE_NETWORK_POLICY)) {
        (Ok(policy), Some(policy_evidence)) => Some((policy, policy_evidence.clone())),
        (Ok(_), None) => {
            refuse(
                &mut refusals,
                LEASE_NETWORK_POLICY,
                format!(
                    "provider spec {} does not attest the lease network policy",
                    spec.evidence_provider()
                ),
            );
            None
        }
        (Err(_), _) => None,
    };
    if let (true, Some((network_policy, network_policy_evidence))) =
        (refusals.is_empty(), admitted_policy)
    {
        return LeaseAdmissionResponse::Admitted {
            schema: LEASE_SCHEMA,
            workload: request.workload.clone(),
            request_sha256: request_sha256.to_owned(),
            provider_spec_sha256: request.provider_spec_sha256.clone(),
            met_by,
            network_policy,
            network_policy_evidence,
        };
    }
    let reason = refusals
        .iter()
        .map(|(label, reason)| format!("`{}` requires `{label}`: {reason}", request.workload))
        .collect::<Vec<_>>()
        .join("; ");
    LeaseAdmissionResponse::Refused {
        schema: LEASE_SCHEMA,
        workload: request.workload.clone(),
        request_sha256: request_sha256.to_owned(),
        provider_spec_sha256: request.provider_spec_sha256.clone(),
        unmet: refusals,
        reason,
    }
}

/// Keep the first, most specific reason for each unmet entry.
fn refuse(refusals: &mut BTreeMap<String, String>, label: &str, reason: String) {
    refusals.entry(label.to_owned()).or_insert(reason);
}

fn unattested_reason(spec: &dyn LeaseProviderFacts, name: &str) -> String {
    let provider = spec.evidence_provider();
    if name != GUEST_READY_AUTHENTICATED {
        return format!("provider spec {provider} does not attest it");
    }
    if !spec.guest_activation_declared() {
        return format!("provider spec {provider} declares no one-shot guest activation");
    }
    match spec.guest_channel_profile() {
        Ok(GuestChannelProfile::UnsupportedPending) => format!(
            "provider spec {provider} declares guest_channel unsupported_pending, so no \
             authenticated guest Ready can reach the controller"
        ),
        Ok(GuestChannelProfile::ControllerRelayedFrames) => {
            format!("provider spec {provider} does not attest it")
        }
        Err(error) => format!("{error:#}"),
    }
}

struct FloorOutcome {
    met_by: BTreeMap<String, RequirementMet>,
    unmet: BTreeMap<String, String>,
}

/// Judge every entry against named evidence with Lillux `satisfies`. An
/// entry is met by the first of its names whose evidence it accepts; an
/// unmet entry carries why each of its names failed.
fn evaluate(
    entries: &[CapabilityRequirement],
    provided: &BTreeMap<String, CapabilityEvidence>,
    missing_reason: &dyn Fn(&str) -> String,
) -> FloorOutcome {
    let mut met_by = BTreeMap::new();
    let mut unmet = BTreeMap::new();
    for entry in entries {
        let mut failures = Vec::new();
        let mut met = None;
        for name in entry.names() {
            match provided.get(name) {
                Some(evidence) => match satisfies(entry, name, evidence) {
                    Ok(()) => {
                        met = Some(RequirementMet {
                            name: name.clone(),
                            source: evidence.source.clone(),
                            provider: evidence.provider.clone(),
                        });
                        break;
                    }
                    Err(mismatch) => failures.push(format!("{mismatch} ({})", evidence.basis)),
                },
                None => failures.push(format!("{name}: {}", missing_reason(name))),
            }
        }
        match met {
            Some(met) => {
                met_by.insert(entry.label(), met);
            }
            None if entry.is_set() => {
                unmet.insert(
                    entry.label(),
                    format!("none of its names is met; {}", failures.join("; ")),
                );
            }
            None => {
                unmet.insert(entry.label(), failures.join("; "));
            }
        }
    }
    FloorOutcome { met_by, unmet }
}

/// What the controller retained for one lease before its first activation
/// contact. None of it is taken from the guest.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeaseReadyExpectation {
    /// The provider occurrence id bound by the create response.
    pub(crate) lease_id: String,
    /// The channel the controller formed for this lease. Its occurrence is
    /// the lease, its allocation digest is the lease's create request, and
    /// its supervisor key is the per-lease guest key.
    pub(crate) channel: ExecutionChannelBinding,
    pub(crate) activation_request_digest: String,
    pub(crate) signed_import_sha256: String,
    pub(crate) guest_package_sha256: String,
    /// Wall-clock activation deadline. No Ready after it is accepted.
    pub(crate) ready_deadline_ms: i64,
}

impl LeaseReadyExpectation {
    fn validate(&self, spec: &dyn LeaseProviderFacts) -> Result<()> {
        ensure!(
            spec.valid_lease_id(&self.lease_id),
            "lease Ready expectation has an invalid lease id"
        );
        self.channel
            .validate()
            .context("lease Ready expectation has an invalid channel binding")?;
        ensure!(
            self.channel.occurrence_id == self.lease_id,
            "lease channel binding names a different occurrence than the lease"
        );
        for digest in [
            &self.activation_request_digest,
            &self.signed_import_sha256,
            &self.guest_package_sha256,
        ] {
            ensure!(
                canonical_digest(digest),
                "lease Ready expectation has an invalid digest"
            );
        }
        ensure!(
            self.ready_deadline_ms > self.channel.issued_at_ms
                && self.ready_deadline_ms <= self.channel.expires_at_ms,
            "lease Ready activation deadline is outside the channel lifetime"
        );
        Ok(())
    }

    /// The activation inputs the guest must report verified, in delivery
    /// order.
    fn verified_inputs(&self) -> [&str; 3] {
        [
            self.activation_request_digest.as_str(),
            self.signed_import_sha256.as_str(),
            self.guest_package_sha256.as_str(),
        ]
    }
}

/// Where one lease's authenticated guest frame chain stands.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestChain {
    pub(crate) boot_id: String,
    pub(crate) ready_frame_digest: String,
    /// Sequence the next guest frame must carry.
    pub(crate) next_sequence: u64,
    /// Digest of the last authenticated guest frame, which the next one names.
    pub(crate) previous_frame_digest: String,
}

/// The stage-two result of the one capability report a lease's guest sent,
/// retained so no later report is ever read.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmittedGuestReport {
    pub(crate) report_frame_digest: String,
    /// Keyed by entry label.
    pub(crate) met_by: BTreeMap<String, RequirementMet>,
}

/// Everything one admitted lease's later admission resumes from. Every
/// admission returns it; a later admission of the same lease must carry it
/// back, so a rebuilt channel continues the chain instead of restarting it
/// and keeps the stage-two result of the report sent before workload code.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestChannelState {
    pub(crate) chain: GuestChain,
    pub(crate) guest_report: AdmittedGuestReport,
}

impl GuestChannelState {
    fn validate(
        &self,
        channel: &ExecutionChannelBinding,
        requires_from_guest: &[CapabilityRequirement],
    ) -> Result<()> {
        let Self {
            chain:
                GuestChain {
                    boot_id,
                    ready_frame_digest,
                    next_sequence,
                    previous_frame_digest,
                },
            guest_report:
                AdmittedGuestReport {
                    report_frame_digest,
                    met_by,
                },
        } = self;
        ensure!(
            valid_boot_id(boot_id),
            "lease channel state boot id is malformed"
        );
        ensure!(
            canonical_digest(ready_frame_digest)
                && canonical_digest(previous_frame_digest)
                && canonical_digest(report_frame_digest),
            "lease channel state digest is invalid"
        );
        ensure!(
            report_frame_digest != ready_frame_digest,
            "lease channel state names its Ready as its capability report"
        );
        // Ready is frame 1 and the report frame 2, so a resumed chain
        // continues at 3 or later.
        ensure!(
            (3..=u64::from(channel.max_frames) + 1).contains(next_sequence),
            "lease channel state sequence is outside the channel bounds"
        );
        ensure!(
            met_by.len() == requires_from_guest.len(),
            "lease channel state retains a guest result for a different requirement set"
        );
        for entry in requires_from_guest {
            let label = entry.label();
            let met = met_by.get(&label).with_context(|| {
                format!("lease channel state retains no guest result for `{label}`")
            })?;
            ensure!(
                entry.names().contains(&met.name),
                "lease channel state says `{label}` was met by `{}`, which it does not name",
                plain_text(&met.name, MAX_NAME_BYTES)
            );
            let evidence = CapabilityEvidence {
                source: met.source.clone(),
                provider: met.provider.clone(),
                basis: "retained stage-two result".to_owned(),
            };
            satisfies(entry, &met.name, &evidence).map_err(|mismatch| {
                anyhow::anyhow!(
                    "lease channel state retains a guest result that does not meet `{label}`: \
                     {mismatch}"
                )
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeaseReadyRequest {
    pub(crate) schema: u32,
    pub(crate) adapter_id: String,
    /// The stage-one request exactly as it was admitted. Its guest entries
    /// are the ones stage two enforces.
    pub(crate) admission: LeaseAdmissionRequest,
    pub(crate) admission_request_sha256: String,
    pub(crate) expectation: LeaseReadyExpectation,
    /// Chain state and retained stage-two result an earlier admission of
    /// this lease returned. Absent only for the first admission, which must
    /// receive Ready and the one capability report.
    pub(crate) resume: Option<GuestChannelState>,
}

impl LeaseReadyRequest {
    pub(crate) fn validate(&self, spec: &dyn LeaseProviderFacts) -> Result<()> {
        ensure!(
            self.schema == LEASE_SCHEMA,
            "unsupported lease Ready schema"
        );
        self.admission.validate()?;
        ensure!(
            canonical_digest(&self.admission_request_sha256)
                && lillux::sha256_hex(&canonical_json(&self.admission)?)
                    == self.admission_request_sha256,
            "lease Ready request carries a different admission than the one it names"
        );
        self.expectation.validate(spec)?;
        if let Some(state) = &self.resume {
            state.validate(
                &self.expectation.channel,
                &self.admission.requires_from_guest,
            )?;
        }
        Ok(())
    }

    /// The wall-clock bound on this admission: the activation deadline while
    /// Ready and the report are awaited, the channel's expiry for a resumed
    /// admission, which receives no frame.
    pub(crate) fn receive_deadline_ms(&self) -> i64 {
        match self.resume {
            None => self.expectation.ready_deadline_ms,
            Some(_) => self.expectation.channel.expires_at_ms,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub(crate) enum LeaseReadyResponse {
    /// Ready and the guest report were authenticated and the report meets
    /// the workload's guest entries. The guest may start its workload. A
    /// resumed admission repeats the retained report result.
    Admitted {
        schema: u32,
        lease_id: String,
        admission_request_sha256: String,
        boot_id: String,
        ready_frame_digest: String,
        report_frame_digest: String,
        /// Keyed by entry label.
        met_by: BTreeMap<String, RequirementMet>,
        /// Carry this back on the next admission of this lease.
        channel: GuestChannelState,
    },
    /// The lease cannot be used. The controller terminates it and observes
    /// the provider terminal state; it never retries activation.
    Terminate {
        schema: u32,
        lease_id: String,
        reason: String,
    },
}

impl LeaseReadyResponse {
    pub(crate) fn terminate(lease_id: &str, reason: &str) -> Self {
        Self::Terminate {
            schema: LEASE_SCHEMA,
            lease_id: lease_id.to_owned(),
            reason: plain_text(reason, MAX_TERMINATE_REASON_BYTES),
        }
    }
}

/// Authenticate the guest's Ready against the retained expectation and start
/// the lease's chain from it. The guest's Ready time may lead `now_ms` by at
/// most `clock_skew_ms`; the controller's own clock alone decides whether
/// Ready arrived before the activation deadline.
pub(crate) fn verify_ready(
    expectation: &LeaseReadyExpectation,
    bytes: &[u8],
    now_ms: i64,
    clock_skew_ms: i64,
) -> Result<GuestChain> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_GUEST_FRAME_BYTES,
        "guest Ready exceeds its byte bound"
    );
    let ready = SignedExecutionFrame::decode_and_verify(bytes, &expectation.channel, now_ms)
        .with_context(|| {
            format!(
                "guest Ready for lease {} is not an authenticated frame of its channel",
                expectation.lease_id
            )
        })?;
    let ExecutionChannelPayload::Ready {
        supervisor_runtime_hash: _,
        base_snapshot_hash: _,
        boot: Some(boot),
    } = &ready.frame().payload
    else {
        bail!(
            "the first guest frame of lease {} is not a Ready naming the guest boot",
            expectation.lease_id
        );
    };
    let SupervisorReadyBoot {
        boot_id,
        verified_input_digests,
        ready_at_ms,
    } = boot;
    ensure!(
        verified_input_digests
            .iter()
            .map(String::as_str)
            .eq(expectation.verified_inputs()),
        "guest Ready verified different activation inputs than the controller delivered"
    );
    ensure!(
        now_ms <= expectation.ready_deadline_ms,
        "guest Ready for lease {} arrived after the activation deadline",
        expectation.lease_id
    );
    let latest_ready_at = now_ms
        .checked_add(clock_skew_ms)
        .context("guest Ready clock bound overflows")?;
    ensure!(
        *ready_at_ms <= latest_ready_at,
        "guest Ready for lease {} claims a ready time {} ms later than the controller's clock, \
         beyond the declared {clock_skew_ms} ms skew",
        expectation.lease_id,
        ready_at_ms.saturating_sub(now_ms)
    );
    Ok(GuestChain {
        boot_id: boot_id.clone(),
        ready_frame_digest: ready.digest().to_owned(),
        next_sequence: 2,
        previous_frame_digest: ready.digest().to_owned(),
    })
}

/// Authenticate the next guest frame of one lease and advance its chain.
/// Returns the frame's payload and digest. Once `report_admitted`, workload
/// code may have run under the guest key, so a further capability report is
/// refused rather than read.
fn authenticate_next(
    expectation: &LeaseReadyExpectation,
    state: &mut GuestChain,
    report_admitted: bool,
    bytes: &[u8],
    now_ms: i64,
) -> Result<(ExecutionChannelPayload, String)> {
    let lease_id = &expectation.lease_id;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_GUEST_FRAME_BYTES,
        "guest frame exceeds its byte bound"
    );
    let frame = SignedExecutionFrame::decode_and_verify(bytes, &expectation.channel, now_ms)
        .with_context(|| {
            format!("guest frame for lease {lease_id} is not an authenticated frame of its channel")
        })?;
    let header = frame.frame();
    ensure!(
        frame.digest() != state.ready_frame_digest,
        "guest frame for lease {lease_id} replays its Ready"
    );
    ensure!(
        !matches!(header.payload, ExecutionChannelPayload::Ready { .. }),
        "a second guest Ready for lease {lease_id} differs from the retained Ready: the guest \
         rebooted or was activated again"
    );
    ensure!(
        header.direction == ChannelDirection::SupervisorToOwner,
        "frame for lease {lease_id} is not a guest observation"
    );
    ensure!(
        !(report_admitted
            && matches!(
                header.payload,
                ExecutionChannelPayload::CapabilityReport { .. }
            )),
        "guest of lease {lease_id} sent a second capability report; only the report sent \
         before any workload code ran is trusted"
    );
    ensure!(
        header.sequence == state.next_sequence,
        "guest frame sequence {} for lease {lease_id} is replayed or out of order; expected {}",
        header.sequence,
        state.next_sequence
    );
    ensure!(
        header.previous_frame_digest.as_deref() == Some(state.previous_frame_digest.as_str()),
        "guest frame for lease {lease_id} does not continue its authenticated chain"
    );
    state.next_sequence = state
        .next_sequence
        .checked_add(1)
        .context("guest frame sequence is exhausted")?;
    state.previous_frame_digest = frame.digest().to_owned();
    Ok((header.payload.clone(), frame.digest().to_owned()))
}

/// Stage two. Every guest entry must be met by the authenticated report from
/// the boot that sent Ready; otherwise the lease is terminated with the
/// guest's own reasons.
pub(crate) fn admit_guest_report(
    lease_id: &str,
    boot_id: &str,
    payload: ExecutionChannelPayload,
    requires_from_guest: &[CapabilityRequirement],
) -> Result<BTreeMap<String, RequirementMet>> {
    let ExecutionChannelPayload::CapabilityReport {
        boot_id: reported_boot,
        provided,
        missing,
    } = payload
    else {
        bail!("guest of lease {lease_id} sent another frame before its capability report");
    };
    ensure!(
        reported_boot == boot_id,
        "guest boot id changed after Ready for lease {lease_id}: the guest rebooted"
    );
    let FloorOutcome { met_by, unmet } =
        evaluate(
            requires_from_guest,
            &provided,
            &|name| match missing.get(name) {
                Some(reason) => format!("guest reports it missing: {reason}"),
                None => "guest did not report it".to_owned(),
            },
        );
    ensure!(
        unmet.is_empty(),
        "{}",
        unmet
            .iter()
            .map(|(label, reason)| format!("requires `{label}`: {reason}"))
            .collect::<Vec<_>>()
            .join("; ")
    );
    Ok(met_by)
}

/// The seam a controller-to-guest transport plugs into. A transport moves
/// whole opaque frames for exactly one lease and never interprets them: Ready
/// and every later frame are authenticated against the lease's channel
/// binding, so a transport adds no authority of its own.
///
/// Controller-to-guest frames (workload requests, and the start signal after
/// the guest report is admitted) never cross this seam: they are signed by the
/// channel's owner key, which never enters the lifecycle adapter process, and
/// the controller sends them itself.
pub(crate) trait GuestFrameTransport {
    /// The next whole frame from the guest, at most `maximum_bytes`, or
    /// `None` when the deadline passes first.
    fn receive(
        &mut self,
        maximum_bytes: usize,
        deadline: MonotonicDeadline,
    ) -> Result<Option<Vec<u8>>>;
}

/// Guest frames the controller relays on the inherited guest-frames
/// descriptor: a connected byte stream carrying one length-prefixed message
/// (4-byte big-endian length, then the frame) per guest frame. Each receive
/// reads exactly one message, so nothing after the frames admission asks for
/// is ever consumed.
pub(crate) struct RelayedFrameTransport {
    channel: lillux::InheritedDuplexChannel,
}

impl RelayedFrameTransport {
    pub(crate) fn new(channel: lillux::InheritedDuplexChannel) -> Self {
        Self { channel }
    }
}

impl GuestFrameTransport for RelayedFrameTransport {
    fn receive(
        &mut self,
        maximum_bytes: usize,
        deadline: MonotonicDeadline,
    ) -> Result<Option<Vec<u8>>> {
        let bound = maximum_bytes.min(MAX_LIFECYCLE_GUEST_FRAME_MESSAGE_BYTES);
        let mut stream = self.channel.with_deadline(deadline);
        match length_prefixed::read_message(&mut stream, bound) {
            Ok(Some(frame)) => Ok(Some(frame)),
            Ok(None) => bail!(
                "the controller closed the guest-frames descriptor before the next guest frame"
            ),
            Err(error) if deadline_elapsed(&error) => Ok(None),
            Err(error) => {
                Err(error.context("the guest-frames descriptor carried no well-formed frame"))
            }
        }
    }
}

/// Whether a relayed read failed only because the receive deadline passed.
fn deadline_elapsed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == ErrorKind::TimedOut)
    })
}

/// Select the transport the signed spec names. `guest_frames` is the
/// inherited guest-frames descriptor, when the controller handed one over;
/// a transport that does not use it drops it unread.
pub(crate) fn open_guest_channel(
    spec: &dyn LeaseProviderFacts,
    lease_id: &str,
    guest_frames: Option<lillux::InheritedDuplexChannel>,
) -> Result<Box<dyn GuestFrameTransport>> {
    match spec.guest_channel_profile()? {
        GuestChannelProfile::UnsupportedPending => bail!(
            "guest channel for lease {lease_id} is not implemented: provider spec {} declares \
             guest_channel unsupported_pending, so no guest Ready or capability report can be \
             received",
            spec.evidence_provider()
        ),
        GuestChannelProfile::ControllerRelayedFrames => {
            let channel = guest_frames.with_context(|| {
                format!(
                    "the controller handed lease {lease_id} no guest-frames descriptor, so no \
                     guest Ready or capability report can be received"
                )
            })?;
            Ok(Box::new(RelayedFrameTransport::new(channel)))
        }
    }
}

/// The transport of a resumed admission. Its stage-two result was fixed by
/// the report sent before workload code, so it reads no guest frame and holds
/// no guest-frames descriptor; any receive is refused.
struct ResumedChain;

impl GuestFrameTransport for ResumedChain {
    fn receive(
        &mut self,
        _maximum_bytes: usize,
        _deadline: MonotonicDeadline,
    ) -> Result<Option<Vec<u8>>> {
        bail!("a resumed lease admission reads no guest frame")
    }
}

/// Receive Ready and the stage-two report for one activated lease, or repeat
/// the retained result of a resumed one. Every failure to admit the guest is
/// a `Terminate` verdict, never an admitted weaker guest. A resumed request
/// that arrives with the guest-frames descriptor is a malformed controller
/// request: it is refused as an error, which yields no verdict and leaves the
/// lease as it was.
pub(crate) fn await_ready(
    spec: &dyn LeaseProviderFacts,
    request: &LeaseReadyRequest,
    deadline: MonotonicDeadline,
    guest_frames: Option<lillux::InheritedDuplexChannel>,
) -> Result<LeaseReadyResponse> {
    let lease_id = &request.expectation.lease_id;
    ensure!(
        request.resume.is_none() || guest_frames.is_none(),
        "lease Ready for lease {lease_id} resumes an admitted chain and reads no guest frame, \
         but the controller handed it the guest-frames descriptor \
         {LIFECYCLE_GUEST_FRAMES_FD_ENV}, which only a first admission may hold"
    );
    if let LeaseAdmissionResponse::Refused { reason, .. } =
        admit_before_spend(spec, &request.admission, &request.admission_request_sha256)
    {
        return Ok(LeaseReadyResponse::terminate(
            lease_id,
            &format!("the lease request was not admitted before spend: {reason}"),
        ));
    }
    let now_ms = lillux::time::timestamp_millis;
    let outcome = match &request.resume {
        Some(_) => receive_ready_and_report(&mut ResumedChain, request, deadline, now_ms),
        None => open_guest_channel(spec, lease_id, guest_frames).and_then(|mut transport| {
            receive_ready_and_report(&mut *transport, request, deadline, now_ms)
        }),
    };
    Ok(match outcome {
        Ok(response) => response,
        Err(error) => LeaseReadyResponse::terminate(lease_id, &format!("{error:#}")),
    })
}

fn receive_ready_and_report(
    transport: &mut dyn GuestFrameTransport,
    request: &LeaseReadyRequest,
    deadline: MonotonicDeadline,
    now_ms: impl Fn() -> i64,
) -> Result<LeaseReadyResponse> {
    let expectation = &request.expectation;
    let lease_id = &expectation.lease_id;
    // A resumed admission reads no frame: its stage-two result was fixed by
    // the report sent before workload code, and `validate` checked it still
    // meets the admitted guest entries.
    let state = match &request.resume {
        Some(state) => state.clone(),
        None => {
            let ready_bytes = transport
                .receive(MAX_GUEST_FRAME_BYTES, deadline)?
                .with_context(|| {
                    format!("no guest Ready for lease {lease_id} before the deadline")
                })?;
            let mut chain = verify_ready(
                expectation,
                &ready_bytes,
                now_ms(),
                request.admission.ready_clock_skew_ms(),
            )?;
            let report_bytes = transport
                .receive(MAX_GUEST_FRAME_BYTES, deadline)?
                .with_context(|| {
                    format!("no guest capability report for lease {lease_id} before the deadline")
                })?;
            let (payload, report_frame_digest) =
                authenticate_next(expectation, &mut chain, false, &report_bytes, now_ms())?;
            let met_by = admit_guest_report(
                lease_id,
                &chain.boot_id,
                payload,
                &request.admission.requires_from_guest,
            )?;
            GuestChannelState {
                chain,
                guest_report: AdmittedGuestReport {
                    report_frame_digest,
                    met_by,
                },
            }
        }
    };
    Ok(LeaseReadyResponse::Admitted {
        schema: LEASE_SCHEMA,
        lease_id: lease_id.clone(),
        admission_request_sha256: request.admission_request_sha256.clone(),
        boot_id: state.chain.boot_id.clone(),
        ready_frame_digest: state.chain.ready_frame_digest.clone(),
        report_frame_digest: state.guest_report.report_frame_digest.clone(),
        met_by: state.guest_report.met_by.clone(),
        channel: state,
    })
}

fn validate_requirements(entries: &[CapabilityRequirement]) -> Result<()> {
    ensure!(
        entries.len() <= MAX_REQUIREMENTS,
        "lease requirements exceed their bound"
    );
    let mut labels = BTreeSet::new();
    for entry in entries {
        if let Some(refusal) = entry.spec_refusal() {
            bail!("lease requirement is malformed: {refusal}");
        }
        ensure!(
            entry.names().len() <= MAX_REQUIREMENTS
                && entry.names().iter().all(|name| valid_name(name))
                && entry.providers().len() <= MAX_REQUIREMENTS
                && entry
                    .providers()
                    .iter()
                    .all(|provider| valid_name(provider)),
            "lease requirement {} names an invalid capability or provider",
            plain_text(&entry.label(), MAX_NAME_BYTES)
        );
        ensure!(
            labels.insert(entry.label()),
            "lease requirement {} is listed twice",
            entry.label()
        );
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._:-".contains(&byte)
        })
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

fn canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Reasons can quote guest-supplied names; keep them printable and bounded.
fn plain_text(value: &str, maximum_bytes: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii() && !character.is_ascii_control() {
                character
            } else {
                '?'
            }
        })
        .take(maximum_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use lillux::crypto::SigningKey;
    use lillux::process_control::CapabilityRequirementSpec;
    use ryeos_state::external_execution::{
        EXECUTION_CHANNEL_BINDING_SCHEMA, ExecutionFrame, ExternalExecutionMode,
    };

    use super::*;
    use crate::egress::{
        AllowListCapability, EgressHostSyntax, EgressProtocol, NetworkPolicyCapability,
        NetworkPolicyType,
    };
    use crate::facts::{LEASE_CREATE_BOUND, LEASE_TERMINATE_OBSERVED};

    const BOOT_ID: &str = "997aad14-f3e6-4990-9d40-d1068e5a38d5";
    const OTHER_BOOT_ID: &str = "00000000-0000-4000-8000-000000000000";
    const SKEW: i64 = DEFAULT_READY_CLOCK_SKEW_MS as i64;

    /// Provider facts for a test provider: a 24 h lease lifetime, exact
    /// HTTPS allow-lists of at most 4 hosts, lease ids under one prefix, and a
    /// guest channel that is either relayed through the controller or not yet
    /// implemented.
    struct TestFacts {
        guest_channel: GuestChannelProfile,
        lease_id_prefix: &'static str,
        network_policy: NetworkPolicyCapability,
    }

    impl LeaseProviderFacts for TestFacts {
        fn evidence_provider(&self) -> &str {
            "test_provider_v1"
        }

        fn maximum_lease_seconds(&self) -> u32 {
            24 * 60 * 60
        }

        fn attested_lease_guarantees(&self) -> BTreeSet<&'static str> {
            let mut guarantees = BTreeSet::from([
                LEASE_CREATE_BOUND,
                LEASE_LIFETIME_BOUND,
                LEASE_NETWORK_POLICY,
                LEASE_TERMINATE_OBSERVED,
            ]);
            if self.guest_channel == GuestChannelProfile::ControllerRelayedFrames {
                guarantees.insert(GUEST_READY_AUTHENTICATED);
            }
            guarantees
        }

        fn guest_channel_profile(&self) -> Result<GuestChannelProfile> {
            Ok(self.guest_channel)
        }

        fn guest_activation_declared(&self) -> bool {
            true
        }

        fn network_policy_capability(&self) -> &NetworkPolicyCapability {
            &self.network_policy
        }

        fn egress_evidence_basis(&self, policy: &EgressPolicy) -> String {
            format!("test provider creates exactly [{}]", policy.rules_text())
        }

        fn valid_lease_id(&self, id: &str) -> bool {
            id.len() > self.lease_id_prefix.len() && id.starts_with(self.lease_id_prefix)
        }
    }

    fn test_facts(guest_channel: GuestChannelProfile) -> TestFacts {
        TestFacts {
            guest_channel,
            lease_id_prefix: "lease-",
            network_policy: NetworkPolicyCapability {
                creatable_types: vec![NetworkPolicyType::DenyAll, NetworkPolicyType::AllowList],
                allow_list: Some(AllowListCapability {
                    rule_protocol: EgressProtocol::Https,
                    max_rules: 4,
                    host_syntax: EgressHostSyntax::ExactDnsName,
                }),
            },
        }
    }

    /// A provider whose spec relays guest frames through the controller.
    fn relayed_facts() -> TestFacts {
        test_facts(GuestChannelProfile::ControllerRelayedFrames)
    }

    /// A provider whose spec declares its guest channel absent.
    fn pending_channel_facts() -> TestFacts {
        test_facts(GuestChannelProfile::UnsupportedPending)
    }

    fn admission(
        requires: Vec<CapabilityRequirement>,
        from_guest: Vec<CapabilityRequirement>,
    ) -> LeaseAdmissionRequest {
        LeaseAdmissionRequest {
            schema: LEASE_SCHEMA,
            adapter_id: "remote-lease-test".into(),
            provider_spec_sha256: "a".repeat(64),
            workload: "workload.remote".into(),
            requires_before_spend: requires,
            requires_from_guest: from_guest,
            guest_network_allow: BTreeSet::new(),
            maximum_lifetime_seconds: 4 * 60 * 60,
            activation_deadline_seconds: 5 * 60,
            ready_clock_skew_ms: None,
        }
    }

    #[test]
    fn stage_one_rejects_malformed_requests() {
        let mut request = admission(vec!["Lease.Create".into()], Vec::new());
        assert!(request.validate().is_err());
        for malformed in [
            "https://example.com",
            "example.com:443",
            "example.com/path",
            "Example.com",
            "example.com.",
            "localhost",
            "192.168.0.1",
            "*.com",
            "api.*.example.com",
        ] {
            request = admission(Vec::new(), Vec::new());
            request.guest_network_allow = BTreeSet::from([malformed.to_owned()]);
            assert!(
                request.validate().is_err(),
                "{malformed:?} must be malformed"
            );
        }
        request = admission(Vec::new(), Vec::new());
        request.guest_network_allow = (0..=MAX_EGRESS_RULES)
            .map(|index| format!("host{index}.example.com"))
            .collect();
        assert!(request.validate().is_err());
        request = admission(Vec::new(), Vec::new());
        request.activation_deadline_seconds = 0;
        assert!(request.validate().is_err());
        request = admission(Vec::new(), Vec::new());
        request.ready_clock_skew_ms = Some(MAX_READY_CLOCK_SKEW_MS);
        request.validate().unwrap();
        request.ready_clock_skew_ms = Some(MAX_READY_CLOCK_SKEW_MS + 1);
        let error = request.validate().err().unwrap();
        assert!(format!("{error:#}").contains("Ready clock skew above"));
        // An undeclared skew is absent from the canonical request rather
        // than serialized as null, so an omitted field stays canonical.
        request = admission(Vec::new(), Vec::new());
        let text = String::from_utf8(canonical_json(&request).unwrap()).unwrap();
        assert!(!text.contains("ready_clock_skew_ms"), "{text}");
        let peer = CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: Some("isolation:namespaces".into()),
            any_of: Vec::new(),
            accept: vec!["peer_attested".into()],
            providers: Vec::new(),
        });
        assert!(admission(Vec::new(), vec![peer]).validate().is_err());
        let twice = admission(
            vec![LEASE_CREATE_BOUND.into(), LEASE_CREATE_BOUND.into()],
            Vec::new(),
        );
        assert!(twice.validate().is_err());
    }

    struct Lease {
        owner_key: SigningKey,
        guest_key: SigningKey,
        expectation: LeaseReadyExpectation,
    }

    fn lease() -> Lease {
        let owner_key = SigningKey::from_bytes(&[5; 32]);
        let guest_key = SigningKey::from_bytes(&[7; 32]);
        let channel = ExecutionChannelBinding {
            schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: ExternalExecutionMode::StructuredSession {},
            placement_thread_id: "T-lease".into(),
            allocation_request_digest: "1".repeat(64),
            occurrence_id: "lease-1".into(),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            candidate_program_digest: "0".repeat(64),
            channel_nonce: "f".repeat(64),
            owner_public_key: STANDARD.encode(owner_key.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(guest_key.verifying_key().as_bytes()),
            issued_at_ms: 100,
            execution_deadline_ms: 10_000,
            expires_at_ms: 20_000,
            candidate_export_max_bytes: 512 * 1024,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        };
        let expectation = LeaseReadyExpectation {
            lease_id: "lease-1".into(),
            channel,
            activation_request_digest: "2".repeat(64),
            signed_import_sha256: "3".repeat(64),
            guest_package_sha256: "4".repeat(64),
            ready_deadline_ms: 2_000,
        };
        Lease {
            owner_key,
            guest_key,
            expectation,
        }
    }

    fn ready_boot(lease: &Lease) -> SupervisorReadyBoot {
        SupervisorReadyBoot {
            boot_id: BOOT_ID.into(),
            verified_input_digests: lease
                .expectation
                .verified_inputs()
                .iter()
                .map(|digest| (*digest).to_owned())
                .collect(),
            ready_at_ms: 1_000,
        }
    }

    fn signed_frame(
        lease: &Lease,
        key: &SigningKey,
        direction: ChannelDirection,
        sequence: u64,
        previous_frame_digest: Option<String>,
        payload: ExecutionChannelPayload,
    ) -> Vec<u8> {
        let channel = &lease.expectation.channel;
        let frame = ExecutionFrame {
            schema: 1,
            binding_digest: channel.digest().unwrap(),
            direction,
            sequence,
            previous_frame_digest,
            acknowledged_peer_sequence: 0,
            payload,
        };
        let signed = SignedExecutionFrame::sign(frame, channel, key).unwrap();
        lillux::canonical_json(&serde_json::to_value(&signed).unwrap())
            .unwrap()
            .into_bytes()
    }

    fn ready_payload(lease: &Lease, boot: Option<SupervisorReadyBoot>) -> ExecutionChannelPayload {
        ExecutionChannelPayload::Ready {
            supervisor_runtime_hash: lease.expectation.channel.supervisor_runtime_hash.clone(),
            base_snapshot_hash: lease.expectation.channel.base_snapshot_hash.clone(),
            boot,
        }
    }

    fn signed_ready(lease: &Lease, boot: SupervisorReadyBoot) -> Vec<u8> {
        signed_frame(
            lease,
            &lease.guest_key,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            ready_payload(lease, Some(boot)),
        )
    }

    fn report(
        boot_id: &str,
        provided: &[(&str, ProofSource)],
        missing: &[(&str, &str)],
    ) -> ExecutionChannelPayload {
        ExecutionChannelPayload::CapabilityReport {
            boot_id: boot_id.into(),
            provided: provided
                .iter()
                .map(|(name, source)| {
                    (
                        (*name).to_owned(),
                        CapabilityEvidence {
                            source: source.clone(),
                            provider: "host".into(),
                            basis: "guest probe".into(),
                        },
                    )
                })
                .collect(),
            missing: missing
                .iter()
                .map(|(name, reason)| ((*name).to_owned(), (*reason).to_owned()))
                .collect(),
        }
    }

    fn signed_report(
        lease: &Lease,
        sequence: u64,
        previous: &str,
        payload: ExecutionChannelPayload,
    ) -> Vec<u8> {
        signed_frame(
            lease,
            &lease.guest_key,
            ChannelDirection::SupervisorToOwner,
            sequence,
            Some(previous.to_owned()),
            payload,
        )
    }

    #[test]
    fn ready_binds_lease_activation_inputs_boot_and_deadline() {
        let lease = lease();
        lease.expectation.validate(&relayed_facts()).unwrap();
        let bytes = signed_ready(&lease, ready_boot(&lease));
        let state = verify_ready(&lease.expectation, &bytes, 1_500, SKEW).unwrap();
        assert_eq!(state.boot_id, BOOT_ID);
        assert_eq!(state.ready_frame_digest, lillux::sha256_hex(&bytes));
        assert_eq!(state.next_sequence, 2);
        assert_eq!(state.previous_frame_digest, state.ready_frame_digest);

        // Signed by the owner key, not the per-lease guest key.
        let forged = signed_frame(
            &lease,
            &lease.owner_key,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            ExecutionChannelPayload::Release,
        );
        assert!(verify_ready(&lease.expectation, &forged, 1_500, SKEW).is_err());
        // After the activation deadline.
        assert!(verify_ready(&lease.expectation, &bytes, 2_001, SKEW).is_err());

        // A guest clock slightly ahead of the controller's is drift within
        // the declared skew, even past the activation deadline, while the
        // controller's own clock is still before it.
        let mut issued_late = ready_boot(&lease);
        issued_late.ready_at_ms = 1_600;
        let issued_late = signed_ready(&lease, issued_late);
        verify_ready(&lease.expectation, &issued_late, 1_500, SKEW).unwrap();
        let error = verify_ready(&lease.expectation, &issued_late, 1_500, 0)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("100 ms later than the controller's clock"));
        let mut skewed_past_deadline = ready_boot(&lease);
        skewed_past_deadline.ready_at_ms = 2_500;
        let skewed_past_deadline = signed_ready(&lease, skewed_past_deadline);
        verify_ready(&lease.expectation, &skewed_past_deadline, 1_900, SKEW).unwrap();
        // Beyond the skew the ready time is refused.
        let mut beyond_skew = ready_boot(&lease);
        beyond_skew.ready_at_ms = 1_500 + SKEW + 1;
        let beyond_skew = signed_ready(&lease, beyond_skew);
        let error = verify_ready(&lease.expectation, &beyond_skew, 1_500, SKEW)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("beyond the declared 5000 ms skew"));

        let mut other_inputs = ready_boot(&lease);
        other_inputs.verified_input_digests[2] = "5".repeat(64);
        let other_inputs = signed_ready(&lease, other_inputs);
        let error = verify_ready(&lease.expectation, &other_inputs, 1_500, SKEW)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("different activation inputs"));

        let no_boot = signed_frame(
            &lease,
            &lease.guest_key,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            ready_payload(&lease, None),
        );
        let error = verify_ready(&lease.expectation, &no_boot, 1_500, SKEW)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("not a Ready naming the guest boot"));

        // Another lease's channel cannot authenticate this Ready.
        let mut other_lease = lease.expectation.clone();
        other_lease.lease_id = "lease-2".into();
        other_lease.channel.occurrence_id = "lease-2".into();
        assert!(verify_ready(&other_lease, &bytes, 1_500, SKEW).is_err());

        let mut noncanonical = bytes.clone();
        noncanonical.push(b' ');
        assert!(verify_ready(&lease.expectation, &noncanonical, 1_500, SKEW).is_err());
    }

    #[test]
    fn guest_frames_continue_the_chain_from_ready() {
        let lease = lease();
        let ready = signed_ready(&lease, ready_boot(&lease));
        let mut state = verify_ready(&lease.expectation, &ready, 1_500, SKEW).unwrap();
        let ready_digest = state.ready_frame_digest.clone();

        let first = signed_report(&lease, 2, &ready_digest, report(BOOT_ID, &[], &[]));
        let (_, first_digest) =
            authenticate_next(&lease.expectation, &mut state, false, &first, 1_500).unwrap();
        assert_eq!(state.next_sequence, 3);
        assert_eq!(state.previous_frame_digest, first_digest);
        let error = authenticate_next(&lease.expectation, &mut state, false, &first, 1_500)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("replayed or out of order"));
        let error = authenticate_next(&lease.expectation, &mut state, false, &ready, 1_500)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("replays its Ready"));

        let mut rebooted = ready_boot(&lease);
        rebooted.boot_id = OTHER_BOOT_ID.into();
        let rebooted = signed_ready(&lease, rebooted);
        let error = authenticate_next(&lease.expectation, &mut state, false, &rebooted, 1_500)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("rebooted or was activated again"));

        let off_chain = signed_report(&lease, 3, &"6".repeat(64), report(BOOT_ID, &[], &[]));
        let error = authenticate_next(&lease.expectation, &mut state, false, &off_chain, 1_500)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("does not continue its authenticated chain"));
        assert_eq!(state.next_sequence, 3, "a refused frame does not advance");

        // After the report is admitted, workload code may hold the guest
        // key, so a further report on the chain is refused, not read.
        let second = signed_report(&lease, 3, &first_digest, report(BOOT_ID, &[], &[]));
        let error = authenticate_next(&lease.expectation, &mut state, true, &second, 1_500)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("sent a second capability report"));
        assert_eq!(state.next_sequence, 3, "a refused report does not advance");
    }

    #[test]
    fn stage_two_judges_the_report_with_the_guest_reasons() {
        let required: Vec<CapabilityRequirement> = vec![
            "guest.exec_route_closed".into(),
            CapabilityRequirement::Spec(CapabilityRequirementSpec {
                name: Some("network:egress_denied".into()),
                any_of: Vec::new(),
                accept: vec!["provider_attested".into()],
                providers: Vec::new(),
            }),
        ];
        let met_by = admit_guest_report(
            "lease-1",
            BOOT_ID,
            report(
                BOOT_ID,
                &[
                    ("guest.exec_route_closed", ProofSource::Measured),
                    ("network:egress_denied", ProofSource::ProviderAttested),
                ],
                &[],
            ),
            &required,
        )
        .unwrap();
        assert_eq!(
            met_by["network:egress_denied"].source,
            ProofSource::ProviderAttested
        );

        let reason = format!(
            "{:#}",
            admit_guest_report(
                "lease-1",
                BOOT_ID,
                report(
                    BOOT_ID,
                    &[("guest.exec_route_closed", ProofSource::Trusted)],
                    &[(
                        "persistence:durable_state",
                        "no persistent disk; 24 h hard lifetime"
                    )],
                ),
                &[
                    "guest.exec_route_closed".into(),
                    "persistence:durable_state".into(),
                    "isolation:namespaces".into(),
                ],
            )
            .err()
            .unwrap()
        );
        assert!(reason.contains("only trusted-proven"));
        assert!(reason.contains("no persistent disk; 24 h hard lifetime"));
        assert!(reason.contains("isolation:namespaces: guest did not report it"));

        let error = admit_guest_report("lease-1", BOOT_ID, report(OTHER_BOOT_ID, &[], &[]), &[])
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("the guest rebooted"));
    }

    struct ScriptedTransport(VecDeque<Vec<u8>>);

    impl GuestFrameTransport for ScriptedTransport {
        fn receive(
            &mut self,
            maximum_bytes: usize,
            _deadline: MonotonicDeadline,
        ) -> Result<Option<Vec<u8>>> {
            let frame = self.0.pop_front();
            ensure!(
                frame
                    .as_ref()
                    .is_none_or(|bytes| bytes.len() <= maximum_bytes),
                "scripted frame exceeds the requested bound"
            );
            Ok(frame)
        }
    }

    fn ready_request(lease: &Lease, from_guest: Vec<CapabilityRequirement>) -> LeaseReadyRequest {
        let admission = admission(Vec::new(), from_guest);
        let admission_request_sha256 = lillux::sha256_hex(&canonical_json(&admission).unwrap());
        LeaseReadyRequest {
            schema: LEASE_SCHEMA,
            adapter_id: "remote-lease-test".into(),
            admission,
            admission_request_sha256,
            expectation: lease.expectation.clone(),
            resume: None,
        }
    }

    #[test]
    fn ready_then_report_admits_and_a_later_admission_resumes_the_chain() {
        let lease = lease();
        let mut request = ready_request(&lease, vec!["guest.exec_route_closed".into()]);
        request.validate(&relayed_facts()).unwrap();
        let ready = signed_ready(&lease, ready_boot(&lease));
        let ready_digest = lillux::sha256_hex(&ready);
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(1));
        let covering = report(
            BOOT_ID,
            &[("guest.exec_route_closed", ProofSource::Measured)],
            &[],
        );

        let first_report = signed_report(&lease, 2, &ready_digest, covering.clone());
        let mut transport =
            ScriptedTransport(VecDeque::from([ready.clone(), first_report.clone()]));
        let LeaseReadyResponse::Admitted {
            boot_id,
            ready_frame_digest,
            report_frame_digest,
            channel,
            ..
        } = receive_ready_and_report(&mut transport, &request, deadline, || 1_500).unwrap()
        else {
            panic!("an authenticated covering report admits the lease");
        };
        assert_eq!(boot_id, BOOT_ID);
        assert_eq!(ready_frame_digest, ready_digest);
        assert_eq!(report_frame_digest, lillux::sha256_hex(&first_report));
        assert_eq!(channel.chain.next_sequence, 3);
        assert_eq!(channel.chain.previous_frame_digest, report_frame_digest);
        assert_eq!(
            channel.guest_report.report_frame_digest,
            report_frame_digest
        );
        assert_eq!(
            channel.guest_report.met_by["guest.exec_route_closed"].source,
            ProofSource::Measured
        );

        // A later admission carries the returned state back after the
        // activation deadline. It reads no frame: a weakened report queued
        // by a guest whose key workload code may now hold is never read, and
        // the retained result is repeated unchanged.
        request.resume = Some(channel.clone());
        request.validate(&relayed_facts()).unwrap();
        assert_eq!(
            request.receive_deadline_ms(),
            lease.expectation.channel.expires_at_ms
        );
        let weakened = signed_report(
            &lease,
            3,
            &report_frame_digest,
            report(
                BOOT_ID,
                &[],
                &[("guest.exec_route_closed", "shell route is open")],
            ),
        );
        let mut transport = ScriptedTransport(VecDeque::from([weakened.clone()]));
        let LeaseReadyResponse::Admitted {
            channel: resumed,
            met_by,
            report_frame_digest: resumed_report,
            ..
        } = receive_ready_and_report(&mut transport, &request, deadline, || 5_000).unwrap()
        else {
            panic!("a resumed admission repeats the retained stage-two result");
        };
        assert_eq!(resumed, channel);
        assert_eq!(met_by, channel.guest_report.met_by);
        assert_eq!(resumed_report, report_frame_digest);
        assert_eq!(transport.0.len(), 1, "a resumed admission reads no frame");
        // The same weakened report arriving on the resumed chain ends the
        // lease instead of being judged.
        let mut chain = resumed.chain.clone();
        let error = authenticate_next(&lease.expectation, &mut chain, true, &weakened, 5_000)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("sent a second capability report"));

        // Resume state is controller data, but it must still be the result of
        // this exact guest requirement set.
        let mut tampered = channel.clone();
        tampered.guest_report.met_by.clear();
        request.resume = Some(tampered);
        let error = request.validate(&relayed_facts()).err().unwrap();
        assert!(format!("{error:#}").contains("different requirement set"));
        let mut tampered = channel.clone();
        if let Some(met) = tampered
            .guest_report
            .met_by
            .get_mut("guest.exec_route_closed")
        {
            met.source = ProofSource::Trusted;
        }
        request.resume = Some(tampered);
        let error = request.validate(&relayed_facts()).err().unwrap();
        assert!(format!("{error:#}").contains("does not meet `guest.exec_route_closed`"));
        let mut tampered = channel.clone();
        tampered.chain.next_sequence = 2;
        request.resume = Some(tampered);
        assert!(
            request.validate(&relayed_facts()).is_err(),
            "a resumed chain continues after its report, never at the report sequence"
        );

        request.resume = None;
        let weak_report = signed_report(
            &lease,
            2,
            &ready_digest,
            report(
                BOOT_ID,
                &[],
                &[("guest.exec_route_closed", "shell route is open")],
            ),
        );
        let mut transport = ScriptedTransport(VecDeque::from([ready.clone(), weak_report]));
        let error = receive_ready_and_report(&mut transport, &request, deadline, || 1_500)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("shell route is open"));

        let mut transport = ScriptedTransport(VecDeque::from([ready]));
        let error = receive_ready_and_report(&mut transport, &request, deadline, || 1_500)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("no guest capability report"));
    }

    #[test]
    fn ready_request_is_bound_to_the_admitted_request() {
        let lease = lease();
        let mut request = ready_request(&lease, vec!["guest.exec_route_closed".into()]);
        request.validate(&relayed_facts()).unwrap();
        // Re-supplying a weaker guest set under the admitted digest fails.
        request.admission.requires_from_guest.clear();
        let error = request.validate(&relayed_facts()).err().unwrap();
        assert!(format!("{error:#}").contains("different admission"));

        let mut request = ready_request(&lease, Vec::new());
        request.resume = Some(GuestChannelState {
            chain: GuestChain {
                boot_id: BOOT_ID.into(),
                ready_frame_digest: "7".repeat(64),
                next_sequence: 1,
                previous_frame_digest: "7".repeat(64),
            },
            guest_report: AdmittedGuestReport {
                report_frame_digest: "8".repeat(64),
                met_by: BTreeMap::new(),
            },
        });
        assert!(
            request.validate(&relayed_facts()).is_err(),
            "a resumed chain never restarts at the Ready sequence"
        );
    }

    #[test]
    fn a_lease_id_the_provider_does_not_issue_is_refused() {
        let lease = lease();
        let request = ready_request(&lease, Vec::new());
        request.validate(&relayed_facts()).unwrap();
        let mut other_ids = relayed_facts();
        other_ids.lease_id_prefix = "occ-";
        let error = request.validate(&other_ids).err().unwrap();
        assert!(
            format!("{error:#}").contains("invalid lease id"),
            "{error:#}"
        );
    }

    #[test]
    fn lease_egress_is_derived_from_the_declared_capability() {
        let facts = relayed_facts();
        let mut request = admission(vec![LEASE_NETWORK_POLICY.into()], Vec::new());
        request.guest_network_allow = BTreeSet::from(["controller.example.com".to_owned()]);
        let LeaseAdmissionResponse::Refused { unmet, .. } =
            admit_before_spend(&facts, &request, &"b".repeat(64))
        else {
            panic!("a bare network policy entry needs a measured proof");
        };
        assert!(unmet.contains_key(LEASE_NETWORK_POLICY));
        let mut deny_only = relayed_facts();
        deny_only.network_policy = NetworkPolicyCapability {
            creatable_types: vec![NetworkPolicyType::DenyAll],
            allow_list: None,
        };
        deny_only.network_policy.validate().unwrap();
        request.requires_before_spend.clear();
        let LeaseAdmissionResponse::Refused { reason, .. } =
            admit_before_spend(&deny_only, &request, &"b".repeat(64))
        else {
            panic!("a deny-all-only capability cannot create an allow-list lease");
        };
        assert!(
            reason.contains(
                "provider spec test_provider_v1 declares network_policy creatable_types \
                 [deny-all] only"
            ),
            "{reason}"
        );
        let LeaseAdmissionResponse::Admitted { network_policy, .. } =
            admit_before_spend(&facts, &request, &"b".repeat(64))
        else {
            panic!("the declared allow-list admits one exact controller host");
        };
        assert_eq!(network_policy.rules_text(), "https controller.example.com");
        assert!(
            NetworkPolicyCapability {
                creatable_types: vec![NetworkPolicyType::AllowList],
                allow_list: None,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn unadmitted_requests_terminate_and_an_absent_guest_channel_refuses() {
        let pending = pending_channel_facts();
        let lease = lease();
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(1));
        let LeaseReadyResponse::Terminate { reason, .. } = await_ready(
            &pending,
            &ready_request(&lease, vec!["guest.exec_route_closed".into()]),
            deadline,
            None,
        )
        .unwrap() else {
            panic!("stage two refuses a request stage one would refuse");
        };
        assert!(reason.contains("not admitted before spend"));

        let LeaseReadyResponse::Terminate {
            lease_id, reason, ..
        } = await_ready(&pending, &ready_request(&lease, Vec::new()), deadline, None).unwrap()
        else {
            panic!("no Ready can be received without a guest channel");
        };
        assert_eq!(lease_id, "lease-1");
        assert!(reason.contains("guest_channel unsupported_pending"));

        // A relaying spec receives guest frames, but only on a descriptor the
        // controller actually handed over.
        let LeaseReadyResponse::Terminate { reason, .. } = await_ready(
            &relayed_facts(),
            &ready_request(&lease, vec!["guest.exec_route_closed".into()]),
            deadline,
            None,
        )
        .unwrap() else {
            panic!("no Ready can be received without the guest-frames descriptor");
        };
        assert!(reason.contains("no guest-frames descriptor"), "{reason}");
    }

    #[test]
    fn a_resumed_admission_needs_no_guest_frames_descriptor_and_refuses_one() {
        let lease = lease();
        let mut request = ready_request(&lease, vec!["guest.exec_route_closed".into()]);
        let ready = signed_ready(&lease, ready_boot(&lease));
        let first_report = signed_report(
            &lease,
            2,
            &lillux::sha256_hex(&ready),
            report(
                BOOT_ID,
                &[("guest.exec_route_closed", ProofSource::Measured)],
                &[],
            ),
        );
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(5));
        let mut transport = ScriptedTransport(VecDeque::from([ready, first_report]));
        let LeaseReadyResponse::Admitted { channel, .. } =
            receive_ready_and_report(&mut transport, &request, deadline, || 1_500).unwrap()
        else {
            panic!("an authenticated covering report admits the lease");
        };
        request.resume = Some(channel.clone());
        request.validate(&relayed_facts()).unwrap();

        // A relaying spec receives guest frames, yet a resumed admission
        // reads none, so it admits without the descriptor and repeats the
        // retained result.
        let LeaseReadyResponse::Admitted {
            channel: resumed,
            met_by,
            report_frame_digest,
            ..
        } = await_ready(&relayed_facts(), &request, deadline, None).unwrap()
        else {
            panic!("a resumed admission without the descriptor repeats the retained result");
        };
        assert_eq!(resumed, channel);
        assert_eq!(met_by, channel.guest_report.met_by);
        assert_eq!(
            report_frame_digest,
            channel.guest_report.report_frame_digest
        );

        // Handing a resumed admission the descriptor is a malformed request:
        // refused by name with no verdict, so the healthy lease is not
        // terminated.
        let error = await_ready(
            &relayed_facts(),
            &request,
            deadline,
            Some(relayed_channel(&[])),
        )
        .err()
        .unwrap();
        let reason = format!("{error:#}");
        assert!(reason.contains(LIFECYCLE_GUEST_FRAMES_FD_ENV), "{reason}");
        assert!(reason.contains("resumes an admitted chain"), "{reason}");

        let error = ResumedChain
            .receive(MAX_GUEST_FRAME_BYTES, deadline)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("reads no guest frame"));
    }

    /// A guest-frames channel whose controller side has already written
    /// exactly `bytes` and closed. A relay process copies the bytes from a
    /// sealed memfd into the peer end of a connected Lillux duplex channel.
    fn relayed_channel(bytes: &[u8]) -> lillux::InheritedDuplexChannel {
        let (channel, peer) = lillux::inherited_duplex_channel_pair().unwrap();
        let source = lillux::sealed_memfd(c"guest-frames-test", bytes).unwrap();
        let mut request = lillux::SubprocessRequest {
            cmd: "/bin/sh".into(),
            argv0: None,
            args: vec!["-c".into(), "exec /bin/cat /dev/fd/4 >&3".into()],
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 10.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        peer.bind_to_subprocess_request(&mut request, "GUEST_FRAMES_PEER_FD", 3)
            .unwrap();
        source.bind_to_subprocess_request(&mut request, 4).unwrap();
        drop(peer);
        drop(source);
        let Ok(process) = lillux::spawn(request) else {
            panic!("the guest-frames relay did not start");
        };
        assert!(process.wait().success, "the guest-frames relay failed");
        channel
    }

    fn framed(frames: &[&[u8]]) -> Vec<u8> {
        frames
            .iter()
            .flat_map(|frame| {
                length_prefixed::encode(frame, MAX_LIFECYCLE_GUEST_FRAME_MESSAGE_BYTES).unwrap()
            })
            .collect()
    }

    fn relayed_outcome(
        bytes: &[u8],
        request: &LeaseReadyRequest,
    ) -> (Result<LeaseReadyResponse>, Box<dyn GuestFrameTransport>) {
        let mut transport =
            open_guest_channel(&relayed_facts(), "lease-1", Some(relayed_channel(bytes))).unwrap();
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(5));
        let outcome = receive_ready_and_report(&mut *transport, request, deadline, || 1_500);
        (outcome, transport)
    }

    #[test]
    fn relayed_ready_and_report_admit_and_nothing_after_them_is_read() {
        let lease = lease();
        let request = ready_request(&lease, vec!["guest.exec_route_closed".into()]);
        let ready = signed_ready(&lease, ready_boot(&lease));
        let ready_digest = lillux::sha256_hex(&ready);
        let covering = signed_report(
            &lease,
            2,
            &ready_digest,
            report(
                BOOT_ID,
                &[("guest.exec_route_closed", ProofSource::Measured)],
                &[],
            ),
        );
        let later = signed_frame(
            &lease,
            &lease.guest_key,
            ChannelDirection::SupervisorToOwner,
            3,
            Some(lillux::sha256_hex(&covering)),
            report(BOOT_ID, &[], &[]),
        );
        let (outcome, mut transport) =
            relayed_outcome(&framed(&[&ready, &covering, &later]), &request);
        let LeaseReadyResponse::Admitted {
            ready_frame_digest,
            report_frame_digest,
            met_by,
            ..
        } = outcome.unwrap()
        else {
            panic!("a relayed covering report admits the lease");
        };
        assert_eq!(ready_frame_digest, ready_digest);
        assert_eq!(report_frame_digest, lillux::sha256_hex(&covering));
        assert_eq!(
            met_by["guest.exec_route_closed"].source,
            ProofSource::Measured
        );

        // Admission consumed exactly two messages: the next one is still
        // whole on the descriptor, and after it the stream ends cleanly.
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(5));
        assert_eq!(
            transport
                .receive(MAX_GUEST_FRAME_BYTES, deadline)
                .unwrap()
                .unwrap(),
            later
        );
        let error = transport
            .receive(MAX_GUEST_FRAME_BYTES, deadline)
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("closed the guest-frames descriptor"),
            "{error:#}"
        );
    }

    #[test]
    fn relayed_frames_refuse_truncation_over_bound_lengths_deadline_and_a_second_ready() {
        let lease = lease();
        let request = ready_request(&lease, vec!["guest.exec_route_closed".into()]);
        let ready = signed_ready(&lease, ready_boot(&lease));
        let ready_digest = lillux::sha256_hex(&ready);
        let covering = signed_report(
            &lease,
            2,
            &ready_digest,
            report(
                BOOT_ID,
                &[("guest.exec_route_closed", ProofSource::Measured)],
                &[],
            ),
        );
        let refusal = |bytes: &[u8]| {
            let (outcome, _transport) = relayed_outcome(bytes, &request);
            format!("{:#}", outcome.err().unwrap())
        };

        // A report cut short by the end of the stream.
        let mut truncated = framed(&[&ready, &covering]);
        truncated.truncate(truncated.len() - 16);
        let reason = refusal(&truncated);
        assert!(reason.contains("no well-formed frame"), "{reason}");
        assert!(reason.contains("ended inside a message"), "{reason}");

        // A length prefix above the bound is refused from the prefix alone.
        let mut over_bound = framed(&[&ready]);
        over_bound.extend(
            u32::try_from(MAX_LIFECYCLE_GUEST_FRAME_MESSAGE_BYTES + 1)
                .unwrap()
                .to_be_bytes(),
        );
        over_bound.extend([b'x'; 16]);
        let reason = refusal(&over_bound);
        assert!(reason.contains("exceeds its 65536-byte bound"), "{reason}");

        // The stream ends on a message boundary before the report.
        let reason = refusal(&framed(&[&ready]));
        assert!(
            reason.contains("closed the guest-frames descriptor before the next guest frame"),
            "{reason}"
        );

        // A second Ready, from a rebooted or re-activated guest, in place of
        // the report; and the same Ready relayed twice.
        let mut rebooted = ready_boot(&lease);
        rebooted.boot_id = OTHER_BOOT_ID.into();
        let second_ready = signed_ready(&lease, rebooted);
        let reason = refusal(&framed(&[&ready, &second_ready]));
        assert!(reason.contains("a second guest Ready"), "{reason}");
        let reason = refusal(&framed(&[&ready, &ready]));
        assert!(reason.contains("replays its Ready"), "{reason}");

        // No frame before the receive deadline. The controller side stays
        // open and silent.
        let (channel, _silent_peer) = lillux::inherited_duplex_channel_pair().unwrap();
        let mut transport = open_guest_channel(&relayed_facts(), "lease-1", Some(channel)).unwrap();
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_millis(50));
        let error = receive_ready_and_report(&mut *transport, &request, deadline, || 1_500)
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("no guest Ready for lease lease-1 before the deadline"),
            "{error:#}"
        );
    }

    #[test]
    fn terminate_reasons_are_printable_and_bounded() {
        let LeaseReadyResponse::Terminate { reason, .. } = LeaseReadyResponse::terminate(
            "lease-1",
            &format!("bad\u{7}name\u{e9}{}", "x".repeat(4096)),
        ) else {
            panic!("terminate constructor");
        };
        assert!(reason.starts_with("bad?name?"));
        assert_eq!(reason.len(), MAX_TERMINATE_REASON_BYTES);
    }
}
