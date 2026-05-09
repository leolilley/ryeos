//! Canonical model-target resolution: tier → provider → auth.env_var.
//!
//! Owned by `ryeos-runtime` so both the daemon (preflight at
//! `/execute/stream`) and the directive runtime (call-time) share
//! one implementation. The daemon uses this to narrow vault secret
//! injection to exactly the provider the directive will use; the
//! runtime uses this as defense-in-depth and to actually wire the
//! provider HTTP client.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::verified_loader::VerifiedLoader;

// ── Directive-side model header (projection) ──────────────────────

/// Minimal projection of a directive's model header, containing only
/// the fields that model-resolution reads. The full `DirectiveHeader`
/// lives in `ryeos-directive-runtime`; this type exists so the
/// shared resolver doesn't depend on the directive-runtime crate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirectiveModelHeader {
    #[serde(default)]
    pub model: Option<ModelSpec>,
}

// ── Core model types ──────────────────────────────────────────────

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

// ── Model routing config ──────────────────────────────────────────

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

// ── Provider config (on-disk shape with auth.env_var) ─────────────

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
    /// JSON template for the request body. Placeholders of the form
    /// `"{key}"` are substituted at call time using a context that
    /// always includes `model`, `messages`, `tools`, `stream`,
    /// `max_tokens`.
    ///
    /// REQUIRED — every provider config (base or profile) must set
    /// this. The adapter does not guess a body shape.
    #[serde(default)]
    pub body_template: Option<Value>,
    /// Static body fragment deep-merged INTO the templated body
    /// after substitution. Use for provider-required constants
    /// like Gemini's `generationConfig.thinkingConfig.includeThoughts`
    /// that don't depend on per-call inputs.
    #[serde(default)]
    pub body_extra: Option<Value>,
    /// Profile-by-model overlay. Profiles are tried in declaration
    /// order; the first whose `match` glob list contains a pattern
    /// matching the model name wins. The matched profile is
    /// deep-merged over the base config.
    #[serde(default)]
    pub profiles: Vec<ProviderProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    pub name: String,
    pub r#match: Vec<String>,
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
    /// See `ProviderConfig::body_template`. When set on a profile,
    /// **replaces** any base body_template wholesale (not deep-merged).
    #[serde(default)]
    pub body_template: Option<Value>,
    /// See `ProviderConfig::body_extra`. Profile body_extra is
    /// deep-merged over base body_extra (so both can contribute,
    /// with profile winning on key collisions).
    #[serde(default)]
    pub body_extra: Option<Value>,
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
    /// Where the adapter places the system prompt.
    /// - `"body_field"` — set `body[field]` to the system string
    ///   (Anthropic Messages: `body["system"]`). `field` defaults
    ///   to `"system"`.
    /// - `"body_inject"` — deep-merge a templated structure into
    ///   the body. The template's `{system}` placeholder is filled
    ///   with the system prompt string. Used by Gemini whose system
    ///   prompt lives at `body.systemInstruction.parts[0].text`.
    /// - `"message_role"` — prepend a `{role: "system", content:
    ///   <prompt>}` message to the converted message list (OpenAI).
    /// - `None` — falls back to `"body_field"` for backwards compat.
    #[serde(default)]
    pub mode: Option<String>,
    /// For `mode = "body_field"`: the body key to set. Defaults to
    /// `"system"`.
    #[serde(default)]
    pub field: Option<String>,
    /// For `mode = "body_inject"`: a JSON template that gets
    /// rendered with `{"system": <prompt>}` and deep-merged into
    /// the request body.
    #[serde(default)]
    pub template: Option<Value>,
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
    /// Stream envelope shape. One of:
    /// - `"delta_merge"` — OpenAI-style: incremental `delta.content`
    ///   text fragments concatenated into the assistant message.
    /// - `"event_typed"` — Anthropic-style: SSE `event:` typed
    ///   frames (`content_block_start`, `content_block_delta`,
    ///   `content_block_stop`, `message_stop`).
    /// - `"complete_chunks"` — Gemini-style: each `data:` frame is
    ///   a complete partial response with `candidates[0].content.
    ///   parts[]`. Uses `paths` to extract text/tool_calls/usage.
    #[serde(default)]
    pub mode: Option<String>,
    /// Path config for `complete_chunks` mode. Ignored otherwise.
    /// Each field is a dot-path into one streamed JSON frame.
    #[serde(default)]
    pub paths: Option<StreamPaths>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamPaths {
    /// Path to the array of content blocks in each frame.
    /// Gemini: `"candidates.0.content.parts"`.
    pub content_path: String,
    /// Field name on a content block that, when present, marks
    /// the block as text. Gemini: `"text"`.
    pub text_field: String,
    /// Optional field that, when present and truthy, marks the
    /// block as a thinking block (filtered out of user-visible
    /// text but counted in token usage). Gemini: `"thought"`.
    #[serde(default)]
    pub thought_field: Option<String>,
    /// Optional field that, when present, marks the block as a
    /// tool call. Gemini: `"functionCall"`.
    #[serde(default)]
    pub tool_call_field: Option<String>,
    /// Path within a tool_call block to the function name.
    /// Gemini: `"functionCall.name"`.
    #[serde(default)]
    pub tool_call_name_path: Option<String>,
    /// Path within a tool_call block to the function arguments
    /// (JSON object). Gemini: `"functionCall.args"`.
    #[serde(default)]
    pub tool_call_args_path: Option<String>,
    /// Path to usage object in the frame.
    /// Gemini: `"usageMetadata"`.
    #[serde(default)]
    pub usage_path: Option<String>,
    /// Field on the usage object for input/prompt tokens.
    /// Gemini: `"promptTokenCount"`.
    #[serde(default)]
    pub input_tokens_field: Option<String>,
    /// Field on the usage object for output/completion tokens.
    /// Gemini: `"candidatesTokenCount"`.
    #[serde(default)]
    pub output_tokens_field: Option<String>,
    /// Path to the finish reason string in the frame.
    /// Gemini: `"candidates.0.finishReason"`.
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
    /// Per-model overrides. Looked up first; falls back to the
    /// per-provider defaults above when absent.
    #[serde(default)]
    pub models: HashMap<String, ModelPricing>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

impl PricingConfig {
    /// Effective pricing for `model_name`: per-model entry if present,
    /// else the provider-level defaults wrapped in `ModelPricing`.
    pub fn for_model(&self, model_name: &str) -> Option<ModelPricing> {
        if let Some(p) = self.models.get(model_name) {
            return Some(p.clone());
        }
        match (self.input_per_million, self.output_per_million) {
            (Some(i), Some(o)) => Some(ModelPricing {
                input_per_million: i,
                output_per_million: o,
            }),
            _ => None,
        }
    }
}

// ── Resolution result types ───────────────────────────────────────

/// Coherent provider + model pair resolved from a single source.
/// Prevents mismatched pairs (e.g. Anthropic model sent to OpenAI)
/// that arise when provider and model are resolved independently.
#[derive(Debug)]
pub struct ResolvedModelTarget {
    pub provider: ProviderConfig,
    pub provider_id: String,
    pub model_name: String,
    pub context_window: u64,
    pub source: &'static str, // "directive" | "routing" for diagnostics
}

/// Intermediate resolution result — provider ID + model + context_window
/// determined coherently from a single source, before loading the provider
/// config from disk.
#[derive(Debug)]
struct ResolvedTargetInfo {
    provider_id: String,
    model_name: String,
    context_window: u64,
    source: &'static str,
}

// ── Provider profile resolution ────────────────────────────────────

impl ProviderConfig {
    /// Return a config with the matching profile (if any) deep-merged
    /// over the base. Profiles are tried in order; first hit wins.
    pub fn resolve_for_model(&self, model_name: &str) -> ProviderConfig {
        for profile in &self.profiles {
            if profile.r#match.iter().any(|pat| glob_match(pat, model_name)) {
                return self.merge_profile(profile);
            }
        }
        self.clone()
    }

    fn merge_profile(&self, p: &ProviderProfile) -> ProviderConfig {
        let mut out = self.clone();
        if let Some(url) = &p.base_url { out.base_url = url.clone(); }
        if let Some(auth) = &p.auth { out.auth = auth.clone(); }
        if let Some(h) = &p.headers {
            // Profile headers replace conflicting keys, others inherit.
            for (k, v) in h { out.headers.insert(k.clone(), v.clone()); }
        }
        if let Some(s) = &p.schemas { out.schemas = Some(s.clone()); }
        if let Some(e) = &p.extra {
            for (k, v) in e { out.extra.insert(k.clone(), v.clone()); }
        }
        if let Some(bt) = &p.body_template {
            // Wholesale replacement, not merge — see ProviderProfile docs.
            out.body_template = Some(bt.clone());
        }
        if let Some(be) = &p.body_extra {
            // Deep-merge profile body_extra onto base body_extra.
            match &mut out.body_extra {
                Some(existing) => {
                    crate::template::deep_merge(existing, be);
                }
                None => out.body_extra = Some(be.clone()),
            }
        }
        // profiles is intentionally *not* carried — the resolved view
        // is flat and the runtime never recurses.
        out.profiles = Vec::new();
        out
    }
}

/// Minimal glob: `*` matches anything, exact otherwise.
/// For our needs (model-name prefix/suffix matching), this is enough.
/// If patterns get more complex, swap in the `globset` crate.
fn glob_match(pattern: &str, candidate: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        candidate.starts_with(prefix)
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        candidate.ends_with(suffix)
    } else {
        pattern == candidate
    }
}

// ── Resolution functions ──────────────────────────────────────────

/// Determine provider ID, model name, and context window coherently
/// from a single source (directive or routing tier). Rejects mixed
/// sources (e.g. directive.provider + routing.model) to prevent
/// mismatched provider/model pairs.
fn resolve_target_info(
    header: &DirectiveModelHeader,
    routing: &Option<ModelRoutingConfig>,
) -> Result<ResolvedTargetInfo> {
    let tier = header
        .model
        .as_ref()
        .and_then(|m| m.tier.as_deref())
        .unwrap_or("general");

    // ── Source 1: directive provides model name ──
    if let Some(ref model_spec) = header.model {
        if model_spec.name.is_some() {
            let name = model_spec.name.as_ref().unwrap();

            // Directive provides the model — it MUST also provide the
            // provider and context_window so the pair is coherent.
            let provider_id = model_spec.provider.as_deref().ok_or_else(|| {
                anyhow!(
                    "directive specifies model.name `{name}` but omits \
                     model.provider — when a directive names a model it \
                     must also name the provider to ensure a coherent pair \
                     (add `provider: <id>` under `model:` in the directive \
                     frontmatter, or remove `model.name` and rely on \
                     model_routing tier `{tier}`)"
                )
            })?;
            let ctx = model_spec.context_window.ok_or_else(|| {
                anyhow!(
                    "directive specifies model.name `{name}` but omits \
                     model.context_window — every model must declare its \
                     context limit explicitly (add `context_window: <value>` \
                     under `model:` in the directive frontmatter)"
                )
            })?;

            return Ok(ResolvedTargetInfo {
                provider_id: provider_id.to_string(),
                model_name: name.clone(),
                context_window: ctx,
                source: "directive",
            });
        }
    }

    // ── Source 2: routing tier provides provider + model ──
    if let Some(ref routing_cfg) = routing {
        if let Some(tier_cfg) = routing_cfg.tiers.get(tier) {
            let ctx = tier_cfg.context_window.ok_or_else(|| {
                anyhow!(
                    "model_routing tier `{tier}`: missing `context_window` — \
                     every tier must declare its context limit explicitly"
                )
            })?;

            return Ok(ResolvedTargetInfo {
                provider_id: tier_cfg.provider.clone(),
                model_name: tier_cfg.model.clone(),
                context_window: ctx,
                source: "routing",
            });
        }
    }

    // ── Source 3: no resolution possible ──
    anyhow::bail!(
        "model target unresolvable: directive has no model.name and \
         model_routing has no tier \"{tier}\". Set `model.name` (and \
         `model.provider`, `model.context_window`) on the directive, \
         or add tier \"{tier}\" to model_routing.yaml."
    )
}

/// Full resolution: coherent target info + provider config loaded from disk.
pub fn resolve_model_target(
    header: &DirectiveModelHeader,
    routing: &Option<ModelRoutingConfig>,
    loader: &VerifiedLoader,
) -> Result<ResolvedModelTarget> {
    let info = resolve_target_info(header, routing)?;

    let config_id = format!("model-providers/{}", info.provider_id);
    let provider = loader
        .load_config_strict::<ProviderConfig>(&config_id)
        .map_err(|e| anyhow!("loading provider config `{config_id}`: {e}"))?
        .ok_or_else(|| {
            anyhow!(
                "provider config not found: `{config_id}` — expected \
                 `.ai/config/ryeos-runtime/{config_id}.yaml` under one of \
                 system / user / project roots (resolved from {})",
                info.source
            )
        })?;

    Ok(ResolvedModelTarget {
        provider: provider.resolve_for_model(&info.model_name),
        provider_id: info.provider_id,
        model_name: info.model_name,
        context_window: info.context_window,
        source: info.source,
    })
}

/// Daemon-side preflight helper: load routing + resolve, returning the
/// resolved target without re-reading the routing config more than
/// once. Caller is expected to pass a loader scoped to the same roots
/// the runtime will see.
pub fn preflight_resolve(
    header: &DirectiveModelHeader,
    loader: &VerifiedLoader,
) -> Result<ResolvedModelTarget> {
    let routing = loader
        .load_config_strict::<ModelRoutingConfig>("model_routing")
        .map_err(|e| anyhow!("loading config `model_routing`: {e}"))?;
    resolve_model_target(header, &routing, loader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_target_from_directive() {
        let header = DirectiveModelHeader {
            model: Some(ModelSpec {
                tier: None,
                provider: Some("openai".to_string()),
                name: Some("gpt-4".to_string()),
                context_window: Some(128_000),
                sampling: None,
            }),
        };
        let info = resolve_target_info(&header, &None).unwrap();
        assert_eq!(info.provider_id, "openai");
        assert_eq!(info.model_name, "gpt-4");
        assert_eq!(info.context_window, 128_000);
        assert_eq!(info.source, "directive");
    }

    #[test]
    fn resolve_target_directive_name_without_provider_errors() {
        let header = DirectiveModelHeader {
            model: Some(ModelSpec {
                tier: None,
                provider: None,
                name: Some("gpt-4".to_string()),
                context_window: Some(128_000),
                sampling: None,
            }),
        };
        let result = resolve_target_info(&header, &None);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("model.provider"), "error should mention provider: {msg}");
    }

    #[test]
    fn resolve_target_directive_name_without_context_window_errors() {
        let header = DirectiveModelHeader {
            model: Some(ModelSpec {
                tier: None,
                provider: Some("openai".to_string()),
                name: Some("gpt-4".to_string()),
                context_window: None,  // <-- missing
                sampling: None,
            }),
        };
        let result = resolve_target_info(&header, &None);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("context_window"), "error should mention context_window: {msg}");
    }

    #[test]
    fn resolve_target_from_routing() {
        let header = DirectiveModelHeader {
            model: Some(ModelSpec {
                tier: Some("fast".to_string()),
                provider: None,
                name: None,
                context_window: None,
                sampling: None,
            }),
        };
        let routing = ModelRoutingConfig {
            category: None,
            tiers: {
                let mut m = HashMap::new();
                m.insert("fast".to_string(), TierConfig {
                    provider: "openai".to_string(),
                    model: "gpt-4o-mini".to_string(),
                    context_window: Some(128_000),
                });
                m
            },
        };
        let info = resolve_target_info(&header, &Some(routing)).unwrap();
        assert_eq!(info.provider_id, "openai");
        assert_eq!(info.model_name, "gpt-4o-mini");
        assert_eq!(info.context_window, 128_000);
        assert_eq!(info.source, "routing");
    }

    #[test]
    fn resolve_target_errors_when_unresolvable() {
        let header = DirectiveModelHeader::default();
        let result = resolve_target_info(&header, &None);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("model target unresolvable"), "error message: {msg}");
        assert!(msg.contains("general"), "error should mention the default tier");
    }

    #[test]
    fn resolve_target_rejects_mixed_sources() {
        // directive.model.provider is set but directive.model.name is NOT.
        // The directive doesn't provide a model name, so the resolver
        // falls through to routing. The routing's tier provides BOTH
        // provider and model coherently — this is fine.
        //
        // The real danger was the OLD code where resolve_provider() read
        // directive.provider and resolve_model() read routing.model
        // independently, producing a mismatch. Now, if directive.model.name
        // is None, the resolver ignores directive.provider entirely and
        // uses routing as a coherent source.
        let header = DirectiveModelHeader {
            model: Some(ModelSpec {
                tier: Some("general".to_string()),
                provider: Some("openai".to_string()),  // ignored — name is None
                name: None,
                context_window: None,
                sampling: None,
            }),
        };
        let routing = ModelRoutingConfig {
            category: None,
            tiers: {
                let mut m = HashMap::new();
                m.insert("general".to_string(), TierConfig {
                    provider: "anthropic".to_string(),
                    model: "claude-sonnet".to_string(),
                    context_window: Some(200_000),
                });
                m
            },
        };
        let info = resolve_target_info(&header, &Some(routing)).unwrap();
        // The routing tier is used as the coherent source, NOT the
        // directive's provider. So provider comes from routing.
        assert_eq!(info.provider_id, "anthropic");
        assert_eq!(info.model_name, "claude-sonnet");
        assert_eq!(info.source, "routing");
    }

    // ── Profile resolution tests ────────────────────────────────────

    fn base_provider() -> ProviderConfig {
        ProviderConfig {
            category: None,
            base_url: "https://base.example.com/v1".to_string(),
            auth: AuthConfig {
                env_var: Some("BASE_KEY".to_string()),
                header_name: Some("Authorization".to_string()),
                prefix: Some("Bearer ".to_string()),
            },
            headers: {
                let mut h = HashMap::new();
                h.insert("Content-Type".to_string(), "application/json".to_string());
                h
            },
            schemas: None,
            pricing: None,
            extra: HashMap::new(),
            body_template: None,
            body_extra: None,
            profiles: vec![
                ProviderProfile {
                    name: "claude".to_string(),
                    r#match: vec!["claude-*".to_string()],
                    base_url: Some("https://anthropic.example.com/v1/messages".to_string()),
                    auth: Some(AuthConfig {
                        env_var: Some("ANTHROPIC_KEY".to_string()),
                        header_name: Some("x-api-key".to_string()),
                        prefix: Some("".to_string()),
                    }),
                    headers: Some({
                        let mut h = HashMap::new();
                        h.insert("anthropic-version".to_string(), "2023-06-01".to_string());
                        h
                    }),
                    schemas: None,
                    extra: None,
                    body_template: None,
                    body_extra: None,
                },
                ProviderProfile {
                    name: "gpt".to_string(),
                    r#match: vec!["gpt-*".to_string()],
                    base_url: None,
                    auth: None,
                    headers: Some({
                        let mut h = HashMap::new();
                        h.insert("X-OpenAI".to_string(), "true".to_string());
                        h
                    }),
                    schemas: None,
                    extra: None,
                    body_template: None,
                    body_extra: None,
                },
                ProviderProfile {
                    name: "gemini".to_string(),
                    r#match: vec!["gemini-*".to_string()],
                    base_url: Some("https://gemini.example.com/v1/models/{model}:generate".to_string()),
                    auth: Some(AuthConfig {
                        env_var: Some("BASE_KEY".to_string()),
                        header_name: Some("x-goog-api-key".to_string()),
                        prefix: Some("".to_string()),
                    }),
                    headers: None,
                    schemas: None,
                    extra: None,
                    body_template: None,
                    body_extra: None,
                },
            ],
        }
    }

    #[test]
    fn profile_match_glob_prefix() {
        let provider = base_provider();
        let resolved = provider.resolve_for_model("claude-haiku-4-5");
        assert_eq!(
            resolved.base_url,
            "https://anthropic.example.com/v1/messages"
        );
        assert_eq!(resolved.auth.env_var.as_deref(), Some("ANTHROPIC_KEY"));
    }

    #[test]
    fn profile_match_first_wins() {
        let provider = base_provider();
        // "gpt-4" matches the "gpt-*" profile (second), not claude
        let resolved = provider.resolve_for_model("gpt-4");
        assert_eq!(
            resolved.base_url,
            "https://base.example.com/v1",
            "gpt profile doesn't override base_url"
        );
        assert_eq!(
            resolved.headers.get("X-OpenAI").map(|s| s.as_str()),
            Some("true"),
            "gpt profile adds its header"
        );
        assert!(
            resolved.headers.contains_key("Content-Type"),
            "base headers inherited"
        );
    }

    #[test]
    fn profile_no_match_returns_base_clone() {
        let provider = base_provider();
        let resolved = provider.resolve_for_model("llama-3-70b");
        assert_eq!(resolved.base_url, "https://base.example.com/v1");
        assert_eq!(resolved.auth.env_var.as_deref(), Some("BASE_KEY"));
    }

    #[test]
    fn profile_merge_overrides_url_auth_keeps_inherited_headers() {
        let provider = base_provider();
        let resolved = provider.resolve_for_model("claude-opus-4");
        assert_eq!(resolved.base_url, "https://anthropic.example.com/v1/messages");
        assert_eq!(resolved.auth.header_name.as_deref(), Some("x-api-key"));
        assert_eq!(resolved.auth.prefix.as_deref(), Some(""));
        // Base header preserved + profile header added
        assert_eq!(resolved.headers.get("Content-Type").map(|s| s.as_str()), Some("application/json"));
        assert_eq!(resolved.headers.get("anthropic-version").map(|s| s.as_str()), Some("2023-06-01"));
    }

    #[test]
    fn profile_resolved_view_has_no_profiles() {
        let provider = base_provider();
        let resolved = provider.resolve_for_model("claude-haiku-4-5");
        assert!(resolved.profiles.is_empty(), "resolved config must be flat");
    }

    // ── Pricing tests ───────────────────────────────────────────────

    #[test]
    fn pricing_per_model_overrides_provider_default() {
        let pricing = PricingConfig {
            input_per_million: Some(1.0),
            output_per_million: Some(5.0),
            models: {
                let mut m = HashMap::new();
                m.insert("claude-haiku-4-5".to_string(), ModelPricing {
                    input_per_million: 0.80,
                    output_per_million: 4.00,
                });
                m
            },
        };
        let p = pricing.for_model("claude-haiku-4-5").unwrap();
        assert_eq!(p.input_per_million, 0.80);
        assert_eq!(p.output_per_million, 4.00);
    }

    #[test]
    fn pricing_falls_back_to_provider_default_when_model_absent() {
        let pricing = PricingConfig {
            input_per_million: Some(1.0),
            output_per_million: Some(5.0),
            models: HashMap::new(),
        };
        let p = pricing.for_model("unknown-model").unwrap();
        assert_eq!(p.input_per_million, 1.0);
        assert_eq!(p.output_per_million, 5.0);
    }

    #[test]
    fn pricing_returns_none_when_no_default_and_no_model_entry() {
        let pricing = PricingConfig {
            input_per_million: None,
            output_per_million: None,
            models: HashMap::new(),
        };
        assert!(pricing.for_model("anything").is_none());
    }

    // ── Glob match tests ────────────────────────────────────────────

    #[test]
    fn glob_match_prefix_star() {
        assert!(super::glob_match("claude-*", "claude-haiku-4-5"));
        assert!(!super::glob_match("claude-*", "gpt-4"));
    }

    #[test]
    fn glob_match_suffix_star() {
        assert!(super::glob_match("*-free", "minimax-m2.5-free"));
        assert!(!super::glob_match("*-free", "minimax-m2.5"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(super::glob_match("big-pickle", "big-pickle"));
        assert!(!super::glob_match("big-pickle", "small-pickle"));
    }
}
