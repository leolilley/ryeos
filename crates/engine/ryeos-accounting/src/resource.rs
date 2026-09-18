//! Financial authority and trustworthy usage evidence for one selected local
//! execution resource. This module owns no device discovery or process
//! lifecycle: it binds their exact retained identities to the shared financial
//! reservation/settlement substrate.

use serde::{Deserialize, Serialize};

use crate::{Currency, HexDigest, UsdNanos};

pub const RESOURCE_ACCOUNTING_AUTHORITY_VERSION: u32 = 1;
pub const RESOURCE_METER_CONTRACT_VERSION: u32 = 1;
pub const RESOURCE_TARIFF_VERSION: u32 = 1;
pub const RESOURCE_USAGE_OBSERVATION_VERSION: u32 = 1;
pub const RESOURCE_RATED_CHARGE_VERSION: u32 = 1;
pub const RESOURCE_BUDGET_TRANSITION_VERSION: u32 = 1;
pub const RESOURCE_OPERATION_BINDING_VERSION: u32 = 3;
pub const RESOURCE_REQUEST_ATTRIBUTION_VERSION: u32 = 1;
pub const RESOURCE_USAGE_PARTITION_VERSION: u32 = 1;

const MAX_TOKEN_BYTES: usize = 256;
const MAX_INTERVALS: usize = 64;
const MAX_ATTRIBUTIONS: usize = 1_024;

/// The closed initial meter set. A different clock or measurement meaning is
/// a different reviewed contract, not an arbitrary string in node policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceMeterKind {
    /// Wall-independent elapsed occupancy of a selected resource by one exact
    /// durable execution owner. The host clock and its incarnation evidence
    /// are supplied by Lillux rather than named by the accounting contract.
    OccupancyDuration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceMeterContract {
    pub version: u32,
    pub kind: ResourceMeterKind,
    /// Exact platform-neutral clock-behavior contract supplied by Lillux.
    /// Accounting treats this as opaque identity and never selects a host
    /// clock or interprets an operating-system clock name.
    pub clock_contract_digest: HexDigest,
    pub contract_digest: HexDigest,
}

impl ResourceMeterContract {
    pub fn compute_digest(&self) -> Result<HexDigest, String> {
        let mut value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        value
            .as_object_mut()
            .ok_or_else(|| "resource meter must encode as an object".to_string())?
            .remove("contract_digest");
        HexDigest::of_canonical_json(&value)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != RESOURCE_METER_CONTRACT_VERSION {
            return Err(format!(
                "unsupported resource meter version {}",
                self.version
            ));
        }
        let computed = self.compute_digest()?;
        if computed != self.contract_digest {
            return Err("resource meter contract digest mismatch".to_string());
        }
        Ok(())
    }

    pub fn sealed(mut self) -> Result<Self, String> {
        self.contract_digest = self.compute_digest()?;
        Ok(self)
    }
}

/// What a deterministic resource tariff represents. Internal allocation and
/// observed external expenditure are never silently additive for the same
/// coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceChargeClass {
    InternalAllocation,
    ExternalExpenditure,
}

/// Immutable deterministic tariff over occupancy milliseconds. Nanosecond
/// meter evidence is converted by rounding duration upward to milliseconds,
/// then upward to the declared billing quantum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceTariffDocument {
    pub version: u32,
    pub currency: Currency,
    pub pricing_generation: String,
    pub charge_class: ResourceChargeClass,
    pub rate_per_million_milliseconds: UsdNanos,
    pub billing_quantum_milliseconds: u64,
    pub minimum_billable_milliseconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

impl ResourceTariffDocument {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != RESOURCE_TARIFF_VERSION {
            return Err(format!(
                "unsupported resource tariff version {}",
                self.version
            ));
        }
        validate_token("pricing_generation", &self.pricing_generation)?;
        if self.rate_per_million_milliseconds.is_zero() {
            return Err("resource tariff rate must be positive".to_string());
        }
        if self.billing_quantum_milliseconds == 0 {
            return Err("resource tariff billing quantum must be positive".to_string());
        }
        if self.minimum_billable_milliseconds % self.billing_quantum_milliseconds != 0 {
            return Err(
                "resource tariff minimum must be a multiple of its billing quantum".to_string(),
            );
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<HexDigest, String> {
        self.validate()?;
        let value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        HexDigest::of_canonical_json(&value)
    }

    pub fn billable_milliseconds(&self, observed_nanoseconds: u64) -> Result<u64, String> {
        self.validate()?;
        let milliseconds = observed_nanoseconds
            .checked_add(999_999)
            .ok_or_else(|| "resource duration rounding overflow".to_string())?
            / 1_000_000;
        let units = milliseconds.max(self.minimum_billable_milliseconds);
        let quantum = self.billing_quantum_milliseconds;
        let quanta = units
            .checked_add(quantum - 1)
            .ok_or_else(|| "resource billing-quantum rounding overflow".to_string())?
            / quantum;
        quanta
            .checked_mul(quantum)
            .ok_or_else(|| "resource billable duration overflow".to_string())
    }

    pub fn charge_for_nanoseconds(&self, observed_nanoseconds: u64) -> Result<UsdNanos, String> {
        let units = self.billable_milliseconds(observed_nanoseconds)?;
        UsdNanos::rate_per_million_mul_units_round_up(self.rate_per_million_milliseconds, units)
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceSpendAuthority {
    /// Complete lifetime is reserved under a deterministic tariff before the
    /// operation's issue boundary.
    Bounded {
        maximum_occupancy_milliseconds: u64,
        maximum: UsdNanos,
    },
    /// Evidence/reporting only. It creates no financial reservation and cannot
    /// satisfy an applicable hard account.
    Advisory,
}

/// Node-admitted financial authority for one stable resource descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceAccountingAuthority {
    pub version: u32,
    pub authority_digest: HexDigest,
    pub stable_resource_id: String,
    pub resource_class: String,
    pub observation_contract_digest: HexDigest,
    pub meter: ResourceMeterContract,
    pub tariff: ResourceTariffDocument,
    pub spend: ResourceSpendAuthority,
}

/// Minimal immutable financial/meter identity retained with the exact live
/// process owner. Full tariff authority remains in the accounting ledger;
/// lifecycle recovery needs only these coordinates to settle that operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceOperationBinding {
    pub version: u32,
    /// Durable accounting-owner gate for the complete process occurrence.
    /// This is distinct from any provider/request launch gate and remains the
    /// admission fence for resident reuse until the owner is drained.
    pub owner_gate_id: HexDigest,
    pub operation_id: HexDigest,
    pub request_digest: HexDigest,
    pub owner_incarnation: HexDigest,
    pub stable_resource_id: String,
    pub authority_digest: HexDigest,
    pub meter_contract_digest: HexDigest,
    pub clock_contract_digest: HexDigest,
    /// Required-nullable financially reserved occupancy. Advisory operations
    /// have no hard lifetime and therefore cannot fund a bounded resident
    /// session.
    pub maximum_occupancy_milliseconds: Option<u64>,
}

impl ResourceOperationBinding {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != RESOURCE_OPERATION_BINDING_VERSION {
            return Err("unsupported resource operation binding version".to_string());
        }
        validate_token("stable_resource_id", &self.stable_resource_id)?;
        if self.maximum_occupancy_milliseconds == Some(0) {
            return Err("resource operation occupancy maximum must be positive".to_string());
        }
        Ok(())
    }
}

impl ResourceAccountingAuthority {
    pub fn compute_digest(&self) -> Result<HexDigest, String> {
        let mut value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        value
            .as_object_mut()
            .ok_or_else(|| "resource accounting authority must encode as an object".to_string())?
            .remove("authority_digest");
        HexDigest::of_canonical_json(&value)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != RESOURCE_ACCOUNTING_AUTHORITY_VERSION {
            return Err(format!(
                "unsupported resource accounting authority version {}",
                self.version
            ));
        }
        validate_token("stable_resource_id", &self.stable_resource_id)?;
        validate_token("resource_class", &self.resource_class)?;
        self.meter.validate()?;
        self.tariff.validate()?;
        // The initial meter proves local occupancy only.  It can price an
        // internal allocation, but it is not an invoice, provider receipt, or
        // external billing-control authority.  Keep external expenditure as
        // a distinct future evidence path instead of manufacturing it from a
        // host clock.
        if self.tariff.charge_class != ResourceChargeClass::InternalAllocation {
            return Err(
                "occupancy-rated resource authority must use internal_allocation; external expenditure requires independent billing evidence"
                    .to_string(),
            );
        }
        if let ResourceSpendAuthority::Bounded {
            maximum_occupancy_milliseconds,
            maximum,
        } = &self.spend
        {
            if *maximum_occupancy_milliseconds == 0 || maximum.is_zero() {
                return Err("bounded resource spend must have positive limits".to_string());
            }
            let observed_ns = maximum_occupancy_milliseconds
                .checked_mul(1_000_000)
                .ok_or_else(|| "resource maximum duration overflows nanoseconds".to_string())?;
            let recomputed = self.tariff.charge_for_nanoseconds(observed_ns)?;
            if recomputed != *maximum {
                return Err(format!(
                    "resource maximum {} does not match tariff-derived {}",
                    maximum.to_canonical_string(),
                    recomputed.to_canonical_string()
                ));
            }
        }
        if self.compute_digest()? != self.authority_digest {
            return Err("resource accounting authority digest mismatch".to_string());
        }
        Ok(())
    }

    pub fn sealed(mut self) -> Result<Self, String> {
        self.authority_digest = self.compute_digest()?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceUsageCoverage {
    Complete,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceBudgetState {
    ReservationDenied,
    Reserved,
    ReleasedUnissued,
    Issued,
    Reconciled,
    ChargedReservedMaximum,
    ReservationBoundViolated,
    AdvisoryPending,
    AdvisoryReleasedUnissued,
    AdvisoryIssued,
    AdvisoryReconciled,
}

impl ResourceBudgetState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReservationDenied => "reservation_denied",
            Self::Reserved => "reserved",
            Self::ReleasedUnissued => "released_unissued",
            Self::Issued => "issued",
            Self::Reconciled => "reconciled",
            Self::ChargedReservedMaximum => "charged_reserved_maximum",
            Self::ReservationBoundViolated => "reservation_bound_violated",
            Self::AdvisoryPending => "advisory_pending",
            Self::AdvisoryReleasedUnissued => "advisory_released_unissued",
            Self::AdvisoryIssued => "advisory_issued",
            Self::AdvisoryReconciled => "advisory_reconciled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "reservation_denied" => Self::ReservationDenied,
            "reserved" => Self::Reserved,
            "released_unissued" => Self::ReleasedUnissued,
            "issued" => Self::Issued,
            "reconciled" => Self::Reconciled,
            "charged_reserved_maximum" => Self::ChargedReservedMaximum,
            "reservation_bound_violated" => Self::ReservationBoundViolated,
            "advisory_pending" => Self::AdvisoryPending,
            "advisory_released_unissued" => Self::AdvisoryReleasedUnissued,
            "advisory_issued" => Self::AdvisoryIssued,
            "advisory_reconciled" => Self::AdvisoryReconciled,
            _ => return None,
        })
    }

    pub fn may_transition_to(self, next: Self) -> bool {
        use ResourceBudgetState as S;
        matches!(
            (self, next),
            (S::Reserved, S::Issued | S::ReleasedUnissued)
                | (
                    S::Issued,
                    S::Reconciled | S::ChargedReservedMaximum | S::ReservationBoundViolated
                )
                | (
                    S::ChargedReservedMaximum,
                    S::Reconciled | S::ReservationBoundViolated
                )
                | (
                    S::AdvisoryPending,
                    S::AdvisoryIssued | S::AdvisoryReleasedUnissued
                )
                | (S::AdvisoryIssued, S::AdvisoryIssued | S::AdvisoryReconciled)
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::ReservationDenied
                | Self::ReleasedUnissued
                | Self::Reconciled
                | Self::ReservationBoundViolated
                | Self::AdvisoryReleasedUnissued
                | Self::AdvisoryReconciled
        )
    }
}

/// Typed audit payload for the resource side of the shared financial ledger.
/// Provider testimony remains in its provider-specific event; this event has
/// no synthetic model, token, retry or credential fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceBudgetTransitionV1 {
    pub version: u32,
    pub transition_id: String,
    pub transition_sequence: u32,
    pub operation_id: String,
    pub budget_authority_site_id: String,
    pub ledger_epoch: u64,
    pub execution_budget_id: String,
    pub root_chain_id: String,
    pub audit_chain_root_id: String,
    pub thread_id: String,
    pub launch_generation: String,
    pub owner_incarnation: String,
    pub stable_resource_id: String,
    pub authority_digest: HexDigest,
    pub transition: ResourceBudgetState,
    pub reserved_usd_nanos: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_charge_usd_nanos: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_digest: Option<HexDigest>,
    pub occurred_at_ms: i64,
}

impl ResourceBudgetTransitionV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != RESOURCE_BUDGET_TRANSITION_VERSION || self.transition_sequence == 0 {
            return Err("resource budget transition version/sequence is invalid".to_string());
        }
        for (label, value) in [
            ("operation_id", &self.operation_id),
            ("budget_authority_site_id", &self.budget_authority_site_id),
            ("execution_budget_id", &self.execution_budget_id),
            ("root_chain_id", &self.root_chain_id),
            ("audit_chain_root_id", &self.audit_chain_root_id),
            ("thread_id", &self.thread_id),
            ("launch_generation", &self.launch_generation),
            ("owner_incarnation", &self.owner_incarnation),
            ("stable_resource_id", &self.stable_resource_id),
        ] {
            validate_token(label, value)?;
        }
        if self.transition_id
            != crate::event::transition_id(&self.operation_id, self.transition_sequence)
        {
            return Err("resource budget transition id is not canonical".to_string());
        }
        use ResourceBudgetState as S;
        let expected = match self.transition {
            S::ReservationDenied | S::Reserved | S::ReleasedUnissued | S::Issued => {
                (false, false, false)
            }
            S::Reconciled | S::ReservationBoundViolated => (false, true, true),
            S::ChargedReservedMaximum => (false, true, false),
            S::AdvisoryPending | S::AdvisoryReleasedUnissued => (true, false, false),
            // Advisory issuance may later retain an exact partial usage row
            // while terminal coverage remains unknown. It still has neither a
            // reservation nor a financial debit.
            S::AdvisoryIssued => {
                let valid = self.reserved_usd_nanos == 0 && self.budget_charge_usd_nanos.is_none();
                if !valid {
                    return Err(format!(
                        "resource budget transition `{}` has contradictory reservation/charge/usage fields",
                        self.transition.as_str()
                    ));
                }
                return Ok(());
            }
            S::AdvisoryReconciled => (true, false, true),
        };
        let actual = (
            self.reserved_usd_nanos == 0,
            self.budget_charge_usd_nanos.is_some(),
            self.usage_digest.is_some(),
        );
        if actual != expected {
            return Err(format!(
                "resource budget transition `{}` has contradictory reservation/charge/usage fields",
                self.transition.as_str()
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUsageInterval {
    pub start_tick_ns: u64,
    pub end_tick_ns: u64,
}

impl ResourceUsageInterval {
    pub fn duration_ns(&self) -> Result<u64, String> {
        self.end_tick_ns
            .checked_sub(self.start_tick_ns)
            .ok_or_else(|| "resource usage interval ends before it starts".to_string())
    }
}

/// Daemon-owned evidence produced by the qualified supervisor. It reports
/// occupancy, not utilization and not an external invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUsageObservation {
    pub version: u32,
    pub operation_id: String,
    pub owner_incarnation: String,
    pub stable_resource_id: String,
    /// Opaque Lillux clock-incarnation identity. Accounting never interprets
    /// an operating-system boot or clock representation.
    pub clock_incarnation_digest: HexDigest,
    pub meter_contract_digest: HexDigest,
    pub clock_contract_digest: HexDigest,
    pub coverage: ResourceUsageCoverage,
    pub intervals: Vec<ResourceUsageInterval>,
}

impl ResourceUsageObservation {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != RESOURCE_USAGE_OBSERVATION_VERSION {
            return Err(format!(
                "unsupported resource usage version {}",
                self.version
            ));
        }
        for (name, value) in [
            ("operation_id", &self.operation_id),
            ("owner_incarnation", &self.owner_incarnation),
            ("stable_resource_id", &self.stable_resource_id),
        ] {
            validate_token(name, value)?;
        }
        if self.intervals.len() > MAX_INTERVALS
            || (self.intervals.is_empty() && self.coverage == ResourceUsageCoverage::Complete)
        {
            return Err("resource usage intervals are outside the bounded range".to_string());
        }
        let mut previous_end = None;
        for interval in &self.intervals {
            let _ = interval.duration_ns()?;
            if previous_end.is_some_and(|end| interval.start_tick_ns < end) {
                return Err("resource usage intervals overlap or are unsorted".to_string());
            }
            previous_end = Some(interval.end_tick_ns);
        }
        Ok(())
    }

    pub fn observed_nanoseconds(&self) -> Result<u64, String> {
        self.validate()?;
        self.intervals.iter().try_fold(0_u64, |total, interval| {
            total
                .checked_add(interval.duration_ns()?)
                .ok_or_else(|| "resource usage duration overflows".to_string())
        })
    }

    /// Validate this supervisor observation against the exact resource
    /// authority frozen before launch. Identity equality is explicit; a
    /// well-formed observation from another meter or resource is not usable.
    pub fn validate_against(&self, authority: &ResourceAccountingAuthority) -> Result<(), String> {
        self.validate()?;
        authority.validate()?;
        if self.stable_resource_id != authority.stable_resource_id {
            return Err("resource usage names another stable resource".to_string());
        }
        if self.meter_contract_digest != authority.meter.contract_digest {
            return Err("resource usage names another meter contract".to_string());
        }
        if self.clock_contract_digest != authority.meter.clock_contract_digest {
            return Err("resource usage names another clock contract".to_string());
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<HexDigest, String> {
        self.validate()?;
        HexDigest::of_canonical_json(
            &serde_json::to_value(self).map_err(|error| error.to_string())?,
        )
    }
}

/// Exact tariff application over one typed usage observation. This is rated
/// allocation/expenditure evidence; it is not resource-use evidence and does
/// not imply that an external provider invoice was observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRatedCharge {
    pub version: u32,
    pub usage_digest: HexDigest,
    pub authority_digest: HexDigest,
    pub tariff_digest: HexDigest,
    pub charge_class: ResourceChargeClass,
    pub coverage: ResourceUsageCoverage,
    pub observed_nanoseconds: u64,
    pub billable_milliseconds: u64,
    pub amount: UsdNanos,
}

impl ResourceRatedCharge {
    pub fn derive(
        authority: &ResourceAccountingAuthority,
        usage: &ResourceUsageObservation,
    ) -> Result<Self, String> {
        usage.validate_against(authority)?;
        let observed_nanoseconds = usage.observed_nanoseconds()?;
        let billable_milliseconds = authority
            .tariff
            .billable_milliseconds(observed_nanoseconds)?;
        let amount = authority
            .tariff
            .charge_for_nanoseconds(observed_nanoseconds)?;
        Ok(Self {
            version: RESOURCE_RATED_CHARGE_VERSION,
            usage_digest: usage.digest()?,
            authority_digest: authority.authority_digest.clone(),
            tariff_digest: authority.tariff.digest()?,
            charge_class: authority.tariff.charge_class,
            coverage: usage.coverage,
            observed_nanoseconds,
            billable_milliseconds,
            amount,
        })
    }

    pub fn validate(
        &self,
        authority: &ResourceAccountingAuthority,
        usage: &ResourceUsageObservation,
    ) -> Result<(), String> {
        if self.version != RESOURCE_RATED_CHARGE_VERSION {
            return Err(format!(
                "unsupported resource rated-charge version {}",
                self.version
            ));
        }
        let expected = Self::derive(authority, usage)?;
        if *self != expected {
            return Err("resource rated charge does not match its authority and usage".to_string());
        }
        Ok(())
    }
}

/// One exact active-request interval within a resident resource operation.
/// It is analytical attribution only: recording it never reserves or debits
/// money and never creates another resource operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequestAttribution {
    pub version: u32,
    pub attribution_id: HexDigest,
    pub operation_id: HexDigest,
    pub thread_id: String,
    pub request_digest: HexDigest,
    pub interval: ResourceUsageInterval,
}

impl ResourceRequestAttribution {
    pub fn compute_id(&self) -> Result<HexDigest, String> {
        let mut value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        value
            .as_object_mut()
            .ok_or_else(|| "resource attribution must encode as an object".to_string())?
            .remove("attribution_id");
        HexDigest::of_canonical_json(&value)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != RESOURCE_REQUEST_ATTRIBUTION_VERSION {
            return Err("unsupported resource request-attribution version".to_string());
        }
        validate_token("thread_id", &self.thread_id)?;
        let _ = self.interval.duration_ns()?;
        if self.compute_id()? != self.attribution_id {
            return Err("resource request attribution identity mismatch".to_string());
        }
        Ok(())
    }

    pub fn sealed(mut self) -> Result<Self, String> {
        self.attribution_id = self.compute_id()?;
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceAttributedShare {
    pub attribution_id: HexDigest,
    pub thread_id: String,
    pub request_digest: HexDigest,
    pub observed_nanoseconds: u64,
    pub allocated_usd_nanos: u64,
}

/// Conservative partition of one already-rated operation. Request shares use
/// floor-proportional allocation; every rounding remainder stays in explicit
/// scope overhead, so the rows conserve both time and the original charge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUsagePartition {
    pub version: u32,
    pub operation_id: HexDigest,
    pub usage_digest: HexDigest,
    /// Tariff result for the observed usage. This is evidence, not
    /// necessarily the amount committed by the financial ledger.
    pub rated_charge_usd_nanos: u64,
    /// Exact ledger debit partitioned across requests and owner overhead.
    /// Advisory operations carry zero here even when they have a rated cost.
    pub allocated_charge_usd_nanos: u64,
    pub attributed: Vec<ResourceAttributedShare>,
    pub overhead_nanoseconds: u64,
    pub overhead_usd_nanos: u64,
}

impl ResourceUsagePartition {
    pub fn derive(
        usage: &ResourceUsageObservation,
        charge: &ResourceRatedCharge,
        allocated_charge_usd_nanos: u64,
        attributions: &[ResourceRequestAttribution],
    ) -> Result<Self, String> {
        usage.validate()?;
        if attributions.len() > MAX_ATTRIBUTIONS {
            return Err("resource request attribution cardinality is unbounded".to_string());
        }
        if charge.usage_digest != usage.digest()? {
            return Err("resource partition charge names another usage observation".to_string());
        }
        let operation_id = HexDigest::new(usage.operation_id.clone())?;
        let total_ns = usage.observed_nanoseconds()?;
        let rated_money = u64::try_from(charge.amount.as_nanos())
            .map_err(|_| "resource charge is outside partition range".to_string())?;
        let total_money = allocated_charge_usd_nanos;
        let mut ordered = attributions.to_vec();
        ordered.sort_by(|left, right| {
            (
                left.interval.start_tick_ns,
                left.interval.end_tick_ns,
                left.attribution_id.as_str(),
            )
                .cmp(&(
                    right.interval.start_tick_ns,
                    right.interval.end_tick_ns,
                    right.attribution_id.as_str(),
                ))
        });
        let mut previous_end = None;
        let mut attributed_ns = 0_u64;
        let mut attributed = Vec::with_capacity(ordered.len());
        let covers = |interval: &ResourceUsageInterval| {
            usage.intervals.iter().any(|covered| {
                interval.start_tick_ns >= covered.start_tick_ns
                    && interval.end_tick_ns <= covered.end_tick_ns
            })
        };
        let mut attributed_money = 0_u64;
        for attribution in ordered {
            attribution.validate()?;
            if attribution.operation_id != operation_id || !covers(&attribution.interval) {
                return Err("resource attribution is outside the exact operation usage".to_string());
            }
            if previous_end.is_some_and(|end| attribution.interval.start_tick_ns < end) {
                return Err("resource request attributions overlap".to_string());
            }
            previous_end = Some(attribution.interval.end_tick_ns);
            let duration = attribution.interval.duration_ns()?;
            attributed_ns = attributed_ns
                .checked_add(duration)
                .ok_or_else(|| "resource attributed duration overflows".to_string())?;
            let share = if total_ns == 0 {
                0
            } else {
                u64::try_from(
                    (u128::from(total_money) * u128::from(duration)) / u128::from(total_ns),
                )
                .map_err(|_| "resource attributed money overflows".to_string())?
            };
            attributed_money = attributed_money
                .checked_add(share)
                .ok_or_else(|| "resource attributed money overflows".to_string())?;
            attributed.push(ResourceAttributedShare {
                attribution_id: attribution.attribution_id,
                thread_id: attribution.thread_id,
                request_digest: attribution.request_digest,
                observed_nanoseconds: duration,
                allocated_usd_nanos: share,
            });
        }
        let overhead_nanoseconds = total_ns
            .checked_sub(attributed_ns)
            .ok_or_else(|| "resource request attribution exceeds observed usage".to_string())?;
        let overhead_usd_nanos = total_money
            .checked_sub(attributed_money)
            .ok_or_else(|| "resource request attribution exceeds rated charge".to_string())?;
        Ok(Self {
            version: RESOURCE_USAGE_PARTITION_VERSION,
            operation_id,
            usage_digest: charge.usage_digest.clone(),
            rated_charge_usd_nanos: rated_money,
            allocated_charge_usd_nanos: total_money,
            attributed,
            overhead_nanoseconds,
            overhead_usd_nanos,
        })
    }
}

fn validate_token(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_TOKEN_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return Err(format!("{label} is not a bounded canonical token"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_budget_transition_matrix_is_closed_and_exact() {
        use ResourceBudgetState as S;
        let states = [
            S::ReservationDenied,
            S::Reserved,
            S::ReleasedUnissued,
            S::Issued,
            S::Reconciled,
            S::ChargedReservedMaximum,
            S::ReservationBoundViolated,
            S::AdvisoryPending,
            S::AdvisoryReleasedUnissued,
            S::AdvisoryIssued,
            S::AdvisoryReconciled,
        ];
        let allowed = [
            (S::Reserved, S::Issued),
            (S::Reserved, S::ReleasedUnissued),
            (S::Issued, S::Reconciled),
            (S::Issued, S::ChargedReservedMaximum),
            (S::Issued, S::ReservationBoundViolated),
            (S::ChargedReservedMaximum, S::Reconciled),
            (S::ChargedReservedMaximum, S::ReservationBoundViolated),
            (S::AdvisoryPending, S::AdvisoryIssued),
            (S::AdvisoryPending, S::AdvisoryReleasedUnissued),
            (S::AdvisoryIssued, S::AdvisoryIssued),
            (S::AdvisoryIssued, S::AdvisoryReconciled),
        ];
        for current in states {
            for next in states {
                assert_eq!(
                    current.may_transition_to(next),
                    allowed.contains(&(current, next)),
                    "unexpected transition matrix result: {} -> {}",
                    current.as_str(),
                    next.as_str()
                );
            }
        }
        for state in states {
            assert_eq!(
                state.is_terminal(),
                matches!(
                    state,
                    S::ReservationDenied
                        | S::ReleasedUnissued
                        | S::Reconciled
                        | S::ReservationBoundViolated
                        | S::AdvisoryReleasedUnissued
                        | S::AdvisoryReconciled
                ),
                "unexpected terminal classification: {}",
                state.as_str()
            );
        }
    }

    fn digest() -> HexDigest {
        HexDigest::new("a".repeat(64)).unwrap()
    }

    fn tariff() -> ResourceTariffDocument {
        ResourceTariffDocument {
            version: RESOURCE_TARIFF_VERSION,
            currency: Currency::Usd,
            pricing_generation: "local-a6000-2026-09".to_string(),
            charge_class: ResourceChargeClass::InternalAllocation,
            rate_per_million_milliseconds: UsdNanos::parse_canonical("0.5").unwrap(),
            billing_quantum_milliseconds: 1_000,
            minimum_billable_milliseconds: 1_000,
            expires_at_ms: None,
        }
    }

    #[test]
    fn tariff_rounds_duration_and_money_up_without_float() {
        let tariff = tariff();
        assert_eq!(tariff.billable_milliseconds(1).unwrap(), 1_000);
        assert_eq!(
            tariff
                .charge_for_nanoseconds(1)
                .unwrap()
                .to_canonical_string(),
            "0.0005"
        );
        assert_eq!(tariff.billable_milliseconds(1_000_000_001).unwrap(), 2_000);
    }

    #[test]
    fn bounded_authority_recomputes_its_maximum() {
        let meter = ResourceMeterContract {
            version: RESOURCE_METER_CONTRACT_VERSION,
            kind: ResourceMeterKind::OccupancyDuration,
            clock_contract_digest: digest(),
            contract_digest: digest(),
        }
        .sealed()
        .unwrap();
        let tariff = tariff();
        let maximum = tariff.charge_for_nanoseconds(60_000_000_000).unwrap();
        let authority = ResourceAccountingAuthority {
            version: RESOURCE_ACCOUNTING_AUTHORITY_VERSION,
            authority_digest: digest(),
            stable_resource_id: "gpu-0".to_string(),
            resource_class: "accelerator".to_string(),
            observation_contract_digest: digest(),
            meter,
            tariff,
            spend: ResourceSpendAuthority::Bounded {
                maximum_occupancy_milliseconds: 60_000,
                maximum,
            },
        }
        .sealed()
        .unwrap();
        authority.validate().unwrap();
    }

    #[test]
    fn usage_refuses_overlap_and_changed_clock_contract() {
        let mut usage = ResourceUsageObservation {
            version: RESOURCE_USAGE_OBSERVATION_VERSION,
            operation_id: "R-1".to_string(),
            owner_incarnation: "P-1".to_string(),
            stable_resource_id: "gpu-0".to_string(),
            clock_incarnation_digest: digest(),
            meter_contract_digest: digest(),
            clock_contract_digest: digest(),
            coverage: ResourceUsageCoverage::Complete,
            intervals: vec![
                ResourceUsageInterval {
                    start_tick_ns: 10,
                    end_tick_ns: 20,
                },
                ResourceUsageInterval {
                    start_tick_ns: 19,
                    end_tick_ns: 30,
                },
            ],
        };
        assert!(usage.validate().is_err());
        usage.intervals[1].start_tick_ns = 20;
        assert_eq!(usage.observed_nanoseconds().unwrap(), 20);

        let meter = ResourceMeterContract {
            version: RESOURCE_METER_CONTRACT_VERSION,
            kind: ResourceMeterKind::OccupancyDuration,
            clock_contract_digest: digest(),
            contract_digest: digest(),
        }
        .sealed()
        .unwrap();
        let authority = ResourceAccountingAuthority {
            version: RESOURCE_ACCOUNTING_AUTHORITY_VERSION,
            authority_digest: digest(),
            stable_resource_id: "gpu-0".to_string(),
            resource_class: "accelerator".to_string(),
            observation_contract_digest: digest(),
            meter,
            tariff: tariff(),
            spend: ResourceSpendAuthority::Advisory,
        }
        .sealed()
        .unwrap();
        usage.meter_contract_digest = authority.meter.contract_digest.clone();
        usage.clock_contract_digest = digest();
        assert!(usage.validate_against(&authority).is_ok());
        usage.clock_contract_digest = HexDigest::new("b".repeat(64)).unwrap();
        assert!(usage.validate_against(&authority).is_err());
    }

    #[test]
    fn rated_charge_binds_exact_usage_tariff_and_coverage() {
        let meter = ResourceMeterContract {
            version: RESOURCE_METER_CONTRACT_VERSION,
            kind: ResourceMeterKind::OccupancyDuration,
            clock_contract_digest: digest(),
            contract_digest: digest(),
        }
        .sealed()
        .unwrap();
        let authority = ResourceAccountingAuthority {
            version: RESOURCE_ACCOUNTING_AUTHORITY_VERSION,
            authority_digest: digest(),
            stable_resource_id: "gpu-0".to_string(),
            resource_class: "accelerator".to_string(),
            observation_contract_digest: digest(),
            meter,
            tariff: tariff(),
            spend: ResourceSpendAuthority::Advisory,
        }
        .sealed()
        .unwrap();
        let usage = ResourceUsageObservation {
            version: RESOURCE_USAGE_OBSERVATION_VERSION,
            operation_id: "R-1".to_string(),
            owner_incarnation: "P-1".to_string(),
            stable_resource_id: "gpu-0".to_string(),
            clock_incarnation_digest: digest(),
            meter_contract_digest: authority.meter.contract_digest.clone(),
            clock_contract_digest: authority.meter.clock_contract_digest.clone(),
            coverage: ResourceUsageCoverage::Complete,
            intervals: vec![ResourceUsageInterval {
                start_tick_ns: 10,
                end_tick_ns: 1_000_000_011,
            }],
        };
        let charge = ResourceRatedCharge::derive(&authority, &usage).unwrap();
        assert_eq!(charge.billable_milliseconds, 2_000);
        assert_eq!(charge.amount.to_canonical_string(), "0.001");
        charge.validate(&authority, &usage).unwrap();
    }

    #[test]
    fn request_partition_conserves_usage_and_one_original_charge() {
        let meter = ResourceMeterContract {
            version: RESOURCE_METER_CONTRACT_VERSION,
            kind: ResourceMeterKind::OccupancyDuration,
            clock_contract_digest: digest(),
            contract_digest: digest(),
        }
        .sealed()
        .unwrap();
        let authority = ResourceAccountingAuthority {
            version: RESOURCE_ACCOUNTING_AUTHORITY_VERSION,
            authority_digest: digest(),
            stable_resource_id: "gpu-0".to_string(),
            resource_class: "accelerator".to_string(),
            observation_contract_digest: digest(),
            meter,
            tariff: tariff(),
            spend: ResourceSpendAuthority::Advisory,
        }
        .sealed()
        .unwrap();
        let operation_id = HexDigest::new("b".repeat(64)).unwrap();
        let usage = ResourceUsageObservation {
            version: RESOURCE_USAGE_OBSERVATION_VERSION,
            operation_id: operation_id.as_str().to_owned(),
            owner_incarnation: "process-1".to_owned(),
            stable_resource_id: "gpu-0".to_owned(),
            clock_incarnation_digest: digest(),
            meter_contract_digest: authority.meter.contract_digest.clone(),
            clock_contract_digest: authority.meter.clock_contract_digest.clone(),
            coverage: ResourceUsageCoverage::Complete,
            intervals: vec![ResourceUsageInterval {
                start_tick_ns: 100,
                end_tick_ns: 1_000_000_100,
            }],
        };
        let charge = ResourceRatedCharge::derive(&authority, &usage).unwrap();
        let attribution = ResourceRequestAttribution {
            version: RESOURCE_REQUEST_ATTRIBUTION_VERSION,
            attribution_id: digest(),
            operation_id,
            thread_id: "T-one".to_owned(),
            request_digest: HexDigest::new("c".repeat(64)).unwrap(),
            interval: ResourceUsageInterval {
                start_tick_ns: 100_000_100,
                end_tick_ns: 700_000_100,
            },
        }
        .sealed()
        .unwrap();
        let partition = ResourceUsagePartition::derive(
            &usage,
            &charge,
            charge.amount.as_nanos() as u64,
            &[attribution],
        )
        .unwrap();
        assert_eq!(partition.attributed[0].observed_nanoseconds, 600_000_000);
        assert_eq!(partition.overhead_nanoseconds, 400_000_000);
        assert_eq!(
            partition.attributed[0].allocated_usd_nanos + partition.overhead_usd_nanos,
            partition.rated_charge_usd_nanos
        );
        assert_eq!(
            charge.amount.as_nanos() as u64,
            partition.allocated_charge_usd_nanos
        );
    }
}
