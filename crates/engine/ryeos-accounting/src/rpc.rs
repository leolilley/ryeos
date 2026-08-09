//! Strict callback DTOs for the provider-attempt budget lifecycle.
//!
//! Trust boundary: the daemon derives thread identity, launch generation,
//! scope, and the reservation maximum from the authenticated callback
//! capability, thread-auth proof, and persisted launch authority — never
//! from these payloads. The runtime supplies only intent coordinates,
//! digests, and typed accounting observations. Transport token fields are
//! stripped before these DTOs are decoded.

use serde::{Deserialize, Serialize};

use crate::authority::{BillableDimension, HexDigest};
use crate::money::UsdNanos;
use crate::state::{AttemptBudgetState, ChargeBasis, ReconciliationReason};

pub const RUNTIME_PROVIDER_ATTEMPT_PREPARE: &str = "runtime.provider_attempt_prepare";
pub const RUNTIME_PROVIDER_ATTEMPT_MARK_ISSUED: &str = "runtime.provider_attempt_mark_issued";
pub const RUNTIME_PROVIDER_ATTEMPT_SETTLE: &str = "runtime.provider_attempt_settle";
pub const RUNTIME_PROVIDER_ATTEMPT_RELEASE_UNISSUED: &str =
    "runtime.provider_attempt_release_unissued";
pub const RUNTIME_PROVIDER_ATTEMPT_GET: &str = "runtime.provider_attempt_get";
pub const RUNTIME_PROVIDER_ATTEMPT_LOCAL_STREAM_START: &str =
    "runtime.provider_attempt_local_stream_start";
pub const RUNTIME_PROVIDER_ATTEMPT_LOCAL_STREAM_NEXT: &str =
    "runtime.provider_attempt_local_stream_next";
pub const RUNTIME_PROVIDER_ATTEMPT_LOCAL_STREAM_CONTROL: &str =
    "runtime.provider_attempt_local_stream_control";

/// The verifier contract version both sides pin: the runtime's shared
/// trusted verifier stamps it (as a digest) into every
/// `VerifiedPreparedSpendBound`, and the daemon accepts exactly this
/// version. Advancing the verifier protocol advances this constant.
pub const SPEND_VERIFIER_CONTRACT_V1: &str = "spend-verifier/v1";

/// Upper bound applied to every free-text diagnostic accepted over RPC.
pub const MAX_DIAGNOSTIC_LEN: usize = 2048;
/// Upper bound applied to retained provider raw decimal audit text.
pub const MAX_RAW_DECIMAL_LEN: usize = 128;
pub const MAX_LOCAL_PROVIDER_REQUEST_BYTES: usize = 16 * 1024 * 1024;

fn validate_bounded(text: &str, what: &str, max: usize) -> Result<(), String> {
    if text.len() > max {
        return Err(format!("{what} exceeds {max} bytes"));
    }
    Ok(())
}

/// A bounded unit count for one billable dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitCount {
    pub dimension: BillableDimension,
    pub units: u64,
}

/// Commitments the shared trusted verifier extracted from the exact
/// prepared request, matching the sealed certificate kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpendBoundCommitments {
    /// `ProviderEnforcedChargeCap`: the cap field present in the prepared
    /// body at the contract's pointer, with the exact committed value.
    ProviderCapField {
        cap_field_pointer: String,
        cap_value: UsdNanos,
    },
    /// `DerivedWorstCaseCharge`: the frozen unit bounds and tariff
    /// generation the maximum was derived from.
    DerivedUnits {
        unit_bounds: Vec<UnitCount>,
        pricing_generation: String,
    },
    /// `ExplicitlyFree`: the free-contract digest for the route.
    ExplicitlyFree { contract_digest: HexDigest },
}

/// Bounded proof produced by the shared trusted verifier over the exact
/// `PreparedProviderRequest`; never contains prompts or body bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedPreparedSpendBound {
    pub prepared_request_digest: HexDigest,
    pub authority_digest: HexDigest,
    /// Must exactly equal the sealed authority maximum; a runtime cannot
    /// lower its own reservation.
    pub maximum: UsdNanos,
    pub commitments: SpendBoundCommitments,
    pub verifier_contract_digest: HexDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptPrepareParams {
    pub thread_id: String,
    pub turn: u32,
    pub attempt_number: u32,
    pub transport: ryeos_provider_contract::PreparedTransportIntent,
    pub request: ryeos_provider_contract::PreparedRequestProjection,
    pub verified_bound: VerifiedPreparedSpendBound,
}

impl ProviderAttemptPrepareParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.thread_id.is_empty() {
            return Err("prepare params must carry a thread".to_string());
        }
        if self.attempt_number == 0 || self.turn == 0 {
            return Err("turn and attempt_number are 1-based".to_string());
        }
        self.request.validate().map_err(|error| error.to_string())?;
        self.transport
            .validate()
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderAttemptPrepareResponse {
    Replay {
        record_hash: String,
        answer: ryeos_provider_contract::ProviderCallAnswer,
    },
    Reserved {
        attempt_id: String,
        request_hash: String,
        coordinate: ryeos_provider_contract::RequestCoordinate,
        reserved: UsdNanos,
        authority_digest: HexDigest,
        execution_budget_id: String,
        replayed: bool,
    },
    ReservationDenied {
        attempt_id: String,
        request_hash: String,
        coordinate: ryeos_provider_contract::RequestCoordinate,
        authority_digest: HexDigest,
        execution_budget_id: String,
        replayed: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptMarkIssuedParams {
    pub thread_id: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub expected_state: AttemptBudgetState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptMarkIssuedResponse {
    pub state: AttemptBudgetState,
    pub replayed: bool,
}

/// Typed spend observation for settlement, independent of token validity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpendAccounting {
    /// Trustworthy provider-reported final charge, raw decimal text as
    /// received. The daemon parses/validates it against the reconciliation
    /// contract; over-scale text is retained as audit truth.
    ProviderReportedFinal { raw_decimal: String },
    /// Authoritative unit counts for deterministic tariff settlement.
    TariffUnits { unit_counts: Vec<UnitCount> },
    /// Explicitly-free route: settles exact zero under its contract.
    ExplicitlyFree,
    /// No trustworthy spend accounting is available for the attempt.
    Unavailable { diagnostic: String },
}

impl SpendAccounting {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::ProviderReportedFinal { raw_decimal } => {
                validate_bounded(raw_decimal, "raw_decimal", MAX_RAW_DECIMAL_LEN)
            }
            Self::Unavailable { diagnostic } => {
                validate_bounded(diagnostic, "diagnostic", MAX_DIAGNOSTIC_LEN)
            }
            Self::TariffUnits { .. } | Self::ExplicitlyFree => Ok(()),
        }
    }
}

/// Typed token observation, validated independently of spend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenAccounting {
    Reported {
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_tokens: Option<u64>,
    },
    Invalid {
        diagnostic: String,
    },
    Unavailable,
}

impl TokenAccounting {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Invalid { diagnostic } => {
                validate_bounded(diagnostic, "diagnostic", MAX_DIAGNOSTIC_LEN)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptSettleParams {
    pub thread_id: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub authority_digest: HexDigest,
    pub coordinate: ryeos_provider_contract::RequestCoordinate,
    pub spend: SpendAccounting,
    pub tokens: TokenAccounting,
    /// Canonical behavior answer when the provider completed successfully.
    /// Absent for failed/cancelled/interrupted issued attempts, which settle
    /// financially but cannot publish replay evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<ryeos_provider_contract::ProviderCallAnswer>,
}

impl ProviderAttemptSettleParams {
    pub fn validate(&self) -> Result<(), String> {
        self.coordinate
            .validate()
            .map_err(|error| error.to_string())?;
        self.spend.validate()?;
        self.tokens.validate().and_then(|()| {
            self.answer.as_ref().map_or(Ok(()), |answer| {
                answer.validate().map_err(|error| error.to_string())
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptSettleResponse {
    pub state: AttemptBudgetState,
    pub budget_charge: UsdNanos,
    pub released: UsdNanos,
    pub charge_basis: ChargeBasis,
    pub replayed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<ProviderCallPublication>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderCallPublication {
    Inserted { record_hash: String },
    Folded { record_hash: String },
}

/// Durable proof that a recorded answer crossed the provider replay index.
/// Terminal financial state without this tuple is explicitly unbanked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallPublicationProof {
    pub cache_key: String,
    pub answer_digest: String,
    pub record_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptReleaseUnissuedParams {
    pub thread_id: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub reason: ReconciliationReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptReleaseUnissuedResponse {
    pub state: AttemptBudgetState,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptGetParams {
    pub thread_id: String,
    pub attempt_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptLocalStreamStartParams {
    pub thread_id: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub coordinate: ryeos_provider_contract::RequestCoordinate,
    /// Exact UTF-8 JSON bytes prepared before reservation. This value is
    /// transported only to the admitted local worker and is never retained in
    /// the accounting ledger or effect record.
    pub request_body: String,
}

impl ProviderAttemptLocalStreamStartParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.thread_id.is_empty() || self.attempt_id.is_empty() {
            return Err("local stream start requires thread and attempt ids".to_string());
        }
        if !lillux::valid_hash(&self.request_hash) {
            return Err("local stream request hash is not canonical".to_string());
        }
        if self.request_body.len() > MAX_LOCAL_PROVIDER_REQUEST_BYTES {
            return Err("local stream request body exceeds its byte bound".to_string());
        }
        self.coordinate
            .validate()
            .map_err(|error| error.to_string())?;
        if !matches!(
            self.coordinate.transport,
            ryeos_provider_contract::TransportCoordinate::AdmittedLocalWorker { .. }
        ) {
            return Err("local stream start requires an admitted-worker coordinate".to_string());
        }
        if lillux::sha256_hex(self.request_body.as_bytes()) != self.coordinate.body_sha256 {
            return Err("local stream body contradicts its reserved digest".to_string());
        }
        serde_json::from_str::<serde_json::Value>(&self.request_body)
            .map_err(|error| format!("local stream request body is not JSON: {error}"))?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderAttemptLocalStreamStartResponse {
    Stream {
        stream_id: String,
    },
    /// The exact contact is still running in this daemon generation, but this
    /// runtime has no replayable semantic cursor for it. A fresh runtime polls
    /// start until the daemon-retained terminal is available; it never
    /// reattaches to a partially consumed event stream.
    Pending {
        retry_after_ms: u64,
    },
    Replay {
        observation_hash: String,
        terminal: ryeos_provider_contract::AdmittedLocalWorkerFinal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptLocalStreamNextParams {
    pub thread_id: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub stream_id: String,
    pub after_sequence: u64,
    pub wait_ms: u64,
    pub max_events: u16,
}

impl ProviderAttemptLocalStreamNextParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.thread_id.is_empty()
            || self.attempt_id.is_empty()
            || !lillux::valid_hash(&self.request_hash)
            || !lillux::valid_hash(&self.stream_id)
            || self.wait_ms > 30_000
            || self.max_events == 0
            || self.max_events > 128
        {
            return Err("local stream poll is not canonical and bounded".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptLocalStreamEvent {
    pub sequence: u64,
    pub kind: ProviderAttemptLocalStreamEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptLocalStreamEventKind {
    Delta,
    Final,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptLocalStreamNextResponse {
    pub events: Vec<ProviderAttemptLocalStreamEvent>,
    pub terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptLocalStreamControl {
    Cancel,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptLocalStreamControlParams {
    pub thread_id: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub stream_id: String,
    pub action: ProviderAttemptLocalStreamControl,
}

impl ProviderAttemptLocalStreamControlParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.thread_id.is_empty()
            || self.attempt_id.is_empty()
            || !lillux::valid_hash(&self.request_hash)
            || !lillux::valid_hash(&self.stream_id)
        {
            return Err("local stream control is not canonical".to_string());
        }
        Ok(())
    }
}

/// Exact recorded state of one attempt, for lost-reply recovery reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptBudgetRecord {
    pub attempt_id: String,
    pub turn: u32,
    pub attempt_number: u32,
    pub state: AttemptBudgetState,
    pub request_hash: String,
    pub authority_digest: HexDigest,
    pub reserved: UsdNanos,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_charge: Option<UsdNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge_basis: Option<ChargeBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ReconciliationReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_proof: Option<ProviderCallPublicationProof>,
}

/// Reservation intent hash (§9.3), shared between the runtime that reserves
/// and the daemon that later verifies a record publication's echoed intent
/// against the reservation row. Both sides hash the same canonical value, so
/// equality with the stored `request_hash` binds every component — including
/// the exact request body digest — to the reservation the ledger billed.
#[allow(clippy::too_many_arguments)]
pub fn provider_attempt_request_hash(
    thread_id: &str,
    turn: u32,
    attempt_number: u32,
    provider_coordinate_key: &str,
) -> String {
    let value = serde_json::json!({
        "thread_id": thread_id,
        "turn": turn,
        "attempt_number": attempt_number,
        "provider_coordinate_key": provider_coordinate_key,
    });
    let canonical = lillux::cas::canonical_json(&value)
        .expect("request-hash input is plain scalar JSON and must canonicalize");
    lillux::cas::sha256_hex(canonical.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_derivations_are_deterministic_and_component_sensitive() {
        let base = || provider_attempt_request_hash("T-1", 2, 1, &"11".repeat(32));
        assert_eq!(base(), base());
        assert_ne!(
            base(),
            provider_attempt_request_hash("T-1", 2, 1, &"22".repeat(32))
        );
    }

    fn digest_of(tag: &str) -> HexDigest {
        HexDigest::new(lillux::cas::sha256_hex(tag.as_bytes())).unwrap()
    }

    #[test]
    fn prepare_params_strict_decode() {
        let request = ryeos_provider_contract::PreparedRequestProjection::from_coordinates(
            vec![],
            vec![],
            "11".repeat(32),
            64,
        )
        .unwrap();
        let params = ProviderAttemptPrepareParams {
            thread_id: "T-1".to_string(),
            turn: 1,
            attempt_number: 1,
            transport: ryeos_provider_contract::PreparedTransportIntent::RemoteHttp {
                method: "POST".to_string(),
                url: "https://provider.invalid/v1".to_string(),
            },
            request,
            verified_bound: VerifiedPreparedSpendBound {
                prepared_request_digest: digest_of("req"),
                authority_digest: digest_of("auth"),
                maximum: UsdNanos::parse_canonical("0.5").unwrap(),
                commitments: SpendBoundCommitments::DerivedUnits {
                    unit_bounds: vec![UnitCount {
                        dimension: BillableDimension::InputTokens,
                        units: 1000,
                    }],
                    pricing_generation: "gen-1".to_string(),
                },
                verifier_contract_digest: digest_of("verifier"),
            },
        };
        params.validate().unwrap();
        let mut value = serde_json::to_value(&params).unwrap();
        let back: ProviderAttemptPrepareParams = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, params);
        value
            .as_object_mut()
            .unwrap()
            .insert("spurious".to_string(), serde_json::Value::Null);
        assert!(serde_json::from_value::<ProviderAttemptPrepareParams>(value).is_err());
    }

    #[test]
    fn zero_based_coordinates_reject() {
        let mut params = ProviderAttemptPrepareParams {
            thread_id: "T-1".to_string(),
            turn: 0,
            attempt_number: 1,
            transport: ryeos_provider_contract::PreparedTransportIntent::RemoteHttp {
                method: "POST".to_string(),
                url: "https://provider.invalid/v1".to_string(),
            },
            request: ryeos_provider_contract::PreparedRequestProjection::from_coordinates(
                vec![],
                vec![],
                "11".repeat(32),
                64,
            )
            .unwrap(),
            verified_bound: VerifiedPreparedSpendBound {
                prepared_request_digest: digest_of("req"),
                authority_digest: digest_of("auth"),
                maximum: UsdNanos::ZERO,
                commitments: SpendBoundCommitments::ExplicitlyFree {
                    contract_digest: digest_of("free"),
                },
                verifier_contract_digest: digest_of("verifier"),
            },
        };
        assert!(params.validate().is_err());
        params.turn = 1;
        params.attempt_number = 0;
        assert!(params.validate().is_err());
    }

    #[test]
    fn diagnostics_are_bounded() {
        let spend = SpendAccounting::Unavailable {
            diagnostic: "x".repeat(MAX_DIAGNOSTIC_LEN + 1),
        };
        assert!(spend.validate().is_err());
        let spend = SpendAccounting::ProviderReportedFinal {
            raw_decimal: "1".repeat(MAX_RAW_DECIMAL_LEN + 1),
        };
        assert!(spend.validate().is_err());
    }
}
