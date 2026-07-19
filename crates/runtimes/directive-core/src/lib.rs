//! Directive-owned model/provider schema and pure launch preparation.
//!
//! This crate deliberately has no executor, daemon, vault, thread, handler-runner,
//! filesystem, or environment dependency. The host supplies already-authorized,
//! resolved, trusted composed views and verified configuration snapshots. This
//! crate applies only directive-domain policy and returns opaque launch data,
//! symbolic secret requirements, and bounded audit facts.

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use url::Url;

pub const MODEL_BINDING: &str = "model";
pub const MODEL_ROUTING_INPUT: &str = "model_routing";
pub const MODEL_PROVIDERS_INPUT: &str = "model_providers";
pub const PROVIDER_SNAPSHOT_KEY: &str = "provider_snapshot";
pub const PROVIDER_CONFIG_PREFIX: &str = "ryeos-runtime/model-providers";

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
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub schemas: Option<SchemasConfig>,
    #[serde(default)]
    pub extra: Option<HashMap<String, Value>>,
    #[serde(default)]
    pub body_template: Option<Value>,
    #[serde(default)]
    pub body_extra: Option<Value>,
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
    pub messages: Option<MessageSchemas>,
    #[serde(default)]
    pub tools: Option<ToolSchemaConfig>,
    #[serde(default)]
    pub streaming: Option<StreamingConfig>,
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
    pub mode: Option<String>,
    #[serde(default)]
    pub paths: Option<StreamPaths>,
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
    #[serde(default)]
    pub input_per_million: Option<f64>,
    #[serde(default)]
    pub output_per_million: Option<f64>,
    #[serde(default)]
    pub models: HashMap<String, ModelPricing>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
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
            })
        })
    }
}

impl ProviderConfig {
    pub fn setup_projection(&self, provider_id: &str) -> Result<ProviderSetupProjection> {
        let setup = match self.extra.get("setup") {
            Some(value) => serde_json::from_value::<ProviderSetupConfig>(value.clone())
                .map_err(|error| anyhow::anyhow!("provider '{provider_id}' setup metadata is invalid: {error}"))?,
            None => ProviderSetupConfig {
                display_name: provider_id.to_string(),
                priority: default_setup_priority(),
                recommended: false,
                credential: self.auth.env_var.as_ref().map(|secret_name| ProviderSetupCredential {
                    label: "Credential".to_string(),
                    secret_name: secret_name.clone(),
                    input: ProviderSetupInput::Secret,
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
            },
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
                bail!("provider '{provider_id}' setup credential fields are empty, unsafe, or too long");
            }
            if self.auth.env_var.as_deref() != Some(credential.secret_name.as_str()) {
                bail!(
                    "provider '{provider_id}' setup secret_name '{}' does not exactly match runtime auth.env_var",
                    credential.secret_name
                );
            }
        }
        if setup.help_url.as_deref().is_some_and(|url| {
            url.len() > 4096
                || !(url.starts_with("https://") || url.starts_with("http://"))
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
            validate_setup_endpoint(
                provider_id,
                &self.base_url,
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
                validate_setup_endpoint(
                    provider_id,
                    &effective.base_url,
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
        if self.base_url.trim().is_empty() {
            bail!("provider config{context} has an empty base_url");
        }
        if self.body_template.is_none() {
            bail!("provider config{context} has no body_template");
        }
        if self.auth.env_var.is_some() != self.auth.header_name.is_some() {
            bail!(
                "provider config{context}: auth.env_var and auth.header_name must both be set or both be absent"
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

        match self.family {
            ProtocolFamily::AnthropicMessages => {
                if let Some(messages) = self.schemas.as_ref().and_then(|s| s.messages.as_ref()) {
                    if messages.assistant_tool_calls_placement
                        != Some(AssistantToolCallsPlacement::InlineBlocks)
                    {
                        bail!(
                            "provider config{context}: anthropic_messages requires inline_blocks tool calls"
                        );
                    }
                }
            }
            ProtocolFamily::GoogleGenerateContent => {
                if let Some(messages) = self.schemas.as_ref().and_then(|s| s.messages.as_ref()) {
                    if messages.content_key.as_deref() != Some("parts") {
                        bail!(
                            "provider config{context}: google_generate_content requires messages.content_key=parts"
                        );
                    }
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
        if let Some(base_url) = &profile.base_url {
            resolved.base_url = base_url.clone();
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
    let validation_url = Url::parse(&validation_source)
        .map_err(|error| anyhow::anyhow!("provider '{provider_id}' setup validation URL is invalid: {error}"))?;
    let base_source = base_url.replace("{model}", "setup-probe");
    let base = Url::parse(&base_source)
        .map_err(|error| anyhow::anyhow!("provider '{provider_id}' base_url is invalid: {error}"))?;
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotTrustClass {
    TrustedBundle,
    TrustedProject,
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
    if let Some(profile) = &matched_profile {
        if profile.trim().is_empty() || profile.len() > 128 || profile.chars().any(char::is_control)
        {
            return Err(DirectivePreparationError::configuration(
                "provider_config_invalid",
                "the matched provider profile name must be 1-128 bytes without control characters",
            ));
        }
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
        matched_profile,
        config_value_digest: provider_entry.value_digest.clone(),
        config_sources: provider_entry.contributors.clone(),
        config_hash,
        provider: resolved_provider,
    };
    let runtime_facts = runtime_facts(&snapshot)?;

    Ok(DirectiveLaunchPreparation {
        snapshot,
        required_secret,
        runtime_facts,
    })
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
) -> std::result::Result<BTreeMap<String, Value>, DirectivePreparationError> {
    let mut facts = BTreeMap::new();
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
