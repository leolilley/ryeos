//! Provider accounting authority: the compact financial authority produced
//! by signed launch preparation, plus the signed tariff / provider-cap
//! contract documents that certificates reference by digest.
//!
//! `provider_id`, `model_name`, and `matched_profile` are diagnostic
//! attribution only — generic code never branches on them. The authority
//! digest, the certificate, and the exact prepared request are the authority.

use serde::{Deserialize, Serialize};

use crate::money::UsdNanos;

/// A lowercase hex sha-256 digest. Wire decoding validates the shape; an
/// arbitrary string can never enter through serde.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct HexDigest(String);

impl<'de> Deserialize<'de> for HexDigest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        HexDigest::new(raw).map_err(serde::de::Error::custom)
    }
}

impl HexDigest {
    pub fn new(digest: String) -> Result<Self, String> {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(format!("expected 64 lowercase hex chars, got {:?}", digest));
        }
        Ok(Self(digest))
    }

    pub fn of_canonical_json(value: &serde_json::Value) -> Result<Self, String> {
        let canonical = lillux::cas::canonical_json(value).map_err(|e| e.to_string())?;
        Ok(Self(lillux::cas::sha256_hex(canonical.as_bytes())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Closed set of billable dimensions a tariff or reconciliation contract can
/// cover. Add variants only when an activated route actually bills them —
/// no dormant policy branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BillableDimension {
    InputTokens,
    OutputTokens,
    ReasoningTokens,
    CacheReadTokens,
    CacheMissTokens,
    CacheWriteTokens,
    PerRequest,
}

/// A closed, sorted, duplicate-free set of covered billable dimensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ClosedBillableDimensionSet(Vec<BillableDimension>);

impl ClosedBillableDimensionSet {
    pub fn new(mut dims: Vec<BillableDimension>) -> Result<Self, String> {
        dims.sort();
        let before = dims.len();
        dims.dedup();
        if dims.len() != before {
            return Err("covered dimension set contains duplicates".to_string());
        }
        Ok(Self(dims))
    }

    pub fn contains(&self, dim: BillableDimension) -> bool {
        self.0.contains(&dim)
    }

    pub fn covers(&self, required: &[BillableDimension]) -> bool {
        required.iter().all(|d| self.contains(*d))
    }

    pub fn as_slice(&self) -> &[BillableDimension] {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ClosedBillableDimensionSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let dims = Vec::<BillableDimension>::deserialize(deserializer)?;
        let sorted = {
            let mut copy = dims.clone();
            copy.sort();
            copy
        };
        if sorted != dims {
            return Err(serde::de::Error::custom(
                "covered dimension set must be sorted canonically",
            ));
        }
        Self::new(dims).map_err(serde::de::Error::custom)
    }
}

/// The only currency in this version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Currency {
    Usd,
}

/// Finality semantics of a provider-reported final charge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalityContract {
    /// The reported charge is final when the response/stream completes; no
    /// later adjustment is trusted for settlement.
    pub final_on_response: bool,
    /// Maximum fraction-digit scale the contract permits for the reported
    /// charge's VALUE (not its textual form — exponent notation and
    /// trailing zeros are valued first; see `reported_decimal_scale`). A
    /// value exact within nine digits settles as-is; a value genuinely
    /// finer than nanos may be rounded toward positive infinity for
    /// enforcement only when its scale is within this declared bound.
    pub max_reported_fraction_digits: u8,
    /// A zero reported charge on a BYOK response is a covered final charge.
    /// Never inferred from a zero or missing report without this.
    #[serde(default)]
    pub byok_zero_is_final: bool,
}

/// Proof kind behind a hard spend bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpendBoundCertificate {
    /// The provider contract and the exact emitted request impose a
    /// server-enforced maximum total charge for that request.
    ProviderEnforcedChargeCap {
        request_cap_contract_digest: HexDigest,
        currency: Currency,
    },
    /// A conservative maximum mechanically derived from the frozen request,
    /// bounded input/output units, and a signed tariff covering every
    /// applicable billable dimension.
    DerivedWorstCaseCharge {
        tariff_contract_digest: HexDigest,
        request_limit_digest: HexDigest,
        covered_dimensions: ClosedBillableDimensionSet,
        currency: Currency,
        pricing_generation: String,
        /// Absent only when the tariff contract states the bound is
        /// indefinite or irrevocably fixed for the exact prepared request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<i64>,
    },
}

/// Spend-bound authority for one resolved route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpendBoundAuthority {
    Paid {
        maximum: UsdNanos,
        certificate: SpendBoundCertificate,
    },
    ExplicitlyFree {
        contract_digest: HexDigest,
    },
    /// A declared admission bound without mechanical proof. Ineligible for
    /// hard spend; never silently upgraded.
    AdvisoryOnly,
}

/// How the actual charge for an issued attempt is reconciled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChargeReconciliationAuthority {
    ProviderReportedFinalCharge {
        schema_digest: HexDigest,
        covered_dimensions: ClosedBillableDimensionSet,
        finality_contract: FinalityContract,
    },
    /// The complete signed tariff is embedded (not referenced by digest) so
    /// the daemon ledger settles deterministic costs from the sealed
    /// authority alone, without reaching into any runtime-owned snapshot.
    DeterministicTariff {
        tariff: SpendTariffDocument,
    },
    Unavailable,
}

/// Compact financial authority sealed into the admitted launch capsule.
///
/// `authority_digest` commits to every other field; `validate()` recomputes
/// and compares it. All money is fixed-point; nothing here is a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAccountingAuthority {
    pub authority_digest: HexDigest,
    pub config_hash: String,
    pub config_value_digest: HexDigest,
    pub billing_principal_digest: HexDigest,
    pub credential_authority_generation: String,
    pub pricing_contract_subject_digest: HexDigest,
    pub provider_id: String,
    pub model_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_profile: Option<String>,
    pub spend_bound: SpendBoundAuthority,
    pub reconciliation: ChargeReconciliationAuthority,
}

impl ProviderAccountingAuthority {
    /// Digest over every field except `authority_digest` itself, via
    /// canonical JSON.
    pub fn compute_digest(&self) -> Result<HexDigest, String> {
        let mut value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let obj = value
            .as_object_mut()
            .ok_or_else(|| "authority must serialize to an object".to_string())?;
        obj.remove("authority_digest");
        HexDigest::of_canonical_json(&value)
    }

    pub fn validate(&self) -> Result<(), String> {
        let computed = self.compute_digest()?;
        if computed != self.authority_digest {
            return Err(format!(
                "authority digest mismatch: sealed {} computed {}",
                self.authority_digest.as_str(),
                computed.as_str()
            ));
        }
        if self.provider_id.is_empty() || self.model_name.is_empty() {
            return Err("authority attribution fields must be non-empty".to_string());
        }
        if let SpendBoundAuthority::Paid { maximum, .. } = &self.spend_bound
            && maximum.is_zero()
        {
            return Err(
                "paid spend bound must have a positive maximum; zero cost requires an \
                 explicitly-free contract"
                    .to_string(),
            );
        }
        if let ChargeReconciliationAuthority::DeterministicTariff { tariff } = &self.reconciliation
        {
            tariff.validate()?;
        }
        Ok(())
    }

    /// Seal: compute and store the digest.
    pub fn sealed(mut self) -> Result<Self, String> {
        self.authority_digest = self.compute_digest()?;
        Ok(self)
    }
}

/// Contract tag for the credential binding MAC construction. Bound into the
/// digest so the construction can never be silently swapped.
pub const CREDENTIAL_BINDING_MAC_CONTRACT: &str = "hmac-sha256/v1";

/// Non-secret digest binding one sealed financial authority to the exact
/// credential values resolved for its launch. Both launch and issue compute
/// this through daemon-owned secret resolution; a runtime never supplies it.
///
/// Secret values enter only as HMAC-SHA256 under `binding_key`, a random
/// daemon-held key persisted outside the ledger database — the stored digest
/// must never become an offline verification oracle for a reader of the
/// ledger file alone.
pub fn credential_binding_digest(
    binding_key: &[u8],
    authority: &ProviderAccountingAuthority,
    secrets: &[(String, String)],
) -> Result<HexDigest, String> {
    if binding_key.len() < 16 {
        return Err("credential binding key must be at least 16 bytes".to_string());
    }
    let mut bindings: Vec<(&str, String)> = secrets
        .iter()
        .map(|(name, value)| {
            (
                name.as_str(),
                lillux::crypto::hmac_sha256_hex(binding_key, value.as_bytes()),
            )
        })
        .collect();
    bindings.sort_by(|left, right| left.0.cmp(right.0));
    if bindings.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err("credential binding contains duplicate secret names".to_string());
    }
    let value = serde_json::json!({
        "binding_mac": CREDENTIAL_BINDING_MAC_CONTRACT,
        "credential_authority_generation": &authority.credential_authority_generation,
        "billing_principal_digest": authority.billing_principal_digest.as_str(),
        "pricing_contract_subject_digest": authority.pricing_contract_subject_digest.as_str(),
        "secrets": bindings
            .into_iter()
            .map(|(name, value_mac)| serde_json::json!({
                "name": name,
                "value_mac": value_mac,
            }))
            .collect::<Vec<_>>(),
    });
    HexDigest::of_canonical_json(&value)
}

/// Signed tariff document: the content a `DerivedWorstCaseCharge`
/// certificate references by digest. Authored and verified through the
/// repository's normal provider-config provenance/signing workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpendTariffDocument {
    pub schema_version: u32,
    pub currency: Currency,
    pub pricing_generation: String,
    /// Per-million-unit rates for token dimensions, canonical decimal USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_million: Option<UsdNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_million: Option<UsdNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_per_million: Option<UsdNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_million: Option<UsdNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_per_million: Option<UsdNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million: Option<UsdNanos>,
    /// Flat surcharge per request, canonical decimal USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_request: Option<UsdNanos>,
    /// Every dimension this route can bill. The certificate is valid only
    /// when each covered dimension has a rate (or is `per_request`).
    pub covered_dimensions: ClosedBillableDimensionSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

pub const SPEND_TARIFF_SCHEMA_VERSION: u32 = 1;

impl SpendTariffDocument {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SPEND_TARIFF_SCHEMA_VERSION {
            return Err(format!(
                "unsupported tariff schema_version {}",
                self.schema_version
            ));
        }
        if self.pricing_generation.is_empty() {
            return Err("tariff pricing_generation must be non-empty".to_string());
        }
        for dim in self.covered_dimensions.as_slice() {
            if self.rate_for(*dim).is_none() {
                return Err(format!(
                    "tariff covers dimension {dim:?} without declaring its rate"
                ));
            }
        }
        Ok(())
    }

    pub fn rate_for(&self, dim: BillableDimension) -> Option<UsdNanos> {
        match dim {
            BillableDimension::InputTokens => self.input_per_million,
            BillableDimension::OutputTokens => self.output_per_million,
            BillableDimension::ReasoningTokens => self.reasoning_per_million,
            BillableDimension::CacheReadTokens => self.cache_read_per_million,
            BillableDimension::CacheMissTokens => self.cache_miss_per_million,
            BillableDimension::CacheWriteTokens => self.cache_write_per_million,
            BillableDimension::PerRequest => self.per_request,
        }
    }

    pub fn digest(&self) -> Result<HexDigest, String> {
        let value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        HexDigest::of_canonical_json(&value)
    }

    /// Conservative worst-case charge for bounded unit counts, rounding
    /// toward positive infinity per dimension, checked throughout.
    pub fn worst_case_charge(
        &self,
        bounds: &[(BillableDimension, u64)],
    ) -> Result<UsdNanos, String> {
        let mut total = UsdNanos::ZERO;
        for dim in self.covered_dimensions.as_slice() {
            let rate = self
                .rate_for(*dim)
                .ok_or_else(|| format!("no rate for covered dimension {dim:?}"))?;
            let charge = if *dim == BillableDimension::PerRequest {
                rate
            } else {
                let units = bounds
                    .iter()
                    .find(|(d, _)| d == dim)
                    .map(|(_, u)| *u)
                    .ok_or_else(|| {
                        format!("no bounded unit count supplied for covered dimension {dim:?}")
                    })?;
                UsdNanos::rate_per_million_mul_units_round_up(rate, units)
                    .map_err(|e| e.to_string())?
            };
            total = total.checked_add(charge).map_err(|e| e.to_string())?;
        }
        Ok(total)
    }
}

/// Signed provider-enforced request-cap contract: the content a
/// `ProviderEnforcedChargeCap` certificate references by digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderChargeCapContract {
    pub schema_version: u32,
    pub currency: Currency,
    /// JSON pointer into the prepared request body where the server-enforced
    /// maximum total charge is set (e.g. `/max_cost_usd`).
    pub cap_field_pointer: String,
    /// The exact cap value RyeOS writes into that field; this is the sealed
    /// route maximum for `ProviderEnforcedChargeCap` routes.
    pub maximum: UsdNanos,
    pub finality: FinalityContract,
}

pub const PROVIDER_CHARGE_CAP_SCHEMA_VERSION: u32 = 1;

impl ProviderChargeCapContract {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != PROVIDER_CHARGE_CAP_SCHEMA_VERSION {
            return Err(format!(
                "unsupported charge-cap schema_version {}",
                self.schema_version
            ));
        }
        if !self.cap_field_pointer.starts_with('/') {
            return Err("cap_field_pointer must be a JSON pointer".to_string());
        }
        if self.maximum.is_zero() {
            return Err("charge-cap maximum must be positive".to_string());
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<HexDigest, String> {
        let value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        HexDigest::of_canonical_json(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(tag: &str) -> HexDigest {
        HexDigest::new(lillux::cas::sha256_hex(tag.as_bytes())).unwrap()
    }

    fn tariff() -> SpendTariffDocument {
        SpendTariffDocument {
            schema_version: SPEND_TARIFF_SCHEMA_VERSION,
            currency: Currency::Usd,
            pricing_generation: "gen-1".to_string(),
            input_per_million: Some(UsdNanos::parse_canonical("3").unwrap()),
            output_per_million: Some(UsdNanos::parse_canonical("15").unwrap()),
            reasoning_per_million: None,
            cache_read_per_million: None,
            cache_miss_per_million: None,
            cache_write_per_million: None,
            per_request: None,
            covered_dimensions: ClosedBillableDimensionSet::new(vec![
                BillableDimension::InputTokens,
                BillableDimension::OutputTokens,
            ])
            .unwrap(),
            expires_at_ms: None,
        }
    }

    fn authority() -> ProviderAccountingAuthority {
        let t = tariff();
        ProviderAccountingAuthority {
            authority_digest: digest_of("placeholder"),
            config_hash: "cfg".to_string(),
            config_value_digest: digest_of("cfg-value"),
            billing_principal_digest: digest_of("principal"),
            credential_authority_generation: "cred-gen-1".to_string(),
            pricing_contract_subject_digest: digest_of("pricing-subject"),
            provider_id: "route".to_string(),
            model_name: "model".to_string(),
            matched_profile: None,
            spend_bound: SpendBoundAuthority::Paid {
                maximum: UsdNanos::parse_canonical("0.5").unwrap(),
                certificate: SpendBoundCertificate::DerivedWorstCaseCharge {
                    tariff_contract_digest: t.digest().unwrap(),
                    request_limit_digest: digest_of("request-limit"),
                    covered_dimensions: t.covered_dimensions.clone(),
                    currency: Currency::Usd,
                    pricing_generation: "gen-1".to_string(),
                    expires_at_ms: None,
                },
            },
            reconciliation: ChargeReconciliationAuthority::DeterministicTariff { tariff: t },
        }
        .sealed()
        .unwrap()
    }

    #[test]
    fn hex_digest_validation() {
        assert!(HexDigest::new("ab".repeat(32)).is_ok());
        assert!(HexDigest::new("AB".repeat(32)).is_err());
        assert!(HexDigest::new("xyz".to_string()).is_err());
    }

    #[test]
    fn dimension_set_is_canonical() {
        assert!(
            ClosedBillableDimensionSet::new(vec![
                BillableDimension::OutputTokens,
                BillableDimension::InputTokens,
            ])
            .is_ok()
        );
        assert!(
            ClosedBillableDimensionSet::new(vec![
                BillableDimension::InputTokens,
                BillableDimension::InputTokens,
            ])
            .is_err()
        );
        // Wire decoding requires canonical order.
        assert!(
            serde_json::from_str::<ClosedBillableDimensionSet>(
                "[\"output_tokens\",\"input_tokens\"]"
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ClosedBillableDimensionSet>(
                "[\"input_tokens\",\"output_tokens\"]"
            )
            .is_ok()
        );
    }

    #[test]
    fn authority_digest_seals_every_field() {
        let sealed = authority();
        sealed.validate().unwrap();
        let mut tampered = sealed.clone();
        tampered.model_name = "other-model".to_string();
        assert!(tampered.validate().is_err());
    }

    const BINDING_KEY: &[u8] = b"test-credential-binding-key-32b!";

    #[test]
    fn credential_binding_is_order_independent_and_value_sensitive() {
        let sealed = authority();
        let first = credential_binding_digest(
            BINDING_KEY,
            &sealed,
            &[
                ("SECONDARY_KEY".to_string(), "two".to_string()),
                ("API_KEY".to_string(), "one".to_string()),
            ],
        )
        .unwrap();
        let reordered = credential_binding_digest(
            BINDING_KEY,
            &sealed,
            &[
                ("API_KEY".to_string(), "one".to_string()),
                ("SECONDARY_KEY".to_string(), "two".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(first, reordered);

        let changed = credential_binding_digest(
            BINDING_KEY,
            &sealed,
            &[
                ("API_KEY".to_string(), "rotated".to_string()),
                ("SECONDARY_KEY".to_string(), "two".to_string()),
            ],
        )
        .unwrap();
        assert_ne!(first, changed);

        let mut next_generation = sealed.clone();
        next_generation.credential_authority_generation = "cred-gen-2".to_string();
        next_generation = next_generation.sealed().unwrap();
        assert_ne!(
            first,
            credential_binding_digest(
                BINDING_KEY,
                &next_generation,
                &[
                    ("API_KEY".to_string(), "one".to_string()),
                    ("SECONDARY_KEY".to_string(), "two".to_string()),
                ],
            )
            .unwrap()
        );
    }

    #[test]
    fn credential_binding_is_keyed_not_an_offline_oracle() {
        // The same authority and secret values under a different daemon key
        // must produce an unrelated digest: a reader of the ledger file
        // alone cannot verify guesses against the stored digest.
        let sealed = authority();
        let secrets = [("API_KEY".to_string(), "one".to_string())];
        let under_first_key = credential_binding_digest(BINDING_KEY, &sealed, &secrets).unwrap();
        let under_other_key =
            credential_binding_digest(b"other-credential-binding-key-32!", &sealed, &secrets)
                .unwrap();
        assert_ne!(under_first_key, under_other_key);
        assert!(credential_binding_digest(b"short", &sealed, &secrets).is_err());
    }

    #[test]
    fn credential_binding_rejects_duplicate_secret_names() {
        assert!(
            credential_binding_digest(
                BINDING_KEY,
                &authority(),
                &[
                    ("API_KEY".to_string(), "one".to_string()),
                    ("API_KEY".to_string(), "two".to_string()),
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn paid_zero_maximum_rejects() {
        let mut a = authority();
        if let SpendBoundAuthority::Paid { maximum, .. } = &mut a.spend_bound {
            *maximum = UsdNanos::ZERO;
        }
        let resealed = a.sealed().unwrap();
        assert!(resealed.validate().is_err());
    }

    #[test]
    fn tariff_requires_rate_per_covered_dimension() {
        let mut t = tariff();
        t.validate().unwrap();
        t.output_per_million = None;
        assert!(t.validate().is_err());
    }

    #[test]
    fn worst_case_charge_is_conservative_and_checked() {
        let t = tariff();
        // 100k input at $3/M = $0.30; 8192 output at $15/M = $0.12288.
        let bound = t
            .worst_case_charge(&[
                (BillableDimension::InputTokens, 100_000),
                (BillableDimension::OutputTokens, 8_192),
            ])
            .unwrap();
        assert_eq!(bound.to_canonical_string(), "0.42288");
        // Missing a covered dimension's bound is an error, not zero.
        assert!(
            t.worst_case_charge(&[(BillableDimension::InputTokens, 100_000)])
                .is_err()
        );
    }

    #[test]
    fn authority_wire_round_trip_is_strict() {
        let sealed = authority();
        let json = serde_json::to_value(&sealed).unwrap();
        let back: ProviderAccountingAuthority = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, sealed);
        let mut extra = json;
        extra
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_string(), serde_json::Value::Null);
        assert!(serde_json::from_value::<ProviderAccountingAuthority>(extra).is_err());
    }
}
