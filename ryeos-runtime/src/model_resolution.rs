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
        provider,
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
}
