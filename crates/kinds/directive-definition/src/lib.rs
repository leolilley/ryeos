//! Directive-kind model/provider schema and pure launch preparation.
//!
//! This crate deliberately has no executor, daemon, vault, thread, handler-runner,
//! filesystem, or environment dependency. The host supplies already-authorized,
//! resolved, trusted composed views and verified configuration snapshots. This
//! crate applies only directive-domain policy and returns opaque launch data,
//! symbolic secret requirements, and bounded audit facts.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use url::Url;

use ryeos_accounting::{
    BillableDimension, ChargeReconciliationAuthority, ClosedBillableDimensionSet, Currency,
    FinalityContract, HexDigest, ProviderAccountingAuthority, ProviderChargeCapContract,
    SpendBoundAuthority, SpendBoundCertificate, SpendTariffDocument, UsdNanos,
};

mod effective;

pub use effective::{
    ContinuationConfig, ContinuationEnabled, DIRECTIVE_EFFECTIVE_HEADER_KEYS, DirectiveEffectClass,
    DirectiveHeader, LimitsSpec, OutputSpec, ReturnNudge, parse_effective_header,
    resolve_external_effect_authority,
};

pub const MODEL_BINDING: &str = "model";
pub const MODEL_ROUTING_INPUT: &str = "model_routing";
pub const MODEL_PROVIDERS_INPUT: &str = "model_providers";
pub const EXECUTION_INPUT: &str = "execution";
pub const PROVIDER_SNAPSHOT_KEY: &str = "provider_snapshot";
pub const PROVIDER_CONFIG_PREFIX: &str = "ryeos-runtime/model-providers";

/// Shared default for `execution.max_provider_output_tokens_per_turn`.
/// Launch preparation and the runtime must derive the same effective output
/// bound from the same absent-field default.
pub const DEFAULT_MAX_PROVIDER_OUTPUT_TOKENS_PER_TURN: u64 = 32_768;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub sampling: Option<SamplingConfig>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingConfig {
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub seed: Option<u64>,
}

/// Provider-neutral reasoning policy authored on a signed model directive.
/// Absence preserves the provider descriptor's existing request shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReasoningConfig {
    pub mode: ReasoningMode,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReasoningMode {
    ProviderDefault,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoutingConfig {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tiers: BTreeMap<String, TierConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierConfig {
    pub provider: String,
    pub model: String,
    pub context_window: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default)]
    pub category: Option<String>,
    pub family: ProtocolFamily,
    pub transport: ProviderTransportConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub schemas: Option<SchemasConfig>,
    #[serde(default)]
    pub pricing: Option<PricingConfig>,
    /// Signed spend authority: billing subject identities plus the tariff or
    /// provider-cap contract a hard spend certificate can be derived from.
    /// Absent means the route is advisory-only (or explicitly free through
    /// `pricing.explicitly_free`).
    #[serde(default)]
    pub spend_authority: Option<SpendAuthorityConfig>,
    #[serde(default)]
    pub extra: HashMap<String, Value>,
    #[serde(default)]
    pub body_template: Option<Value>,
    #[serde(default)]
    pub body_extra: Option<Value>,
    #[serde(default)]
    pub profiles: Vec<ProviderProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    pub name: String,
    pub r#match: Vec<String>,
    #[serde(default)]
    pub family: Option<ProtocolFamily>,
    #[serde(default)]
    pub transport: Option<ProviderTransportConfig>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub schemas: Option<SchemasConfig>,
    #[serde(default)]
    pub spend_authority: Option<SpendAuthorityConfig>,
    #[serde(default)]
    pub extra: Option<HashMap<String, Value>>,
    #[serde(default)]
    pub body_template: Option<Value>,
    #[serde(default)]
    pub body_extra: Option<Value>,
}

/// Provider-domain transport selection. The remote and admitted-worker paths
/// are mechanically distinct and therefore form a closed enum; provider IDs,
/// URL spelling, and pricing never infer this choice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderTransportConfig {
    RemoteHttp {
        base_url: String,
    },
    AdmittedLocalWorker {
        execute: String,
        effect_class_ceiling: ryeos_effect_contract::EffectClass,
    },
}

impl ProviderTransportConfig {
    pub fn remote_base_url(&self) -> Option<&str> {
        match self {
            Self::RemoteHttp { base_url } => Some(base_url),
            Self::AdmittedLocalWorker { .. } => None,
        }
    }

    pub fn admitted_local_worker_ref(&self) -> Option<&str> {
        match self {
            Self::AdmittedLocalWorker { execute, .. } => Some(execute),
            Self::RemoteHttp { .. } => None,
        }
    }

    /// Maximum durable call semantics currently proven by this transport.
    /// Sealed local execution requires retained qualification evidence and
    /// cannot be minted by authored provider configuration alone.
    pub const fn effect_class_ceiling(&self) -> ryeos_effect_contract::EffectClass {
        match self {
            Self::RemoteHttp { .. } | Self::AdmittedLocalWorker { .. } => {
                ryeos_effect_contract::EffectClass::Recorded
            }
        }
    }

    fn validate(&self, context: &str) -> Result<()> {
        match self {
            Self::RemoteHttp { base_url } => {
                if base_url.trim().is_empty() {
                    bail!("provider config{context} has an empty remote base_url");
                }
                let probe = base_url.replace("{model}", "transport-probe");
                let parsed = Url::parse(&probe).map_err(|error| {
                    anyhow::anyhow!(
                        "provider config{context} has an invalid remote base_url: {error}"
                    )
                })?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.fragment().is_some()
                {
                    bail!("provider config{context} remote transport violates URL policy");
                }
            }
            Self::AdmittedLocalWorker {
                execute,
                effect_class_ceiling,
            } => {
                if execute.is_empty()
                    || execute.len() > 512
                    || !execute.contains(':')
                    || execute.bytes().any(|byte| {
                        byte.is_ascii_whitespace() || byte.is_ascii_control() || byte == b'\\'
                    })
                {
                    bail!("provider config{context} worker ref is not canonical");
                }
                if *effect_class_ceiling != ryeos_effect_contract::EffectClass::Recorded {
                    bail!(
                        "provider config{context} admitted worker cannot claim sealed effects without qualification evidence"
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default)]
    pub env_var: Option<String>,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProtocolFamily {
    ChatCompletions,
    AnthropicMessages,
    GoogleGenerateContent,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TextPlacement {
    #[default]
    String,
    PartsArray,
    BlocksArray,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AssistantToolCallsPlacement {
    #[default]
    TopLevelField,
    InlineBlocks,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolResultWrapMode {
    #[default]
    Direct,
    Parts,
    ContentBlocks,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SystemMessageMode {
    #[default]
    BodyField,
    BodyInject,
    MessageRole,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemasConfig {
    #[serde(default)]
    pub accounting: Option<ProviderAccountingConfig>,
    #[serde(default)]
    pub messages: Option<MessageSchemas>,
    #[serde(default)]
    pub tools: Option<ToolSchemaConfig>,
    #[serde(default)]
    pub streaming: Option<StreamingConfig>,
    #[serde(default)]
    pub output_limit: Option<OutputLimitConfig>,
    #[serde(default)]
    pub reasoning: Option<ReasoningSchemaConfig>,
}

/// Signed mapping from provider-neutral reasoning policy to provider-native
/// request fields. The runtime follows only these declared paths and values;
/// provider and model identities never influence request mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningSchemaConfig {
    #[serde(default)]
    pub mode: Option<ReasoningModeSchemaConfig>,
    #[serde(default)]
    pub effort: Option<ReasoningEffortSchemaConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningModeSchemaConfig {
    pub path: String,
    pub values: ReasoningModeValues,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningModeValues {
    pub enabled: Value,
    pub disabled: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningEffortSchemaConfig {
    pub path: String,
    pub values: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAccountingConfig {
    /// A completed response must contain structurally valid final token usage.
    /// This policy is independent from the paths/mode used to parse it.
    #[serde(default)]
    pub require_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputLimitConfig {
    /// Dot-separated path in the rendered request body.
    pub path: String,
    pub semantics: OutputLimitSemantics,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputLimitSemantics {
    ProviderNativeOutputTokens,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSchemas {
    #[serde(default)]
    pub role_map: Option<HashMap<String, String>>,
    #[serde(default)]
    pub content_key: Option<String>,
    #[serde(default)]
    pub text_placement: Option<TextPlacement>,
    #[serde(default)]
    pub assistant_tool_calls_placement: Option<AssistantToolCallsPlacement>,
    #[serde(default)]
    pub text_block_template: Option<Value>,
    #[serde(default)]
    pub tool_call_block_template: Option<Value>,
    #[serde(default)]
    pub system_message: Option<SystemMessageConfig>,
    #[serde(default)]
    pub tool_result: Option<ToolResultConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemMessageConfig {
    pub mode: SystemMessageMode,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub template: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultConfig {
    pub role: String,
    pub wrap_mode: ToolResultWrapMode,
    pub block_template: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSchemaConfig {
    pub template: Value,
    #[serde(default)]
    pub list_wrap: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingConfig {
    #[serde(default)]
    pub mode: Option<StreamingMode>,
    #[serde(default)]
    pub paths: Option<StreamPaths>,
    #[serde(default)]
    pub metadata: Option<StreamMetadataConfig>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamingMode {
    EventTyped,
    DeltaMerge,
    CompleteChunks,
}

impl StreamingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EventTyped => "event_typed",
            Self::DeltaMerge => "delta_merge",
            Self::CompleteChunks => "complete_chunks",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamMetadataConfig {
    #[serde(default)]
    pub usage: Option<StreamUsageConfig>,
    #[serde(default)]
    pub finish_reason_path: Option<String>,
    #[serde(default)]
    pub error: Option<StreamErrorConfig>,
    #[serde(default)]
    pub response_id_path: Option<String>,
    #[serde(default)]
    pub generation_id_header: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamUsageConfig {
    pub path: String,
    #[serde(default)]
    pub input_tokens_path: Option<String>,
    #[serde(default)]
    pub output_tokens_path: Option<String>,
    #[serde(default)]
    pub reasoning_tokens_path: Option<String>,
    #[serde(default)]
    pub cache_read_tokens_path: Option<String>,
    #[serde(default)]
    pub cache_miss_tokens_path: Option<String>,
    #[serde(default)]
    pub cache_write_tokens_path: Option<String>,
    /// When true, the signed protocol declares that cache-read and cache-miss
    /// tokens are a complete, non-overlapping partition of input tokens.
    #[serde(default)]
    pub cache_partition_of_input: bool,
    #[serde(default)]
    pub reported_cost_path: Option<String>,
    #[serde(default)]
    pub reported_cost_unit: Option<ReportedCostUnit>,
    #[serde(default)]
    pub cost_details_path: Option<String>,
    #[serde(default)]
    pub is_byok_path: Option<String>,
    #[serde(default)]
    pub reasoning_included_in_output: bool,
    #[serde(default)]
    pub aggregation: UsageAggregation,
    /// The signed protocol contract permits exactly one matching usage
    /// snapshot per response.
    #[serde(default)]
    pub single_snapshot: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReportedCostUnit {
    Usd,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum UsageAggregation {
    /// Provider frames contain cumulative totals. Each declared counter must
    /// be nondecreasing; regressions are contract violations.
    #[default]
    CumulativeFields,
    /// Each matching frame is a complete authoritative snapshot.
    LatestSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamErrorConfig {
    pub path: String,
    pub message_path: String,
    /// Raw finish-reason values that assert an error object must be present at
    /// `path`. This is a signed protocol contract, not a provider-name rule.
    #[serde(default)]
    pub finish_reasons: Vec<String>,
    #[serde(default)]
    pub code_path: Option<String>,
    #[serde(default)]
    pub metadata_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamPaths {
    pub content_path: String,
    pub text_field: String,
    #[serde(default)]
    pub thought_field: Option<String>,
    #[serde(default)]
    pub tool_call_field: Option<String>,
    #[serde(default)]
    pub tool_call_name_path: Option<String>,
    #[serde(default)]
    pub tool_call_args_path: Option<String>,
    #[serde(default)]
    pub usage_path: Option<String>,
    #[serde(default)]
    pub input_tokens_field: Option<String>,
    #[serde(default)]
    pub output_tokens_field: Option<String>,
    #[serde(default)]
    pub finish_reason_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingConfig {
    /// This route is intentionally zero-cost (for example, a local model).
    /// Without this declaration, zero fallback rates mean untracked pricing.
    #[serde(default)]
    pub explicitly_free: bool,
    /// Canonical USD decimal strings per million units. JSON/YAML numbers are
    /// rejected at this boundary: authoritative money never passes `f64`.
    #[serde(default)]
    pub input_per_million: Option<UsdNanos>,
    #[serde(default)]
    pub output_per_million: Option<UsdNanos>,
    #[serde(default)]
    pub cache_read_per_million: Option<UsdNanos>,
    #[serde(default)]
    pub cache_miss_per_million: Option<UsdNanos>,
    #[serde(default)]
    pub models: HashMap<String, ModelPricing>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_per_million: UsdNanos,
    pub output_per_million: UsdNanos,
    #[serde(default)]
    pub cache_read_per_million: Option<UsdNanos>,
    #[serde(default)]
    pub cache_miss_per_million: Option<UsdNanos>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSetupConfig {
    pub display_name: String,
    #[serde(default = "default_setup_priority")]
    pub priority: i32,
    #[serde(default)]
    pub recommended: bool,
    #[serde(default)]
    pub credential: Option<ProviderSetupCredential>,
    #[serde(default)]
    pub help_url: Option<String>,
    #[serde(default)]
    pub validation: Option<ProviderSetupValidation>,
    #[serde(default)]
    pub models: Vec<ProviderSetupModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSetupCredential {
    pub label: String,
    pub secret_name: String,
    pub input: ProviderSetupInput,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSetupInput {
    Secret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSetupValidation {
    pub r#ref: String,
    pub url: String,
    #[serde(default = "default_validation_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub may_incur_cost: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSetupModel {
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub recommended: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderSetupProjection {
    pub provider_id: String,
    pub display_name: String,
    pub priority: i32,
    pub recommended: bool,
    pub credential: Option<ProviderSetupCredential>,
    pub help_url: Option<String>,
    pub validation: Option<ProviderSetupValidation>,
    pub models: Vec<ProviderSetupModelProjection>,
}

#[derive(Debug, Clone)]
pub struct ProviderSetupModelProjection {
    pub name: String,
    pub display_name: String,
    pub context_window: Option<u64>,
    pub recommended: bool,
    pub pricing: Option<ModelPricing>,
}

fn default_setup_priority() -> i32 {
    100
}

fn default_validation_timeout_seconds() -> u64 {
    15
}

impl PricingConfig {
    pub fn for_model(&self, model_name: &str) -> Option<ModelPricing> {
        self.models.get(model_name).cloned().or_else(|| {
            Some(ModelPricing {
                input_per_million: self.input_per_million?,
                output_per_million: self.output_per_million?,
                cache_read_per_million: self.cache_read_per_million,
                cache_miss_per_million: self.cache_miss_per_million,
            })
        })
    }
}

/// Signed spend authority for a provider route: the stable billing subject
/// identities plus the contract a hard spend certificate is derived from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpendAuthorityConfig {
    /// Non-secret stable identity of the billing principal whose negotiated
    /// pricing applies (for example an org or key alias).
    pub billing_principal: String,
    /// Generation label of the credential authority. Operators advance it on
    /// rotation; an attempt freezes the generation it was admitted under.
    pub credential_authority_generation: String,
    /// Non-secret identity of the pricing contract subject.
    pub pricing_contract_subject: String,
    /// Signed tariff for `DerivedWorstCaseCharge` certificates.
    #[serde(default)]
    pub tariff: Option<SpendTariffDocument>,
    /// Server-enforced request charge cap for `ProviderEnforcedChargeCap`
    /// certificates. Preferred over the tariff when both are declared.
    #[serde(default)]
    pub request_charge_cap: Option<ProviderChargeCapContract>,
    /// Contract making the provider's reported final charge authoritative
    /// for settlement.
    #[serde(default)]
    pub reported_final_charge: Option<ReportedFinalChargeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedFinalChargeConfig {
    pub final_on_response: bool,
    pub max_reported_fraction_digits: u8,
    #[serde(default)]
    pub byok_zero_is_final: bool,
    /// Billable dimensions the reported final charge covers.
    pub covered_dimensions: ClosedBillableDimensionSet,
}

fn validate_subject_label(label: &str, value: &str) -> std::result::Result<(), String> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(format!(
            "spend_authority.{label} must be 1-256 bytes without control characters"
        ));
    }
    Ok(())
}

impl SpendAuthorityConfig {
    pub fn validate(&self, provider: &ProviderConfig) -> std::result::Result<(), String> {
        validate_subject_label("billing_principal", &self.billing_principal)?;
        validate_subject_label(
            "credential_authority_generation",
            &self.credential_authority_generation,
        )?;
        validate_subject_label("pricing_contract_subject", &self.pricing_contract_subject)?;
        if let Some(tariff) = &self.tariff {
            tariff.validate()?;
        }
        if let Some(cap) = &self.request_charge_cap {
            cap.validate()?;
        }
        if provider
            .pricing
            .as_ref()
            .is_some_and(|pricing| pricing.explicitly_free)
            && (self.tariff.is_some() || self.request_charge_cap.is_some())
        {
            return Err(
                "spend_authority tariff/cap contracts cannot be combined with \
                 pricing.explicitly_free"
                    .to_string(),
            );
        }
        if self.reported_final_charge.is_some() {
            let has_reported_cost_path = provider
                .schemas
                .as_ref()
                .and_then(|schemas| schemas.streaming.as_ref())
                .and_then(|streaming| streaming.metadata.as_ref())
                .and_then(|metadata| metadata.usage.as_ref())
                .is_some_and(|usage| usage.reported_cost_path.is_some());
            if !has_reported_cost_path {
                return Err(
                    "spend_authority.reported_final_charge requires streaming metadata \
                     usage.reported_cost_path"
                        .to_string(),
                );
            }
        }
        Ok(())
    }
}

/// Launch-time bounds the derived worst-case certificate is computed from.
#[derive(Debug, Clone, Copy)]
pub struct AccountingAuthorityInputs {
    pub context_window: u64,
    /// Effective provider-native output-token ceiling for one attempt.
    /// `0` means the runtime ceiling is disabled — no bounded output exists,
    /// so no derived certificate can be issued.
    pub max_provider_output_tokens_per_turn: u64,
}

fn subject_digest(value: &str) -> std::result::Result<HexDigest, DirectivePreparationError> {
    HexDigest::new(lillux::sha256_hex(value.as_bytes()))
        .map_err(|error| DirectivePreparationError::internal("accounting_authority_failed", error))
}

fn contract_digest_of<T: Serialize>(
    contract: &T,
) -> std::result::Result<HexDigest, DirectivePreparationError> {
    let value = serde_json::to_value(contract).map_err(|error| {
        DirectivePreparationError::internal("accounting_authority_failed", error.to_string())
    })?;
    HexDigest::of_canonical_json(&value)
        .map_err(|error| DirectivePreparationError::internal("accounting_authority_failed", error))
}

/// Bound one billable dimension to the mechanically valid launch-time unit
/// ceiling: prompt-side dimensions are bounded by the declared context
/// window the provider enforces; generation-side dimensions by the bounded
/// provider-native output ceiling of the prepared request.
fn dimension_bound(
    dim: BillableDimension,
    inputs: &AccountingAuthorityInputs,
) -> Option<(BillableDimension, u64)> {
    match dim {
        BillableDimension::InputTokens
        | BillableDimension::CacheReadTokens
        | BillableDimension::CacheMissTokens
        | BillableDimension::CacheWriteTokens => Some((dim, inputs.context_window)),
        BillableDimension::OutputTokens | BillableDimension::ReasoningTokens => {
            (inputs.max_provider_output_tokens_per_turn != 0)
                .then_some((dim, inputs.max_provider_output_tokens_per_turn))
        }
        BillableDimension::PerRequest => Some((dim, 1)),
    }
}

/// Resolve the sealed financial authority for one prepared route.
///
/// A raw signed maximum without a mechanically derivable proof produces
/// `AdvisoryOnly`, never hard eligibility. Explicitly-free routes bind their
/// zero contract to the exact signed config value digest.
pub fn resolve_accounting_authority(
    snapshot: &ResolvedProviderSnapshot,
    inputs: &AccountingAuthorityInputs,
) -> std::result::Result<ProviderAccountingAuthority, DirectivePreparationError> {
    let provider = &snapshot.provider;
    let spend_authority = provider.spend_authority.as_ref();
    let config_value_digest =
        HexDigest::new(snapshot.config_value_digest.clone()).map_err(|error| {
            DirectivePreparationError::internal("accounting_authority_failed", error)
        })?;

    let (billing_principal_digest, credential_authority_generation, pricing_subject_digest) =
        match spend_authority {
            Some(sa) => (
                subject_digest(&sa.billing_principal)?,
                sa.credential_authority_generation.clone(),
                subject_digest(&sa.pricing_contract_subject)?,
            ),
            None => (
                subject_digest("unscoped")?,
                "unscoped".to_string(),
                subject_digest("unscoped")?,
            ),
        };

    let explicitly_free = provider
        .pricing
        .as_ref()
        .is_some_and(|pricing| pricing.explicitly_free);

    let spend_bound = if explicitly_free {
        SpendBoundAuthority::ExplicitlyFree {
            contract_digest: config_value_digest.clone(),
        }
    } else if let Some(cap) = spend_authority.and_then(|sa| sa.request_charge_cap.as_ref()) {
        SpendBoundAuthority::Paid {
            maximum: cap.maximum,
            certificate: SpendBoundCertificate::ProviderEnforcedChargeCap {
                request_cap_contract_digest: contract_digest_of(cap)?,
                currency: Currency::Usd,
            },
        }
    } else if let Some(tariff) = spend_authority.and_then(|sa| sa.tariff.as_ref()) {
        let bounds: Option<Vec<(BillableDimension, u64)>> = tariff
            .covered_dimensions
            .as_slice()
            .iter()
            .map(|dim| dimension_bound(*dim, inputs))
            .collect();
        match bounds {
            None => SpendBoundAuthority::AdvisoryOnly,
            Some(bounds) => {
                let maximum = tariff.worst_case_charge(&bounds).map_err(|error| {
                    DirectivePreparationError::configuration(
                        "accounting_authority_invalid",
                        format!("tariff worst-case charge failed: {error}"),
                    )
                })?;
                if maximum.is_zero() {
                    SpendBoundAuthority::AdvisoryOnly
                } else {
                    let request_limit_digest = contract_digest_of(&serde_json::json!({
                        "context_window": inputs.context_window,
                        "max_provider_output_tokens_per_turn":
                            inputs.max_provider_output_tokens_per_turn,
                        "output_limit_path": provider
                            .schemas
                            .as_ref()
                            .and_then(|schemas| schemas.output_limit.as_ref())
                            .map(|limit| limit.path.clone()),
                    }))?;
                    SpendBoundAuthority::Paid {
                        maximum,
                        certificate: SpendBoundCertificate::DerivedWorstCaseCharge {
                            tariff_contract_digest: contract_digest_of(tariff)?,
                            request_limit_digest,
                            covered_dimensions: tariff.covered_dimensions.clone(),
                            currency: Currency::Usd,
                            pricing_generation: tariff.pricing_generation.clone(),
                            expires_at_ms: tariff.expires_at_ms,
                        },
                    }
                }
            }
        }
    } else {
        SpendBoundAuthority::AdvisoryOnly
    };

    let reconciliation =
        if let Some(rfc) = spend_authority.and_then(|sa| sa.reported_final_charge.as_ref()) {
            let usage_schema = provider
                .schemas
                .as_ref()
                .and_then(|schemas| schemas.streaming.as_ref())
                .and_then(|streaming| streaming.metadata.as_ref())
                .and_then(|metadata| metadata.usage.as_ref())
                .expect("validated: reported_final_charge requires usage schema");
            ChargeReconciliationAuthority::ProviderReportedFinalCharge {
                schema_digest: contract_digest_of(usage_schema)?,
                covered_dimensions: rfc.covered_dimensions.clone(),
                finality_contract: FinalityContract {
                    final_on_response: rfc.final_on_response,
                    max_reported_fraction_digits: rfc.max_reported_fraction_digits,
                    byok_zero_is_final: rfc.byok_zero_is_final,
                },
            }
        } else if let Some(tariff) = spend_authority.and_then(|sa| sa.tariff.as_ref()) {
            // Embed the complete signed tariff so daemon-side settlement is
            // self-contained in the sealed authority.
            ChargeReconciliationAuthority::DeterministicTariff {
                tariff: tariff.clone(),
            }
        } else {
            ChargeReconciliationAuthority::Unavailable
        };

    let placeholder_digest = subject_digest("unsealed")?;
    ProviderAccountingAuthority {
        authority_digest: placeholder_digest,
        config_hash: snapshot.config_hash.clone(),
        config_value_digest,
        billing_principal_digest,
        credential_authority_generation,
        pricing_contract_subject_digest: pricing_subject_digest,
        provider_id: snapshot.provider_id.clone(),
        model_name: snapshot.model_name.clone(),
        matched_profile: snapshot.matched_profile.clone(),
        spend_bound,
        reconciliation,
    }
    .sealed()
    .map_err(|error| DirectivePreparationError::internal("accounting_authority_failed", error))
}

impl ProviderConfig {
    pub fn setup_projection(&self, provider_id: &str) -> Result<ProviderSetupProjection> {
        let setup = match self.extra.get("setup") {
            Some(value) => {
                serde_json::from_value::<ProviderSetupConfig>(value.clone()).map_err(|error| {
                    anyhow::anyhow!("provider '{provider_id}' setup metadata is invalid: {error}")
                })?
            }
            None => {
                ProviderSetupConfig {
                    display_name: provider_id.to_string(),
                    priority: default_setup_priority(),
                    recommended: false,
                    credential: self.auth.env_var.as_ref().map(|secret_name| {
                        ProviderSetupCredential {
                            label: "Credential".to_string(),
                            secret_name: secret_name.clone(),
                            input: ProviderSetupInput::Secret,
                        }
                    }),
                    help_url: None,
                    validation: None,
                    models: self
                        .pricing
                        .as_ref()
                        .map(|pricing| {
                            let mut names = pricing.models.keys().cloned().collect::<Vec<_>>();
                            names.sort();
                            names
                                .into_iter()
                                .map(|name| ProviderSetupModel {
                                    name,
                                    display_name: None,
                                    context_window: None,
                                    recommended: false,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            }
        };
        if setup.display_name.trim().is_empty()
            || setup.display_name.len() > 160
            || setup.display_name.chars().any(char::is_control)
        {
            bail!("provider '{provider_id}' setup display_name is empty, unsafe, or too long");
        }
        if let Some(credential) = &setup.credential {
            if credential.label.trim().is_empty()
                || credential.label.len() > 160
                || credential.label.chars().any(char::is_control)
                || credential.secret_name.is_empty()
                || credential.secret_name.len() > 128
                || !credential
                    .secret_name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                bail!(
                    "provider '{provider_id}' setup credential fields are empty, unsafe, or too long"
                );
            }
            if self.auth.env_var.as_deref() != Some(credential.secret_name.as_str()) {
                bail!(
                    "provider '{provider_id}' setup secret_name '{}' does not exactly match runtime auth.env_var",
                    credential.secret_name
                );
            }
        }
        if setup.help_url.as_deref().is_some_and(|url| {
            url.len() > 4096 || !(url.starts_with("https://") || url.starts_with("http://"))
        }) {
            bail!("provider '{provider_id}' setup help_url is invalid");
        }
        if let Some(validation) = &setup.validation {
            if validation.r#ref.trim().is_empty()
                || validation.r#ref.len() > 512
                || validation.url.len() > 4096
                || validation.timeout_seconds == 0
                || !(validation.url.starts_with("https://")
                    || validation.url.starts_with("http://"))
            {
                bail!("provider '{provider_id}' setup validation is incomplete");
            }
            let base_url = self.transport.remote_base_url().ok_or_else(|| {
                anyhow::anyhow!(
                    "provider '{provider_id}' admitted worker cannot declare HTTP setup validation"
                )
            })?;
            validate_setup_endpoint(
                provider_id,
                base_url,
                validation,
                setup.credential.is_some(),
            )?;
        }
        let mut seen_models = std::collections::BTreeSet::new();
        let models = setup
            .models
            .into_iter()
            .map(|model| {
                if model.name.trim().is_empty()
                    || model.name.len() > 256
                    || model.name.chars().any(char::is_control)
                    || !seen_models.insert(model.name.clone())
                {
                    bail!("provider '{provider_id}' setup model names must be safe, bounded, and unique");
                }
                if model
                    .display_name
                    .as_deref()
                    .is_some_and(|value| {
                        value.trim().is_empty()
                            || value.len() > 160
                            || value.chars().any(char::is_control)
                    })
                {
                    bail!("provider '{provider_id}' setup model '{}' has an unsafe display_name", model.name);
                }
                if model.context_window == Some(0) {
                    bail!("provider '{provider_id}' setup model '{}' has zero context_window", model.name);
                }
                Ok(ProviderSetupModelProjection {
                    display_name: model
                        .display_name
                        .clone()
                        .unwrap_or_else(|| model.name.clone()),
                    pricing: self.pricing.as_ref().and_then(|pricing| pricing.for_model(&model.name)),
                    name: model.name,
                    context_window: model.context_window,
                    recommended: model.recommended,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        for model in &models {
            let effective = self.resolve_for_model(&model.name);
            match &setup.credential {
                Some(credential)
                    if effective.auth.env_var.as_deref()
                        != Some(credential.secret_name.as_str()) =>
                {
                    bail!(
                        "provider '{provider_id}' setup credential does not match model '{}' runtime auth",
                        model.name
                    );
                }
                None if effective.auth.env_var.is_some() => {
                    bail!(
                        "provider '{provider_id}' model '{}' requires a credential that setup does not declare",
                        model.name
                    );
                }
                _ => {}
            }
            if let Some(validation) = &setup.validation {
                let base_url = effective.transport.remote_base_url().ok_or_else(|| {
                    anyhow::anyhow!("provider '{provider_id}' admitted worker cannot declare HTTP setup validation")
                })?;
                validate_setup_endpoint(
                    provider_id,
                    base_url,
                    validation,
                    setup.credential.is_some(),
                )?;
            }
        }
        Ok(ProviderSetupProjection {
            provider_id: provider_id.to_string(),
            display_name: setup.display_name,
            priority: setup.priority,
            recommended: setup.recommended,
            credential: setup.credential,
            help_url: setup.help_url,
            validation: setup.validation,
            models,
        })
    }

    pub fn matched_profile(&self, model_name: &str) -> Option<&ProviderProfile> {
        self.profiles.iter().find(|profile| {
            profile
                .r#match
                .iter()
                .any(|pattern| glob_match(pattern, model_name))
        })
    }

    pub fn resolve_for_model(&self, model_name: &str) -> ProviderConfig {
        self.matched_profile(model_name)
            .map(|profile| self.merge_profile(profile))
            .unwrap_or_else(|| self.clone())
    }

    pub fn validate(&self, context: &str) -> Result<()> {
        self.transport.validate(context)?;
        if self.body_template.is_none() {
            bail!("provider config{context} has no body_template");
        }
        validate_body_template_placeholders(
            self.body_template
                .as_ref()
                .expect("body_template presence checked"),
            context,
        )?;
        if self.auth.env_var.is_some() != self.auth.header_name.is_some() {
            bail!(
                "provider config{context}: auth.env_var and auth.header_name must both be set or both be absent"
            );
        }
        if matches!(
            &self.transport,
            ProviderTransportConfig::AdmittedLocalWorker { .. }
        ) && (self.auth.env_var.is_some() || !self.headers.is_empty())
        {
            bail!(
                "provider config{context}: admitted worker transport cannot carry HTTP auth or headers"
            );
        }
        if self
            .auth
            .env_var
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
            || self
                .auth
                .header_name
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
        {
            bail!("provider config{context}: auth fields cannot be empty");
        }

        if let Some(messages) = self.schemas.as_ref().and_then(|s| s.messages.as_ref()) {
            if matches!(
                messages.text_placement,
                Some(TextPlacement::PartsArray | TextPlacement::BlocksArray)
            ) && messages.text_block_template.is_none()
            {
                bail!(
                    "provider config{context}: wrapped text placement requires messages.text_block_template"
                );
            }
            if messages.assistant_tool_calls_placement
                == Some(AssistantToolCallsPlacement::InlineBlocks)
                && messages.tool_call_block_template.is_none()
            {
                bail!(
                    "provider config{context}: inline tool calls require messages.tool_call_block_template"
                );
            }
            if let Some(system) = &messages.system_message {
                if system.mode == SystemMessageMode::BodyInject && system.template.is_none() {
                    bail!(
                        "provider config{context}: body_inject system messages require a template"
                    );
                }
                if system.mode == SystemMessageMode::BodyField && system.field.is_none() {
                    bail!("provider config{context}: body_field system messages require a field");
                }
            }
        }

        if let Some(metadata) = self
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.streaming.as_ref())
            .and_then(|streaming| streaming.metadata.as_ref())
        {
            let require_path = |label: &str, value: &str| -> Result<()> {
                if value.trim().is_empty() {
                    bail!("provider config{context}: streaming metadata {label} cannot be empty");
                }
                Ok(())
            };
            if let Some(usage) = metadata.usage.as_ref() {
                require_path("usage.path", &usage.path)?;
                if usage.input_tokens_path.is_none() || usage.output_tokens_path.is_none() {
                    bail!(
                        "provider config{context}: streaming metadata usage requires input_tokens_path and output_tokens_path"
                    );
                }
                if usage.reported_cost_path.is_some() != usage.reported_cost_unit.is_some() {
                    bail!(
                        "provider config{context}: streaming metadata usage reported_cost_path and reported_cost_unit must be declared together"
                    );
                }
                if usage.single_snapshot && usage.aggregation == UsageAggregation::CumulativeFields
                {
                    bail!(
                        "provider config{context}: streaming metadata usage.single_snapshot requires latest_snapshot aggregation"
                    );
                }
                if usage.cache_partition_of_input
                    && (usage.cache_read_tokens_path.is_none()
                        || usage.cache_miss_tokens_path.is_none())
                {
                    bail!(
                        "provider config{context}: streaming metadata usage.cache_partition_of_input requires cache_read_tokens_path and cache_miss_tokens_path"
                    );
                }
                for (label, path) in [
                    (
                        "usage.input_tokens_path",
                        usage.input_tokens_path.as_deref(),
                    ),
                    (
                        "usage.output_tokens_path",
                        usage.output_tokens_path.as_deref(),
                    ),
                    (
                        "usage.reasoning_tokens_path",
                        usage.reasoning_tokens_path.as_deref(),
                    ),
                    (
                        "usage.cache_read_tokens_path",
                        usage.cache_read_tokens_path.as_deref(),
                    ),
                    (
                        "usage.cache_miss_tokens_path",
                        usage.cache_miss_tokens_path.as_deref(),
                    ),
                    (
                        "usage.cache_write_tokens_path",
                        usage.cache_write_tokens_path.as_deref(),
                    ),
                    (
                        "usage.reported_cost_path",
                        usage.reported_cost_path.as_deref(),
                    ),
                    (
                        "usage.cost_details_path",
                        usage.cost_details_path.as_deref(),
                    ),
                    ("usage.is_byok_path", usage.is_byok_path.as_deref()),
                ] {
                    if let Some(path) = path {
                        require_path(label, path)?;
                    }
                }
            }
            for (label, path) in [
                ("finish_reason_path", metadata.finish_reason_path.as_deref()),
                ("response_id_path", metadata.response_id_path.as_deref()),
                (
                    "generation_id_header",
                    metadata.generation_id_header.as_deref(),
                ),
            ] {
                if let Some(path) = path {
                    require_path(label, path)?;
                }
            }
            if let Some(error) = metadata.error.as_ref() {
                require_path("error.path", &error.path)?;
                require_path("error.message_path", &error.message_path)?;
                if let Some(path) = error.code_path.as_deref() {
                    require_path("error.code_path", path)?;
                }
                if let Some(path) = error.metadata_path.as_deref() {
                    require_path("error.metadata_path", path)?;
                }
                let mut seen = std::collections::HashSet::new();
                for reason in &error.finish_reasons {
                    require_path("error.finish_reasons", reason)?;
                    if !seen.insert(reason.to_ascii_lowercase()) {
                        bail!(
                            "provider config{context}: streaming metadata error.finish_reasons must be unique"
                        );
                    }
                }
            }
        }

        let streaming = self
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.streaming.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "provider config{context}: schemas.streaming is required and must declare a mode"
                )
            })?;
        if streaming.mode.is_none() {
            bail!("provider config{context}: schemas.streaming.mode is required");
        }
        {
            match streaming.mode {
                Some(StreamingMode::EventTyped)
                    if self.family != ProtocolFamily::AnthropicMessages =>
                {
                    bail!(
                        "provider config{context}: streaming mode event_typed requires family anthropic_messages"
                    );
                }
                Some(StreamingMode::DeltaMerge)
                    if self.family != ProtocolFamily::ChatCompletions =>
                {
                    bail!(
                        "provider config{context}: streaming mode delta_merge requires family chat_completions"
                    );
                }
                Some(StreamingMode::CompleteChunks) => {
                    if self.family != ProtocolFamily::GoogleGenerateContent {
                        bail!(
                            "provider config{context}: streaming mode complete_chunks requires family google_generate_content"
                        );
                    }
                    if streaming.paths.is_none() {
                        bail!(
                            "provider config{context}: streaming mode complete_chunks requires streaming.paths"
                        );
                    }
                }
                _ => {}
            }
        }

        if self
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.output_limit.as_ref())
            .is_none()
        {
            bail!("provider config{context}: schemas.output_limit is required");
        }

        if self
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.accounting.as_ref())
            .is_some_and(|accounting| accounting.require_usage)
        {
            let streaming = self
                .schemas
                .as_ref()
                .and_then(|schemas| schemas.streaming.as_ref())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                    "provider config{context}: accounting.require_usage requires schemas.streaming"
                )
                })?;
            let metadata_usage = streaming
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.usage.as_ref())
                .is_some();
            let protocol_usage = match streaming.mode {
                Some(StreamingMode::EventTyped | StreamingMode::DeltaMerge) => true,
                Some(StreamingMode::CompleteChunks) => streaming
                    .paths
                    .as_ref()
                    .and_then(|paths| paths.usage_path.as_ref())
                    .is_some(),
                None => false,
            };
            if !metadata_usage && !protocol_usage {
                bail!(
                    "provider config{context}: accounting.require_usage needs a declared metadata usage schema or a streaming mode with a usage source"
                );
            }
        }

        if let Some(output_limit) = self
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.output_limit.as_ref())
            && output_limit
                .path
                .split('.')
                .any(|segment| segment.is_empty())
        {
            bail!(
                "provider config{context}: output_limit.path must contain non-empty dot-separated segments"
            );
        }

        if let Some(reasoning) = self
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.reasoning.as_ref())
        {
            validate_reasoning_schema(reasoning, context)?;
        }

        if let Some(pricing) = self.pricing.as_ref()
            && pricing.explicitly_free
            && (pricing
                .input_per_million
                .is_some_and(|rate| !rate.is_zero())
                || pricing
                    .output_per_million
                    .is_some_and(|rate| !rate.is_zero())
                || pricing
                    .cache_read_per_million
                    .is_some_and(|rate| !rate.is_zero())
                || pricing
                    .cache_miss_per_million
                    .is_some_and(|rate| !rate.is_zero())
                || !pricing.models.is_empty())
        {
            bail!(
                "provider config{context}: pricing.explicitly_free cannot be combined with non-zero/default or per-model prices"
            );
        }
        if let Some(pricing) = self.pricing.as_ref() {
            if pricing.cache_read_per_million.is_some() != pricing.cache_miss_per_million.is_some()
            {
                bail!(
                    "provider config{context}: default cache_read_per_million and cache_miss_per_million must be declared together"
                );
            }
            for (model, rates) in &pricing.models {
                if rates.cache_read_per_million.is_some() != rates.cache_miss_per_million.is_some()
                {
                    bail!(
                        "provider config{context}: pricing model {model} must declare cache_read_per_million and cache_miss_per_million together"
                    );
                }
            }
        }

        if let Some(spend_authority) = self.spend_authority.as_ref() {
            spend_authority
                .validate(self)
                .map_err(|error| anyhow::anyhow!("provider config{context}: {error}"))?;
        }

        match self.family {
            ProtocolFamily::AnthropicMessages => {
                if let Some(messages) = self.schemas.as_ref().and_then(|s| s.messages.as_ref())
                    && messages.assistant_tool_calls_placement
                        != Some(AssistantToolCallsPlacement::InlineBlocks)
                {
                    bail!(
                        "provider config{context}: anthropic_messages requires inline_blocks tool calls"
                    );
                }
            }
            ProtocolFamily::GoogleGenerateContent => {
                if let Some(messages) = self.schemas.as_ref().and_then(|s| s.messages.as_ref())
                    && messages.content_key.as_deref() != Some("parts")
                {
                    bail!(
                        "provider config{context}: google_generate_content requires messages.content_key=parts"
                    );
                }
            }
            ProtocolFamily::ChatCompletions => {}
        }

        Ok(())
    }

    fn merge_profile(&self, profile: &ProviderProfile) -> ProviderConfig {
        let mut resolved = self.clone();
        if let Some(family) = profile.family {
            resolved.family = family;
        }
        if let Some(transport) = &profile.transport {
            resolved.transport = transport.clone();
        }
        if let Some(auth) = &profile.auth {
            resolved.auth = auth.clone();
        }
        if let Some(headers) = &profile.headers {
            resolved.headers.extend(headers.clone());
        }
        if let Some(schemas) = &profile.schemas {
            resolved.schemas = Some(schemas.clone());
        }
        if let Some(spend_authority) = &profile.spend_authority {
            resolved.spend_authority = Some(spend_authority.clone());
        }
        if let Some(extra) = &profile.extra {
            resolved.extra.extend(extra.clone());
        }
        if let Some(body_template) = &profile.body_template {
            resolved.body_template = Some(body_template.clone());
        }
        if let Some(body_extra) = &profile.body_extra {
            match &mut resolved.body_extra {
                Some(existing) => deep_merge(existing, body_extra),
                None => resolved.body_extra = Some(body_extra.clone()),
            }
        }
        resolved.profiles.clear();
        resolved
    }
}

fn validate_setup_endpoint(
    provider_id: &str,
    base_url: &str,
    validation: &ProviderSetupValidation,
    sends_credential: bool,
) -> Result<()> {
    let validation_source = validation.url.replace("{model}", "setup-probe");
    let validation_url = Url::parse(&validation_source).map_err(|error| {
        anyhow::anyhow!("provider '{provider_id}' setup validation URL is invalid: {error}")
    })?;
    let base_source = base_url.replace("{model}", "setup-probe");
    let base = Url::parse(&base_source).map_err(|error| {
        anyhow::anyhow!("provider '{provider_id}' base_url is invalid: {error}")
    })?;
    if validation_url.username() != ""
        || validation_url.password().is_some()
        || base.username() != ""
        || base.password().is_some()
    {
        bail!("provider '{provider_id}' setup URLs cannot contain user information");
    }
    let same_origin = validation_url.scheme() == base.scheme()
        && validation_url.host_str() == base.host_str()
        && validation_url.port_or_known_default() == base.port_or_known_default();
    if !same_origin {
        bail!("provider '{provider_id}' validation URL must use the provider base_url origin");
    }
    if sends_credential && validation_url.scheme() != "https" {
        bail!("provider '{provider_id}' validation must use HTTPS when sending a credential");
    }
    if validation_url.scheme() == "http" {
        let loopback = matches!(
            validation_url.host_str(),
            Some("localhost" | "127.0.0.1" | "::1")
        );
        if !loopback {
            bail!("provider '{provider_id}' permits HTTP validation only for a loopback host");
        }
    }
    Ok(())
}

fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

fn validate_body_template_placeholders(template: &Value, context: &str) -> Result<()> {
    match template {
        Value::String(value) => {
            let trimmed = value.trim();
            if let Some(placeholder) = trimmed
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
                && !matches!(placeholder, "model" | "messages" | "tools" | "stream")
            {
                bail!(
                    "provider config{context}: body_template placeholder `{{{placeholder}}}` is not supported"
                );
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_body_template_placeholders(value, context)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_body_template_placeholders(value, context)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn glob_match(pattern: &str, candidate: &str) -> bool {
    match (pattern.strip_suffix('*'), pattern.strip_prefix('*')) {
        (Some(prefix), _) => candidate.starts_with(prefix),
        (_, Some(suffix)) => candidate.ends_with(suffix),
        _ => pattern == candidate,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotItemSpace {
    Bundle,
    Project,
    Node,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotTrustClass {
    TrustedBundle,
    TrustedProject,
    TrustedNode,
    UntrustedProject,
    Unsigned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfigSource {
    pub space: SnapshotItemSpace,
    pub root_label: String,
    pub canonical_id: String,
    pub content_digest: String,
    pub trust_class: SnapshotTrustClass,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedProviderSnapshot {
    pub provider_id: String,
    pub model_name: String,
    pub context_window: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub sampling: Option<SamplingConfig>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub matched_profile: Option<String>,
    pub config_value_digest: String,
    pub config_sources: Vec<ProviderConfigSource>,
    pub config_hash: String,
    pub provider: ProviderConfig,
}

fn deserialize_required_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl ResolvedProviderSnapshot {
    pub fn compute_hash(provider: &ProviderConfig) -> Result<String> {
        let value = serde_json::to_value(provider)?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&value)?.as_bytes(),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedConfigItem {
    pub value: Value,
    pub value_digest: String,
    pub contributors: Vec<ProviderConfigSource>,
}

#[derive(Debug, Clone)]
pub struct DirectiveLaunchPreparationInput<'a> {
    pub primary_ref: &'a str,
    pub primary_composed: &'a Value,
    pub model_ref: &'a str,
    pub model_composed: &'a Value,
    pub model_routing: Option<&'a VerifiedConfigItem>,
    pub provider_catalog: &'a BTreeMap<String, VerifiedConfigItem>,
    /// Verified `ryeos-runtime/execution` config item, when present. The
    /// derived worst-case spend bound freezes its provider-native output
    /// ceiling; absence uses the shared runtime default.
    pub execution: Option<&'a VerifiedConfigItem>,
}

#[derive(Debug, Clone)]
pub struct PreparedSecretRequirement {
    pub name: String,
    pub config_input: &'static str,
    pub canonical_id: String,
    pub value_digest: String,
}

#[derive(Debug, Clone)]
pub struct DirectiveLaunchPreparation {
    pub snapshot: ResolvedProviderSnapshot,
    pub required_secret: Option<PreparedSecretRequirement>,
    pub runtime_facts: BTreeMap<String, Value>,
    /// Sealed financial authority for the resolved route.
    pub accounting_authority: ProviderAccountingAuthority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectivePreparationErrorClass {
    Caller,
    Configuration,
    Internal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DirectiveDiagnosticScalar {
    Bool(bool),
    Integer(i64),
    String(String),
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct DirectivePreparationError {
    pub code: &'static str,
    pub message: String,
    pub classification: DirectivePreparationErrorClass,
    pub binding: Option<&'static str>,
    pub details: BTreeMap<String, DirectiveDiagnosticScalar>,
}

impl DirectivePreparationError {
    fn configuration(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            classification: DirectivePreparationErrorClass::Configuration,
            binding: Some(MODEL_BINDING),
            details: BTreeMap::new(),
        }
    }

    fn internal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            classification: DirectivePreparationErrorClass::Internal,
            binding: None,
            details: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct ResolvedTarget {
    provider_id: String,
    model_name: String,
    context_window: u64,
    sampling: Option<SamplingConfig>,
    reasoning: Option<ReasoningConfig>,
}

pub fn prepare_directive_launch(
    input: DirectiveLaunchPreparationInput<'_>,
) -> std::result::Result<DirectiveLaunchPreparation, DirectivePreparationError> {
    if input.primary_ref != input.model_ref
        && input
            .primary_composed
            .as_object()
            .is_some_and(|value| value.contains_key(MODEL_BINDING))
    {
        return Err(DirectivePreparationError::configuration(
            "primary_model_conflict",
            "a distinct primary behavior directive must not compose its own model block",
        ));
    }

    let model_value = input
        .model_composed
        .as_object()
        .and_then(|value| value.get(MODEL_BINDING))
        .ok_or_else(|| {
            DirectivePreparationError::configuration(
                "model_target_missing",
                "the bound model directive must contain a non-null model mapping",
            )
        })?;
    if model_value.is_null() || !model_value.is_object() {
        return Err(DirectivePreparationError::configuration(
            "model_target_invalid",
            "the bound model directive model value must be a non-null mapping",
        ));
    }

    let model: ModelSpec = serde_json::from_value(model_value.clone()).map_err(|error| {
        DirectivePreparationError::configuration(
            "model_target_invalid",
            format!("invalid bound model directive: {error}"),
        )
    })?;
    let target = resolve_target(&model, input.model_routing)?;
    validate_provider_id(&target.provider_id)?;

    let config_id = format!("{PROVIDER_CONFIG_PREFIX}/{}", target.provider_id);
    let provider_entry = input.provider_catalog.get(&config_id).ok_or_else(|| {
        DirectivePreparationError::configuration(
            "provider_config_missing",
            format!("provider config {config_id} is not present in the verified catalog"),
        )
    })?;
    validate_digest("provider config value", &provider_entry.value_digest)?;
    if provider_entry.contributors.is_empty() {
        return Err(DirectivePreparationError::internal(
            "provider_provenance_missing",
            format!("provider config {config_id} has no contributor provenance"),
        ));
    }
    for source in &provider_entry.contributors {
        if source.canonical_id != config_id {
            return Err(DirectivePreparationError::internal(
                "provider_provenance_mismatch",
                format!(
                    "provider config {config_id} has contributor for {}",
                    source.canonical_id
                ),
            ));
        }
        validate_digest("provider config content", &source.content_digest)?;
    }

    let provider: ProviderConfig =
        serde_json::from_value(provider_entry.value.clone()).map_err(|error| {
            DirectivePreparationError::configuration(
                "provider_config_invalid",
                format!("invalid provider config {config_id}: {error}"),
            )
        })?;
    let matched_profile = provider
        .matched_profile(&target.model_name)
        .map(|profile| profile.name.clone());
    if let Some(profile) = &matched_profile
        && (profile.trim().is_empty()
            || profile.len() > 128
            || profile.chars().any(char::is_control))
    {
        return Err(DirectivePreparationError::configuration(
            "provider_config_invalid",
            "the matched provider profile name must be 1-128 bytes without control characters",
        ));
    }
    let resolved_provider = provider.resolve_for_model(&target.model_name);
    resolved_provider
        .validate(&format!(
            " for model {} (provider {})",
            target.model_name, target.provider_id
        ))
        .map_err(|error| {
            DirectivePreparationError::configuration("provider_config_invalid", error.to_string())
        })?;
    validate_reasoning_selection(
        target.reasoning.as_ref(),
        resolved_provider
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.reasoning.as_ref()),
    )?;
    let config_hash =
        ResolvedProviderSnapshot::compute_hash(&resolved_provider).map_err(|error| {
            DirectivePreparationError::internal(
                "provider_config_hash_failed",
                format!("could not hash resolved provider config: {error}"),
            )
        })?;

    let required_secret =
        resolved_provider
            .auth
            .env_var
            .as_ref()
            .map(|name| PreparedSecretRequirement {
                name: name.clone(),
                config_input: MODEL_PROVIDERS_INPUT,
                canonical_id: config_id.clone(),
                value_digest: provider_entry.value_digest.clone(),
            });

    let snapshot = ResolvedProviderSnapshot {
        provider_id: target.provider_id,
        model_name: target.model_name,
        context_window: target.context_window,
        sampling: target.sampling,
        reasoning: target.reasoning,
        matched_profile,
        config_value_digest: provider_entry.value_digest.clone(),
        config_sources: provider_entry.contributors.clone(),
        config_hash,
        provider: resolved_provider,
    };
    let accounting_inputs = AccountingAuthorityInputs {
        context_window: snapshot.context_window,
        max_provider_output_tokens_per_turn: resolve_output_ceiling(input.execution)?,
    };
    let accounting_authority = resolve_accounting_authority(&snapshot, &accounting_inputs)?;
    let runtime_facts = runtime_facts(&snapshot, &accounting_authority)?;

    Ok(DirectiveLaunchPreparation {
        snapshot,
        required_secret,
        runtime_facts,
        accounting_authority,
    })
}

/// Extract the provider-native output ceiling from the verified execution
/// config item, applying the shared default when the item or field is
/// absent. A present field with a non-integer shape is a configuration
/// error, never a silent default.
fn resolve_output_ceiling(
    execution: Option<&VerifiedConfigItem>,
) -> std::result::Result<u64, DirectivePreparationError> {
    let Some(item) = execution else {
        return Ok(DEFAULT_MAX_PROVIDER_OUTPUT_TOKENS_PER_TURN);
    };
    let Some(object) = item.value.as_object() else {
        return Err(DirectivePreparationError::configuration(
            "execution_config_invalid",
            "the execution config input must be a mapping",
        ));
    };
    match object.get("max_provider_output_tokens_per_turn") {
        None => Ok(DEFAULT_MAX_PROVIDER_OUTPUT_TOKENS_PER_TURN),
        Some(value) => value.as_u64().ok_or_else(|| {
            DirectivePreparationError::configuration(
                "execution_config_invalid",
                "execution.max_provider_output_tokens_per_turn must be an unsigned integer",
            )
        }),
    }
}

fn resolve_target(
    model: &ModelSpec,
    routing: Option<&VerifiedConfigItem>,
) -> std::result::Result<ResolvedTarget, DirectivePreparationError> {
    let has_tier = model.tier.is_some();
    let explicit_count = [
        model.provider.is_some(),
        model.name.is_some(),
        model.context_window.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();

    if has_tier && explicit_count != 0 {
        return Err(DirectivePreparationError::configuration(
            "model_target_mixed",
            "model.tier is mutually exclusive with provider, name, and context_window",
        ));
    }
    if !has_tier && explicit_count == 0 {
        return Err(DirectivePreparationError::configuration(
            "model_target_missing",
            "model must declare either a tier or a complete provider/name/context_window target",
        ));
    }
    if !has_tier && explicit_count != 3 {
        return Err(DirectivePreparationError::configuration(
            "model_target_partial",
            "an explicit model target requires provider, name, and context_window together",
        ));
    }

    if let Some(tier) = &model.tier {
        if tier.trim().is_empty() {
            return Err(DirectivePreparationError::configuration(
                "model_tier_invalid",
                "model.tier cannot be empty",
            ));
        }
        let routing = routing.ok_or_else(|| {
            DirectivePreparationError::configuration(
                "model_routing_missing",
                format!("model tier {tier} requires the model_routing config input"),
            )
        })?;
        let routing: ModelRoutingConfig =
            serde_json::from_value(routing.value.clone()).map_err(|error| {
                DirectivePreparationError::configuration(
                    "model_routing_invalid",
                    format!("invalid model_routing config: {error}"),
                )
            })?;
        let selected = routing.tiers.get(tier).ok_or_else(|| {
            DirectivePreparationError::configuration(
                "model_tier_not_found",
                format!("model_routing does not declare tier {tier}"),
            )
        })?;
        validate_nonempty("tier provider", &selected.provider)?;
        validate_model_name("tier model", &selected.model)?;
        validate_context_window(selected.context_window)?;
        return Ok(ResolvedTarget {
            provider_id: selected.provider.clone(),
            model_name: selected.model.clone(),
            context_window: selected.context_window,
            sampling: model.sampling.clone(),
            reasoning: model.reasoning.clone(),
        });
    }

    let provider_id = model.provider.clone().expect("explicit target checked");
    let model_name = model.name.clone().expect("explicit target checked");
    let context_window = model.context_window.expect("explicit target checked");
    validate_nonempty("model.provider", &provider_id)?;
    validate_model_name("model.name", &model_name)?;
    validate_context_window(context_window)?;
    Ok(ResolvedTarget {
        provider_id,
        model_name,
        context_window,
        sampling: model.sampling.clone(),
        reasoning: model.reasoning.clone(),
    })
}

fn validate_provider_id(value: &str) -> std::result::Result<(), DirectivePreparationError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => true,
            b'.' | b'_' | b'-' => index != 0,
            _ => false,
        })
    {
        return Err(DirectivePreparationError::configuration(
            "provider_id_invalid",
            format!("provider id {value:?} is not a valid config identity segment"),
        ));
    }
    Ok(())
}

fn validate_nonempty(
    field: &str,
    value: &str,
) -> std::result::Result<(), DirectivePreparationError> {
    if value.trim().is_empty() {
        return Err(DirectivePreparationError::configuration(
            "model_target_invalid",
            format!("{field} cannot be empty"),
        ));
    }
    Ok(())
}

fn validate_model_name(
    field: &str,
    value: &str,
) -> std::result::Result<(), DirectivePreparationError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(DirectivePreparationError::configuration(
            "model_target_invalid",
            format!("{field} must be 1-256 bytes without control characters"),
        ));
    }
    Ok(())
}

fn validate_reasoning_selection(
    reasoning: Option<&ReasoningConfig>,
    schema: Option<&ReasoningSchemaConfig>,
) -> std::result::Result<(), DirectivePreparationError> {
    let Some(reasoning) = reasoning else {
        return Ok(());
    };
    if reasoning.mode == ReasoningMode::Disabled && reasoning.effort.is_some() {
        return Err(DirectivePreparationError::configuration(
            "model_reasoning_invalid",
            "model.reasoning.effort cannot be combined with disabled reasoning",
        ));
    }
    if reasoning.mode != ReasoningMode::ProviderDefault
        && schema.and_then(|schema| schema.mode.as_ref()).is_none()
    {
        return Err(DirectivePreparationError::configuration(
            "model_reasoning_unsupported",
            "the selected provider route does not declare a reasoning mode mapping",
        ));
    }
    if let Some(effort) = reasoning.effort.as_deref() {
        validate_reasoning_effort_name(effort).map_err(|message| {
            DirectivePreparationError::configuration("model_reasoning_invalid", message)
        })?;
        let supported = schema
            .and_then(|schema| schema.effort.as_ref())
            .is_some_and(|config| config.values.contains_key(effort));
        if !supported {
            return Err(DirectivePreparationError::configuration(
                "model_reasoning_unsupported",
                format!("the selected provider route does not declare reasoning effort {effort:?}"),
            ));
        }
    }
    Ok(())
}

fn validate_reasoning_schema(config: &ReasoningSchemaConfig, context: &str) -> Result<()> {
    if config.mode.is_none() && config.effort.is_none() {
        bail!("provider config{context}: schemas.reasoning must declare mode or effort mapping");
    }
    if let Some(mode) = config.mode.as_ref() {
        validate_reasoning_path("reasoning.mode.path", &mode.path, context)?;
        validate_reasoning_wire_value(
            "reasoning.mode.values.enabled",
            &mode.values.enabled,
            context,
        )?;
        validate_reasoning_wire_value(
            "reasoning.mode.values.disabled",
            &mode.values.disabled,
            context,
        )?;
        if mode.values.enabled == mode.values.disabled {
            bail!(
                "provider config{context}: reasoning enabled and disabled wire values must differ"
            );
        }
    }
    if let Some(effort) = config.effort.as_ref() {
        validate_reasoning_path("reasoning.effort.path", &effort.path, context)?;
        if effort.values.is_empty() || effort.values.len() > 32 {
            bail!("provider config{context}: reasoning.effort.values must contain 1-32 entries");
        }
        for (name, value) in &effort.values {
            validate_reasoning_effort_name(name).map_err(|message| {
                anyhow::anyhow!("provider config{context}: reasoning effort key: {message}")
            })?;
            validate_reasoning_wire_value("reasoning.effort.values", value, context)?;
        }
    }
    if let (Some(mode), Some(effort)) = (config.mode.as_ref(), config.effort.as_ref())
        && object_paths_conflict(&mode.path, &effort.path)
    {
        bail!(
            "provider config{context}: reasoning mode and effort paths must not equal or contain one another"
        );
    }
    Ok(())
}

fn validate_reasoning_path(label: &str, path: &str, context: &str) -> Result<()> {
    let segments = path.split('.').collect::<Vec<_>>();
    if path.is_empty()
        || path.len() > 256
        || segments.len() > 16
        || segments.iter().any(|segment| {
            segment.is_empty()
                || segment.len() > 64
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        bail!("provider config{context}: {label} must be a bounded dot-separated object path");
    }
    Ok(())
}

fn validate_reasoning_effort_name(value: &str) -> std::result::Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        return Err(
            "reasoning effort must be 1-64 lowercase ASCII letters, digits, '_' or '-'".to_string(),
        );
    }
    Ok(())
}

fn validate_reasoning_wire_value(label: &str, value: &Value, context: &str) -> Result<()> {
    if !matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_)) {
        bail!("provider config{context}: {label} must be a non-null JSON scalar");
    }
    if value
        .as_str()
        .is_some_and(|value| value.len() > 128 || value.chars().any(char::is_control))
    {
        bail!(
            "provider config{context}: {label} string must be at most 128 bytes without control characters"
        );
    }
    Ok(())
}

fn object_paths_conflict(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('.'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn validate_context_window(value: u64) -> std::result::Result<(), DirectivePreparationError> {
    if value == 0 || value > i64::MAX as u64 {
        return Err(DirectivePreparationError::configuration(
            "model_context_window_invalid",
            "model.context_window must be between 1 and i64::MAX",
        ));
    }
    Ok(())
}

fn validate_digest(label: &str, value: &str) -> std::result::Result<(), DirectivePreparationError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(DirectivePreparationError::internal(
            "provider_provenance_invalid",
            format!("{label} digest is not lowercase SHA-256 hex"),
        ));
    }
    Ok(())
}

fn runtime_facts(
    snapshot: &ResolvedProviderSnapshot,
    accounting_authority: &ProviderAccountingAuthority,
) -> std::result::Result<BTreeMap<String, Value>, DirectivePreparationError> {
    let mut facts = BTreeMap::new();
    facts.insert(
        "authority_digest".to_string(),
        Value::String(accounting_authority.authority_digest.as_str().to_string()),
    );
    facts.insert(
        "spend_bound".to_string(),
        Value::String(
            match &accounting_authority.spend_bound {
                SpendBoundAuthority::Paid { .. } => "paid",
                SpendBoundAuthority::ExplicitlyFree { .. } => "explicitly_free",
                SpendBoundAuthority::AdvisoryOnly => "advisory_only",
            }
            .to_string(),
        ),
    );
    facts.insert(
        "provider_id".to_string(),
        Value::String(snapshot.provider_id.clone()),
    );
    facts.insert(
        "model_name".to_string(),
        Value::String(snapshot.model_name.clone()),
    );
    facts.insert(
        "context_window".to_string(),
        Value::Number(snapshot.context_window.into()),
    );
    facts.insert(
        "sampling".to_string(),
        serde_json::to_value(&snapshot.sampling).map_err(|error| {
            DirectivePreparationError::internal(
                "runtime_facts_failed",
                format!("could not serialize sampling facts: {error}"),
            )
        })?,
    );
    facts.insert(
        "reasoning".to_string(),
        serde_json::to_value(&snapshot.reasoning).map_err(|error| {
            DirectivePreparationError::internal(
                "runtime_facts_failed",
                format!("could not serialize reasoning facts: {error}"),
            )
        })?,
    );
    if let Some(profile) = &snapshot.matched_profile {
        facts.insert(
            "matched_profile".to_string(),
            Value::String(profile.clone()),
        );
    }
    facts.insert(
        "config_hash".to_string(),
        Value::String(snapshot.config_hash.clone()),
    );
    facts.insert(
        "config_value_digest".to_string(),
        Value::String(snapshot.config_value_digest.clone()),
    );
    facts.insert(
        "config_sources".to_string(),
        Value::Array(
            snapshot
                .config_sources
                .iter()
                .map(|source| Value::String(source.root_label.clone()))
                .collect(),
        ),
    );
    Ok(facts)
}

#[cfg(test)]
mod reasoning_tests {
    use super::*;

    fn snapshot(reasoning: Option<ReasoningConfig>) -> ResolvedProviderSnapshot {
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "https://example.invalid".to_string(),
            },
            auth: AuthConfig::default(),
            headers: HashMap::new(),
            schemas: None,
            pricing: Some(PricingConfig {
                explicitly_free: true,
                input_per_million: None,
                output_per_million: None,
                cache_read_per_million: None,
                cache_miss_per_million: None,
                models: HashMap::new(),
            }),
            spend_authority: None,
            extra: HashMap::new(),
            body_template: None,
            body_extra: None,
            profiles: Vec::new(),
        };
        ResolvedProviderSnapshot {
            provider_id: "fixture".to_string(),
            model_name: "fixture-model".to_string(),
            context_window: 16_384,
            sampling: None,
            reasoning,
            matched_profile: None,
            config_value_digest: "0".repeat(64),
            config_sources: vec![ProviderConfigSource {
                space: SnapshotItemSpace::Bundle,
                root_label: "bundle:fixture".to_string(),
                canonical_id: "ryeos-runtime/model-providers/fixture".to_string(),
                content_digest: "1".repeat(64),
                trust_class: SnapshotTrustClass::TrustedBundle,
            }],
            config_hash: ResolvedProviderSnapshot::compute_hash(&provider).expect("provider hash"),
            provider,
        }
    }

    fn launch_provider() -> ProviderConfig {
        serde_json::from_value(serde_json::json!({
            "family": "chat_completions",
            "transport": {"kind": "remote_http", "base_url": "https://example.invalid"},
            "schemas": {
                "streaming": {"mode": "delta_merge"},
                "output_limit": {
                    "path": "max_tokens",
                    "semantics": "provider_native_output_tokens"
                },
                "reasoning": {
                    "mode": {
                        "path": "thinking.type",
                        "values": {"enabled": "on", "disabled": "off"}
                    },
                    "effort": {
                        "path": "reasoning_effort",
                        "values": {"high": "high", "max": "max"}
                    }
                }
            },
            "pricing": {"explicitly_free": true},
            "body_template": {
                "model": "{model}",
                "messages": "{messages}",
                "stream": "{stream}"
            }
        }))
        .expect("launch provider fixture")
    }

    fn schema() -> ReasoningSchemaConfig {
        ReasoningSchemaConfig {
            mode: Some(ReasoningModeSchemaConfig {
                path: "thinking.type".to_string(),
                values: ReasoningModeValues {
                    enabled: Value::String("on".to_string()),
                    disabled: Value::String("off".to_string()),
                },
            }),
            effort: Some(ReasoningEffortSchemaConfig {
                path: "reasoning_effort".to_string(),
                values: BTreeMap::from([
                    ("high".to_string(), Value::String("high".to_string())),
                    ("max".to_string(), Value::String("max".to_string())),
                ]),
            }),
        }
    }

    #[test]
    fn absent_reasoning_policy_preserves_provider_default_without_schema() {
        validate_reasoning_selection(None, None).expect("absent policy must remain compatible");
        validate_reasoning_selection(
            Some(&ReasoningConfig {
                mode: ReasoningMode::ProviderDefault,
                effort: None,
            }),
            None,
        )
        .expect("explicit provider_default must not require a provider mapping");
    }

    #[test]
    fn explicit_reasoning_mode_requires_a_signed_provider_mapping() {
        let error = validate_reasoning_selection(
            Some(&ReasoningConfig {
                mode: ReasoningMode::Disabled,
                effort: None,
            }),
            None,
        )
        .expect_err("unmapped reasoning mode must fail before provider execution");
        assert_eq!(error.code, "model_reasoning_unsupported");
        assert_eq!(
            error.classification,
            DirectivePreparationErrorClass::Configuration
        );
        assert_eq!(error.binding, Some(MODEL_BINDING));
    }

    #[test]
    fn disabled_reasoning_rejects_effort_even_when_both_are_mapped() {
        let error = validate_reasoning_selection(
            Some(&ReasoningConfig {
                mode: ReasoningMode::Disabled,
                effort: Some("high".to_string()),
            }),
            Some(&schema()),
        )
        .expect_err("disabled reasoning plus effort is contradictory");
        assert_eq!(error.code, "model_reasoning_invalid");
    }

    #[test]
    fn unsupported_effort_fails_closed_at_launch_preparation() {
        let error = validate_reasoning_selection(
            Some(&ReasoningConfig {
                mode: ReasoningMode::Enabled,
                effort: Some("low".to_string()),
            }),
            Some(&schema()),
        )
        .expect_err("unmapped effort must fail before provider execution");
        assert_eq!(error.code, "model_reasoning_unsupported");
        assert!(error.message.contains("low"));
    }

    #[test]
    fn provider_reasoning_schema_rejects_conflicting_paths() {
        let mut schema = schema();
        schema.effort.as_mut().expect("effort schema").path = "thinking".to_string();
        let error = validate_reasoning_schema(&schema, " test").expect_err("path conflict");
        assert!(error.to_string().contains("must not equal or contain"));
    }

    #[test]
    fn sealed_provider_snapshot_requires_an_explicit_reasoning_field() {
        let mut value = serde_json::to_value(snapshot(None)).expect("serialize snapshot");
        value
            .as_object_mut()
            .expect("snapshot object")
            .remove("reasoning");
        let error = serde_json::from_value::<ResolvedProviderSnapshot>(value)
            .expect_err("old snapshot shape must fail closed");
        assert!(error.to_string().contains("reasoning"));
    }

    #[test]
    fn reasoning_policy_is_recorded_as_a_sealed_launch_fact() {
        let expected = ReasoningConfig {
            mode: ReasoningMode::Disabled,
            effort: None,
        };
        let snapshot = snapshot(Some(expected.clone()));
        let authority = resolve_accounting_authority(
            &snapshot,
            &AccountingAuthorityInputs {
                context_window: snapshot.context_window,
                max_provider_output_tokens_per_turn: 1024,
            },
        )
        .expect("accounting authority");
        let facts = runtime_facts(&snapshot, &authority).expect("runtime facts");
        assert_eq!(
            facts.get("reasoning"),
            Some(&serde_json::to_value(expected).expect("reasoning fact"))
        );
    }

    #[test]
    fn launch_preparation_resolves_and_seals_reasoning_policy() {
        let composed = serde_json::json!({
            "model": {
                "provider": "fixture",
                "name": "fixture-model",
                "context_window": 16384,
                "reasoning": {"mode": "disabled"}
            }
        });
        let config_id = "ryeos-runtime/model-providers/fixture".to_string();
        let provider_catalog = BTreeMap::from([(
            config_id.clone(),
            VerifiedConfigItem {
                value: serde_json::to_value(launch_provider()).expect("provider value"),
                value_digest: "0".repeat(64),
                contributors: vec![ProviderConfigSource {
                    space: SnapshotItemSpace::Bundle,
                    root_label: "bundle:fixture".to_string(),
                    canonical_id: config_id,
                    content_digest: "1".repeat(64),
                    trust_class: SnapshotTrustClass::TrustedBundle,
                }],
            },
        )]);

        let prepared = prepare_directive_launch(DirectiveLaunchPreparationInput {
            primary_ref: "directive:test/model",
            primary_composed: &composed,
            model_ref: "directive:test/model",
            model_composed: &composed,
            model_routing: None,
            provider_catalog: &provider_catalog,
            execution: None,
        })
        .expect("reasoning policy launch preparation");

        let expected = ReasoningConfig {
            mode: ReasoningMode::Disabled,
            effort: None,
        };
        assert_eq!(prepared.snapshot.reasoning, Some(expected.clone()));
        assert_eq!(
            prepared.runtime_facts.get("reasoning"),
            Some(&serde_json::to_value(expected).expect("reasoning fact"))
        );
    }
}

#[cfg(test)]
mod shipped_provider_config_tests {
    use super::ProviderConfig;

    #[test]
    fn deepseek_config_deserializes_and_validates_for_every_shipped_model() {
        let value: serde_json::Value = serde_yaml::from_str(include_str!(
            "../../../../bundles/standard/.ai/config/ryeos-runtime/model-providers/deepseek.yaml"
        ))
        .expect("shipped DeepSeek provider YAML");
        let provider: ProviderConfig =
            serde_json::from_value(value).expect("shipped DeepSeek provider config");

        for model in ["deepseek-v4-pro", "deepseek-v4-flash"] {
            provider
                .resolve_for_model(model)
                .validate(&format!(" for shipped model {model}"))
                .unwrap_or_else(|error| panic!("invalid shipped DeepSeek config: {error:#}"));
        }
    }

    #[test]
    fn admitted_local_worker_is_recorded_only_until_qualification_exists() {
        let value: serde_json::Value = serde_yaml::from_str(include_str!(
            "../../../../bundles/standard/.ai/config/ryeos-runtime/model-providers/local-tinygrad.yaml"
        ))
        .expect("shipped local worker provider YAML");
        let provider: ProviderConfig =
            serde_json::from_value(value.clone()).expect("shipped local worker provider config");
        provider
            .validate(" for shipped local worker")
            .expect("recorded local worker provider must be admissible");

        let mut unqualified = value;
        unqualified["transport"]["effect_class_ceiling"] = serde_json::json!("sealed");
        let unqualified: ProviderConfig = serde_json::from_value(unqualified)
            .expect("sealed claim remains a syntactically typed effect class");
        let error = unqualified
            .validate(" for unqualified local worker")
            .expect_err("authored local worker config cannot mint sealed evidence");
        assert!(error.to_string().contains("qualification evidence"));
    }
}
