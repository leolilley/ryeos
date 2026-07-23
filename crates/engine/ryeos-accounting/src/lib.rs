//! Shared financial accounting authority for RyeOS provider budgets.
//!
//! One schema cut, no alternates: the types here are the only accepted
//! shapes for authoritative money (`UsdNanos`), spend-bound certificates,
//! the reservation state machine, the budget callback protocol, and the
//! `provider_attempt_budget_transition_v1` audit event. Presentation `f64`
//! values are one-way derived and never parse back into authority.

pub mod authority;
pub mod event;
pub mod money;
pub mod rpc;
pub mod state;

pub use authority::{
    BillableDimension, ChargeReconciliationAuthority, ClosedBillableDimensionSet, Currency,
    FinalityContract, HexDigest, ProviderAccountingAuthority, ProviderChargeCapContract,
    SpendBoundAuthority, SpendBoundCertificate, SpendTariffDocument,
    PROVIDER_CHARGE_CAP_SCHEMA_VERSION, SPEND_TARIFF_SCHEMA_VERSION,
};
pub use event::{
    transition_id, ProviderAttemptBudgetTransitionV1, PROVIDER_ATTEMPT_BUDGET_TRANSITION_VERSION,
};
pub use money::{MoneyError, UsdNanos, NANOS_PER_USD};
pub use rpc::{
    ProviderAttemptBudgetRecord, ProviderAttemptGetParams, ProviderAttemptMarkIssuedParams,
    ProviderAttemptMarkIssuedResponse, ProviderAttemptReleaseUnissuedParams,
    ProviderAttemptReleaseUnissuedResponse, ProviderAttemptReserveParams,
    ProviderAttemptReserveResponse, ProviderAttemptSettleParams, ProviderAttemptSettleResponse,
    SpendAccounting, SpendBoundCommitments, TokenAccounting, UnitCount,
    VerifiedPreparedSpendBound, MAX_DIAGNOSTIC_LEN, MAX_RAW_DECIMAL_LEN,
    RUNTIME_PROVIDER_ATTEMPT_GET, RUNTIME_PROVIDER_ATTEMPT_MARK_ISSUED,
    RUNTIME_PROVIDER_ATTEMPT_RELEASE_UNISSUED, RUNTIME_PROVIDER_ATTEMPT_RESERVE,
    RUNTIME_PROVIDER_ATTEMPT_SETTLE,
};
pub use state::{
    AccountHealth, AttemptBudgetState, AuthorityHealth, ChargeBasis, ReconciliationReason,
};
