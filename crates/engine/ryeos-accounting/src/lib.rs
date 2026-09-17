//! Shared financial accounting authority for RyeOS provider budgets.
//!
//! One schema cut, no alternates: the types here are the only accepted
//! shapes for authoritative money (`UsdNanos`), spend-bound certificates,
//! the reservation state machine, the budget callback protocol, and the
//! `provider_attempt_budget_transition_v1` audit event. Presentation `f64`
//! values are one-way derived and never parse back into authority.

pub mod admission;
pub mod authority;
pub mod event;
pub mod money;
pub mod resource;
pub mod rpc;
pub mod state;

pub use admission::{
    AdmittedFinancialAuthority, FINANCIAL_AUTHORITY_KIND, SpendBoundClass,
    admit_financial_authority,
};
pub use authority::{
    BillableDimension, CREDENTIAL_BINDING_MAC_CONTRACT, ChargeReconciliationAuthority,
    ClosedBillableDimensionSet, Currency, FinalityContract, HexDigest,
    PROVIDER_CHARGE_CAP_SCHEMA_VERSION, ProviderAccountingAuthority, ProviderChargeCapContract,
    SPEND_TARIFF_SCHEMA_VERSION, SpendBoundAuthority, SpendBoundCertificate, SpendTariffDocument,
    credential_binding_digest,
};
pub use event::{
    PROVIDER_ATTEMPT_BUDGET_TRANSITION_VERSION, ProviderAttemptBudgetTransitionV1, transition_id,
};
pub use money::{MoneyError, NANOS_PER_USD, UsdNanos, reported_decimal_scale};
pub use resource::{
    RESOURCE_ACCOUNTING_AUTHORITY_VERSION, RESOURCE_BUDGET_TRANSITION_VERSION,
    RESOURCE_METER_CONTRACT_VERSION, RESOURCE_OPERATION_BINDING_VERSION,
    RESOURCE_RATED_CHARGE_VERSION, RESOURCE_REQUEST_ATTRIBUTION_VERSION, RESOURCE_TARIFF_VERSION,
    RESOURCE_USAGE_OBSERVATION_VERSION, RESOURCE_USAGE_PARTITION_VERSION,
    ResourceAccountingAuthority, ResourceAttributedShare, ResourceBudgetState,
    ResourceBudgetTransitionV1, ResourceChargeClass, ResourceMeterContract, ResourceMeterKind,
    ResourceOperationBinding, ResourceRatedCharge, ResourceRequestAttribution,
    ResourceSpendAuthority, ResourceTariffDocument, ResourceUsageCoverage, ResourceUsageInterval,
    ResourceUsageObservation, ResourceUsagePartition,
};
pub use rpc::{
    MAX_DIAGNOSTIC_LEN, MAX_RAW_DECIMAL_LEN, ProviderAttemptBudgetRecord, ProviderAttemptGetParams,
    ProviderAttemptLocalStreamControl, ProviderAttemptLocalStreamControlParams,
    ProviderAttemptLocalStreamEvent, ProviderAttemptLocalStreamEventKind,
    ProviderAttemptLocalStreamNextParams, ProviderAttemptLocalStreamNextResponse,
    ProviderAttemptLocalStreamStartParams, ProviderAttemptLocalStreamStartResponse,
    ProviderAttemptMarkIssuedParams, ProviderAttemptMarkIssuedResponse,
    ProviderAttemptPrepareParams, ProviderAttemptPrepareResponse,
    ProviderAttemptReleaseUnissuedParams, ProviderAttemptReleaseUnissuedResponse,
    ProviderAttemptSettleParams, ProviderAttemptSettleResponse, ProviderCallPublication,
    ProviderCallPublicationProof, ProviderRetryAdvance, ProviderRetryDecision, ProviderRetryReason,
    RUNTIME_PROVIDER_ATTEMPT_GET, RUNTIME_PROVIDER_ATTEMPT_LOCAL_STREAM_CONTROL,
    RUNTIME_PROVIDER_ATTEMPT_LOCAL_STREAM_NEXT, RUNTIME_PROVIDER_ATTEMPT_LOCAL_STREAM_START,
    RUNTIME_PROVIDER_ATTEMPT_MARK_ISSUED, RUNTIME_PROVIDER_ATTEMPT_PREPARE,
    RUNTIME_PROVIDER_ATTEMPT_RELEASE_UNISSUED, RUNTIME_PROVIDER_ATTEMPT_SETTLE, SpendAccounting,
    SpendBoundCommitments, TokenAccounting, UnitCount, VerifiedPreparedSpendBound,
};
pub use state::{
    AccountHealth, AttemptBudgetState, AuthorityHealth, ChargeBasis, ReconciliationReason,
};
