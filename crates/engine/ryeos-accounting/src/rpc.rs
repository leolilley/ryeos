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

pub const RUNTIME_PROVIDER_ATTEMPT_RESERVE: &str = "runtime.provider_attempt_reserve";
pub const RUNTIME_PROVIDER_ATTEMPT_MARK_ISSUED: &str = "runtime.provider_attempt_mark_issued";
pub const RUNTIME_PROVIDER_ATTEMPT_SETTLE: &str = "runtime.provider_attempt_settle";
pub const RUNTIME_PROVIDER_ATTEMPT_RELEASE_UNISSUED: &str =
    "runtime.provider_attempt_release_unissued";
pub const RUNTIME_PROVIDER_ATTEMPT_GET: &str = "runtime.provider_attempt_get";

/// The verifier contract version both sides pin: the runtime's shared
/// trusted verifier stamps it (as a digest) into every
/// `VerifiedPreparedSpendBound`, and the daemon accepts exactly this
/// version. Advancing the verifier protocol advances this constant.
pub const SPEND_VERIFIER_CONTRACT_V1: &str = "spend-verifier/v1";

/// Upper bound applied to every free-text diagnostic accepted over RPC.
pub const MAX_DIAGNOSTIC_LEN: usize = 2048;
/// Upper bound applied to retained provider raw decimal audit text.
pub const MAX_RAW_DECIMAL_LEN: usize = 128;

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
pub struct ProviderAttemptReserveParams {
    pub thread_id: String,
    pub turn: u32,
    pub attempt_number: u32,
    /// Reservation intent hash (§9.3): binds coordinate, config hash, route
    /// facts, output limit, proven maximum authority, and body digest.
    pub request_hash: String,
    pub config_hash: String,
    pub verified_bound: VerifiedPreparedSpendBound,
}

impl ProviderAttemptReserveParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.thread_id.is_empty() || self.request_hash.is_empty() || self.config_hash.is_empty()
        {
            return Err("reserve params must carry thread, request hash, config hash".to_string());
        }
        if self.attempt_number == 0 || self.turn == 0 {
            return Err("turn and attempt_number are 1-based".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptReserveResponse {
    pub attempt_id: String,
    /// `Reserved`, or `ReservationDenied` for a durable insufficient-budget
    /// denial (no debit was created).
    pub state: AttemptBudgetState,
    pub reserved: UsdNanos,
    pub authority_digest: HexDigest,
    pub execution_budget_id: String,
    /// True when this response was recovered from the recorded operation
    /// rather than a fresh transition.
    pub replayed: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptSettleParams {
    pub thread_id: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub authority_digest: HexDigest,
    pub spend: SpendAccounting,
    pub tokens: TokenAccounting,
}

impl ProviderAttemptSettleParams {
    pub fn validate(&self) -> Result<(), String> {
        self.spend.validate()?;
        self.tokens.validate()
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
    config_hash: &str,
    provider_id: &str,
    model_name: &str,
    requested_output_tokens: Option<u64>,
    authority_digest: &str,
    body_sha256: &str,
) -> String {
    let value = serde_json::json!({
        "thread_id": thread_id,
        "turn": turn,
        "attempt_number": attempt_number,
        "config_hash": config_hash,
        "provider_id": provider_id,
        "model_name": model_name,
        "requested_output_tokens": requested_output_tokens,
        "authority_digest": authority_digest,
        "body_sha256": body_sha256,
    });
    let canonical = lillux::cas::canonical_json(&value)
        .expect("request-hash input is plain scalar JSON and must canonicalize");
    lillux::cas::sha256_hex(canonical.as_bytes())
}

/// The prepared provider request's digest (§9.2) from its non-secret parts:
/// method, url, sorted header names, exact body digest, and the effective
/// output ceiling. The runtime computes it at prepare time over what it will
/// actually send; the daemon recomputes it from an echoed preimage, so a
/// record's request identity is never runtime-named.
pub fn prepared_request_digest_from_parts(
    method: &str,
    url: &str,
    sorted_header_names: &[String],
    body_sha256: &str,
    requested_output_tokens: Option<u64>,
) -> String {
    let value = serde_json::json!({
        "method": method,
        "url": url,
        "header_names": sorted_header_names,
        "body_sha256": body_sha256,
        "requested_output_tokens": requested_output_tokens,
    });
    let canonical = lillux::cas::canonical_json(&value)
        .expect("request-digest input is plain scalar JSON and must canonicalize");
    lillux::cas::sha256_hex(canonical.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_derivations_are_deterministic_and_component_sensitive() {
        let base = || {
            provider_attempt_request_hash(
                "T-1", 2, 1, "cfg", "openrouter", "gpt", Some(64), "auth", "body",
            )
        };
        assert_eq!(base(), base());
        assert_ne!(
            base(),
            provider_attempt_request_hash(
                "T-1", 2, 1, "cfg", "openrouter", "gpt", Some(64), "auth", "other-body",
            )
        );

        let names = vec!["Content-Type".to_string(), "x-api-key".to_string()];
        let digest = || {
            prepared_request_digest_from_parts("POST", "https://p/v1", &names, "body", Some(64))
        };
        assert_eq!(digest(), digest());
        assert_ne!(
            digest(),
            prepared_request_digest_from_parts("POST", "https://p/v1", &names, "body", Some(65))
        );
    }

    fn digest_of(tag: &str) -> HexDigest {
        HexDigest::new(lillux::cas::sha256_hex(tag.as_bytes())).unwrap()
    }

    #[test]
    fn reserve_params_strict_decode() {
        let params = ProviderAttemptReserveParams {
            thread_id: "T-1".to_string(),
            turn: 1,
            attempt_number: 1,
            request_hash: "abc".to_string(),
            config_hash: "cfg".to_string(),
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
        let back: ProviderAttemptReserveParams = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, params);
        value
            .as_object_mut()
            .unwrap()
            .insert("spurious".to_string(), serde_json::Value::Null);
        assert!(serde_json::from_value::<ProviderAttemptReserveParams>(value).is_err());
    }

    #[test]
    fn zero_based_coordinates_reject() {
        let mut params = ProviderAttemptReserveParams {
            thread_id: "T-1".to_string(),
            turn: 0,
            attempt_number: 1,
            request_hash: "abc".to_string(),
            config_hash: "cfg".to_string(),
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
