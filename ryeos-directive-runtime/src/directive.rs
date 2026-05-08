use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Typed runtime view of a directive's effective header *after* the
/// daemon-side composer has produced
/// `KindComposedView::ExtendsChain(...).composed`.
///
/// The runtime owns the typed shape of the fields it **consumes**
/// (`model`, `permissions`, `limits`, `outputs`, `context`, `hooks`,
/// `extends`, `name`).  `deny_unknown_fields` is mandatory — relaxing
/// it once already masked a routing-config regression by accepting
/// fields the runtime quietly ignored.
///
/// The composer ships extra frontmatter keys (`body`, `category`,
/// `description`, `inputs`, `required_secrets`, …) that are owned by
/// the engine / dispatcher (e.g. `required_secrets` is read on the
/// daemon side by `dispatch.rs::vault_bindings`).  Those fields MUST
/// be stripped *before* deserialising into `DirectiveHeader` —
/// `bootstrap.rs::parse_effective_header` does the projection — so
/// the typed shape stays narrow and `deny_unknown_fields` keeps
/// catching real drift instead of being defeated by every new
/// composer-emitted key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectiveHeader {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub model: Option<ModelSpec>,
    #[serde(default)]
    pub permissions: Option<PermissionsSpec>,
    #[serde(default)]
    pub limits: Option<LimitsSpec>,
    #[serde(default)]
    pub outputs: Option<Vec<OutputSpec>>,
    #[serde(default)]
    pub context: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub hooks: Option<Vec<Value>>,
}

/// Allow-list of composed-view top-level keys the runtime decodes
/// into [`DirectiveHeader`].  See the type's doc comment for the
/// rationale: every other key is owned by the daemon (composer,
/// dispatcher, augmentations) and is read directly off
/// `KindComposedView` by the daemon, not by the runtime.
pub const DIRECTIVE_HEADER_RUNTIME_KEYS: &[&str] = &[
    "name",
    "extends",
    "model",
    "permissions",
    "limits",
    "outputs",
    "context",
    "hooks",
];

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
}

/// LLM sampling parameters for best-effort replay. Not all providers
/// support every field — e.g. OpenAI supports `seed`, Anthropic does
/// not. The provider adapter silently omits unsupported fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingConfig {
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsSpec {
    #[serde(default)]
    pub execute: Vec<String>,
    #[serde(default)]
    pub fetch: Vec<String>,
    #[serde(default)]
    pub sign: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSpec {
    #[serde(default)]
    pub turns: Option<u32>,
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub spend_usd: Option<f64>,
    #[serde(default)]
    pub spawns: Option<u32>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub duration_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSchema {
    pub name: String,
    pub item_id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Kind-schema metadata header (e.g. `"ryeos-runtime/model-providers"`)
    /// surfaced on the typed struct so `deny_unknown_fields` keeps
    /// holding the line. Not consumed by the runtime; logged at
    /// bootstrap for parity with the other config structs.
    #[serde(default)]
    pub category: Option<String>,
    pub base_url: String,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub schemas: Option<SchemasConfig>,
    #[serde(default)]
    pub pricing: Option<PricingConfig>,
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default)]
    pub env_var: Option<String>,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemasConfig {
    #[serde(default)]
    pub messages: Option<MessageSchemas>,
    #[serde(default)]
    pub streaming: Option<StreamingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSchemas {
    #[serde(default)]
    pub role_map: Option<HashMap<String, String>>,
    #[serde(default)]
    pub content_key: Option<String>,
    #[serde(default)]
    pub content_wrap: Option<String>,
    #[serde(default)]
    pub system_message: Option<SystemMessageConfig>,
    #[serde(default)]
    pub tool_result: Option<ToolResultConfig>,
    #[serde(default)]
    pub tool_list_wrap: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemMessageConfig {
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultConfig {
    #[serde(default)]
    pub wrap_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingConfig {
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingConfig {
    #[serde(default)]
    pub input_per_million: Option<f64>,
    #[serde(default)]
    pub output_per_million: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    /// Kind-schema metadata header (e.g. `"ryeos-runtime"`) surfaced so
    /// `deny_unknown_fields` keeps holding the line. Not consumed by
    /// the runtime.
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub retry_status_codes: Vec<u16>,
    #[serde(default)]
    pub never_retry: Vec<String>,
    #[serde(default)]
    pub backoff_base_ms: u64,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub tool_preload: bool,
    #[serde(default)]
    pub retry_on_timeout: bool,
}

fn default_timeout() -> u64 { 300 }

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            category: None,
            retries: 2,
            retry_status_codes: vec![429, 500, 502, 503],
            never_retry: vec![],
            backoff_base_ms: 1000,
            timeout_seconds: default_timeout(),
            tool_preload: false,
            retry_on_timeout: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub execution: ExecutionConfig,
    pub model_routing: Option<ModelRoutingConfig>,
    pub provider: Option<ProviderConfig>,
    pub tools: Vec<ToolSchema>,
    pub system_prompt: Option<String>,
    pub user_prompt: String,
    pub context_before: Option<String>,
    pub context_after: Option<String>,
    pub context_positions: HashMap<String, Vec<String>>,
    pub hooks: Vec<ryeos_runtime::HookDefinition>,
    pub outputs: Option<Vec<OutputSpec>>,
    #[serde(skip)]
    pub risk_policy: Option<crate::harness::RiskPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoutingConfig {
    /// Kind-schema metadata header (e.g. `"ryeos-runtime"`) surfaced so
    /// `deny_unknown_fields` keeps holding the line. Not consumed by
    /// the runtime.
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tiers: HashMap<String, TierConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMessage {
    pub role: String,
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    ToolUse { id: String, name: String, arguments: String },
    Done,
}
