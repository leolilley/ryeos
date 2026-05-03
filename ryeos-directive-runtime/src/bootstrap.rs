use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::directive::*;
use ryeos_engine::resolution::KindComposedView;
use ryeos_runtime::verified_loader::VerifiedLoader;

/// Conventional `derive_as` name on the directive kind's
/// `composer_config` for the post-header body. The runtime reads
/// `view.derived[DERIVED_BODY]` rather than naming any underlying
/// parser field — adding a new directive-style kind = adding a new
/// kind schema YAML, no runtime change.
const DERIVED_BODY: &str = "body";

/// Conventional `derive_as` name on the directive kind's
/// `composer_config` for the merged `Map<String, Vec<String>>`
/// context positions.
const DERIVED_COMPOSED_CONTEXT: &str = "composed_context";

pub struct BootstrapOutput {
    pub config: BootstrapConfig,
    pub provider: ProviderConfig,
    pub model_name: String,
    pub context_window: u64,
}

pub fn bootstrap(
    _project_root: &Path,
    _user_root: Option<&Path>,
    _system_roots: &[PathBuf],
    composed_view: &KindComposedView,
    _envelope_limits: &ryeos_runtime::envelope::HardLimits,
    loader: &VerifiedLoader,
    inventory: &HashMap<String, Vec<ryeos_runtime::envelope::ItemDescriptor>>,
) -> Result<BootstrapOutput> {
    // The runtime no longer composes — the daemon-side composer has
    // already produced the effective header (in `composed`), the
    // body and context positions (in `derived`), and any
    // launcher-side caps (in `policy_facts`). The runtime fails loud
    // if the daemon shipped a view missing the derived shapes the
    // directive kind's `composer_config` is supposed to populate.
    let header = parse_effective_header(composed_view)?;

    tracing::info!(
        directive_name = ?header.name.as_deref(),
        has_model = header.model.is_some(),
        has_permissions = header.permissions.is_some(),
        has_limits = header.limits.is_some(),
        context_position_count = header.context.as_ref().map(|c| c.len()).unwrap_or(0),
        hooks_count = header.hooks.as_ref().map(|h| h.len()).unwrap_or(0),
        "directive runtime: typed header surface"
    );

    let execution = loader
        .load_config_strict::<ExecutionConfig>("execution")
        .map_err(|e| anyhow!("loading config `execution`: {e}"))?
        .unwrap_or_default();

    let model_routing = loader
        .load_config_strict::<ModelRoutingConfig>("model_routing")
        .map_err(|e| anyhow!("loading config `model_routing`: {e}"))?;

    // Consumer for the `category` round-trip fields on each typed
    // config — keeps `pub category: Option<String>` from being dead
    // code while making operators see a parity signal between bundle
    // and overlay configs at every launch.
    tracing::info!(
        execution_category = ?execution.category.as_deref(),
        model_routing_category = ?model_routing.as_ref().and_then(|r| r.category.as_deref()),
        model_routing_tier_count = model_routing.as_ref().map(|r| r.tiers.len()).unwrap_or(0),
        "directive runtime: typed config metadata"
    );

    let provider = resolve_provider(&header, &model_routing, loader)?;
    let (model_name, context_window) = resolve_model(&header, &model_routing)?;

    tracing::info!(
        provider_category = ?provider.category.as_deref(),
        model_name = %model_name,
        "directive runtime: provider resolved"
    );

    // Tools come from the daemon-baked `LaunchEnvelope.inventory` —
    // the directive-runtime never re-scans, re-parses, or extracts
    // tool metadata. The daemon owns the engine + parser dispatcher
    // and is the single source of truth for kind-aware work
    // (`Engine::build_inventory_for_launching_kind`). Anything missing
    // from the inventory is treated as "no such tool" by the
    // dispatcher's per-call resolution.
    let tools = project_tool_inventory(inventory);
    let hooks = load_hooks(loader)?;

    let risk_policy = load_risk_policy(loader)?;

    let context_positions: HashMap<String, Vec<String>> =
        composed_view.derived_string_seq_map(DERIVED_COMPOSED_CONTEXT);

    // Context rendering: read pre-rendered strings from the daemon's
    // compose_context_positions launch augmentation. The augmentation
    // runs between resolution and parent runtime spawn, pre-resolves
    // knowledge refs, dispatches a child knowledge-runtime thread, and
    // writes rendered strings into the parent's composed view.
    //
    // Hard contract: if the directive declared a non-empty `context:`
    // block, the daemon MUST have populated `rendered_contexts` in the
    // derived map. Missing/malformed = daemon bug. No silent fallback
    // — the user must see the error and fix the root cause.
    let any_non_empty = context_positions.values().any(|v| !v.is_empty());

    let rendered: HashMap<String, String> = if any_non_empty {
        let obj = composed_view
            .derived
            .get("rendered_contexts")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "directive runtime: directive declares non-empty context positions \
                     ({positions:?}) but composed view is missing `rendered_contexts` \
                     — the daemon's compose_context_positions launch augmentation must \
                     have failed silently or not run",
                    positions = context_positions.keys().collect::<Vec<_>>()
                )
            })?
            .as_object()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "directive runtime: `rendered_contexts` in composed view must be \
                     an object"
                )
            })?;
        obj.iter()
            .map(|(k, v)| {
                let s = v.as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "directive runtime: rendered_contexts[{k}] must be a string, got {v:?}"
                    )
                })?;
                Ok((k.clone(), s.to_string()))
            })
            .collect::<Result<HashMap<_, _>>>()?
    } else {
        HashMap::new()
    };

    let system_prompt = rendered.get("system").cloned();
    let context_before = rendered.get("before").cloned();
    let context_after = rendered.get("after").cloned();

    let user_prompt = composed_view
        .derived_string(DERIVED_BODY)
        .ok_or_else(|| {
            anyhow!(
                "directive runtime: composed view missing derived `{DERIVED_BODY}` — \
                 the directive kind schema's composer_config must declare a field rule \
                 with `derive_as: {DERIVED_BODY}`"
            )
        })?
        .to_string();

    Ok(BootstrapOutput {
        config: BootstrapConfig {
            execution,
            model_routing,
            provider: Some(provider.clone()),
            tools,
            system_prompt,
            user_prompt,
            context_before,
            context_after,
            context_positions,
            hooks,
            risk_policy,
        },
        provider,
        model_name,
        context_window,
    })
}

/// Project the engine-supplied composed view down to the keys the
/// runtime actually consumes, then deserialise into the typed
/// `DirectiveHeader`.
///
/// The composer ships every frontmatter key it preserved (e.g.
/// `body`, `category`, `description`, `inputs`, `required_secrets`)
/// in `view.composed`. Most of those are owned by the daemon — the
/// dispatcher reads `required_secrets` to populate vault bindings,
/// the engine reads `category` for kind routing, etc. The runtime
/// only needs the model/permissions/limits/outputs/context/hooks
/// surface, so we project explicitly and let `deny_unknown_fields`
/// keep catching drift in the keys we *do* own.
fn parse_effective_header(view: &KindComposedView) -> Result<DirectiveHeader> {
    let projected = match view.composed.as_object() {
        Some(map) => {
            let mut out = serde_json::Map::with_capacity(DIRECTIVE_HEADER_RUNTIME_KEYS.len());
            for &key in DIRECTIVE_HEADER_RUNTIME_KEYS {
                if let Some(v) = map.get(key) {
                    out.insert(key.to_string(), v.clone());
                }
            }
            serde_json::Value::Object(out)
        }
        // Non-object composed views are unexpected — fall through to
        // the typed deserialise so the error names the actual cause.
        None => view.composed.clone(),
    };
    serde_json::from_value(projected).map_err(|e| {
        anyhow!("deserialize composed view into DirectiveHeader: {e}")
    })
}

fn resolve_provider(
    directive: &DirectiveHeader,
    routing: &Option<ModelRoutingConfig>,
    loader: &VerifiedLoader,
) -> Result<ProviderConfig> {
    let tier = directive
        .model
        .as_ref()
        .and_then(|m| m.tier.as_deref())
        .unwrap_or("general");

    // No silent fallback. Pre-V5.5 code defaulted to "anthropic" when
    // neither the directive nor the routing supplied a provider id —
    // that masked the routing-config regression P3b debug surfaced
    // (model_routing.yaml not loading) for 30+ minutes by producing a
    // misleading "provider config not found: model-providers/anthropic"
    // error. The runtime MUST name the actual missing precondition.
    let provider_id = directive
        .model
        .as_ref()
        .and_then(|m| m.provider.as_deref())
        .map(str::to_string)
        .or_else(|| {
            routing
                .as_ref()
                .and_then(|r| r.tiers.get(tier))
                .map(|t| t.provider.clone())
        })
        .ok_or_else(|| {
            anyhow!(
                "cannot resolve provider: directive specifies no `model.provider` \
                 and no model_routing entry was available for tier `{tier}` \
                 (load model_routing.yaml under .ai/config/rye-runtime/ in \
                 system / user / project space, or set `model.provider` on the \
                 directive)"
            )
        })?;

    let config_id = format!("model-providers/{provider_id}");

    loader
        .load_config_strict::<ProviderConfig>(&config_id)
        .map_err(|e| anyhow!("loading provider config `{config_id}`: {e}"))?
        .ok_or_else(|| {
            anyhow!(
                "provider config not found: `{config_id}` — expected \
                 `.ai/config/rye-runtime/{config_id}.yaml` under one of \
                 system / user / project roots (resolved provider id from \
                 {source})",
                source = if directive
                    .model
                    .as_ref()
                    .and_then(|m| m.provider.as_deref())
                    .is_some()
                {
                    "directive's `model.provider`"
                } else {
                    "model_routing tier"
                }
            )
        })
}

fn resolve_model(
    directive: &DirectiveHeader,
    routing: &Option<ModelRoutingConfig>,
) -> anyhow::Result<(String, u64)> {
    if let Some(ref model) = directive.model {
        if let Some(ref name) = model.name {
            return Ok((name.clone(), 200_000));
        }
    }

    let tier = directive
        .model
        .as_ref()
        .and_then(|m| m.tier.as_deref())
        .unwrap_or("general");

    if let Some(ref routing) = routing {
        if let Some(tier_cfg) = routing.tiers.get(tier) {
            // The bare provider model id goes on the wire — the
            // routing's `provider` field selects WHICH
            // `model-providers/<id>.yaml` to load, not which prefix to
            // prepend to the model name. Pre-V5.5 code emitted
            // `<provider>/<model>` which only matched OpenRouter-style
            // gateways and broke direct OpenAI / Anthropic / Zen.
            let ctx = tier_cfg.context_window.unwrap_or(200_000);
            return Ok((tier_cfg.model.clone(), ctx));
        }
    }

    anyhow::bail!(
        "model unresolvable: directive has no model.name and model_routing \
         has no tier \"{tier}\". Set `model.name` on the directive or add \
         tier \"{tier}\" to model_routing.yaml."
    )
}

/// Project the daemon-baked inventory of `kind:tool` items shipped in
/// `LaunchEnvelope.inventory["tool"]` into the runtime's typed
/// `ToolSchema` shape.
///
/// The directive-runtime no longer scans, parses, or extracts tool
/// metadata. The daemon already owns the engine + parser dispatcher
/// and bakes the inventory via `Engine::build_inventory_for_launching_kind`
/// before spawning. Hard-failing on missing inventory would penalise
/// the legitimate "directive declares tools but the LLM never calls
/// any" case, so we treat an absent `tool` entry as "no tools declared"
/// and let the dispatcher's per-call `unknown tool` path surface any
/// genuine misconfiguration.
fn project_tool_inventory(
    inventory: &std::collections::HashMap<String, Vec<ryeos_runtime::envelope::ItemDescriptor>>,
) -> Vec<ToolSchema> {
    inventory
        .get("tool")
        .map(|items| {
            items
                .iter()
                .map(|d| ToolSchema {
                    name: d.name.clone(),
                    item_id: d.item_id.clone(),
                    description: d.description.clone(),
                    input_schema: d.schema.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn load_hooks(loader: &VerifiedLoader) -> Result<Vec<ryeos_runtime::HookDefinition>> {
    let mut hooks = Vec::new();

    // `hook_conditions` config is optional — its absence is fine. But
    // *any* parse / verification failure is a hard error: silently
    // dropping a malformed hooks file masked critical safety policy.
    let items = loader
        .scan_kind("config")
        .map_err(|e| anyhow::anyhow!("load_hooks: cannot enumerate kind `config`: {e}"))?;

    let hook_files = ["hook_conditions"];
    for hook_name in &hook_files {
        let Some(item) = items.iter().find(|i| i.name == *hook_name) else {
            continue;
        };
        let verified = loader.load_verified("config", &item.path).map_err(|e| {
            anyhow::anyhow!(
                "load_hooks: cannot load `{hook_name}` at {}: {e}",
                item.path.display()
            )
        })?;
        let config: serde_yaml::Value =
            serde_yaml::from_str(&verified.content).map_err(|e| {
                anyhow::anyhow!(
                    "load_hooks: parse YAML for `{hook_name}` at {}: {e}",
                    item.path.display()
                )
            })?;
        // `builtin_hooks` is optional, but if present it MUST be a YAML
        // sequence. The previous `.and_then(as_sequence)` silently
        // skipped a malformed file — masking critical safety policy.
        let hooks_value = match config.get("builtin_hooks") {
            None | Some(serde_yaml::Value::Null) => continue,
            Some(v) => v,
        };
        let hooks_arr = hooks_value.as_sequence().ok_or_else(|| {
            anyhow::anyhow!(
                "load_hooks: `builtin_hooks` in `{hook_name}` at {} must be a YAML sequence, \
                 got {}",
                item.path.display(),
                yaml_kind(hooks_value)
            )
        })?;
        for (idx, h) in hooks_arr.iter().enumerate() {
            let hook = serde_yaml::from_value::<ryeos_runtime::HookDefinition>(h.clone())
                .map_err(|e| {
                    anyhow::anyhow!(
                        "load_hooks: malformed hook[{idx}] in `{hook_name}` at {}: {e}",
                        item.path.display()
                    )
                })?;
            hooks.push(hook);
        }
    }

    Ok(hooks)
}

fn load_risk_policy(loader: &VerifiedLoader) -> Result<Option<crate::harness::RiskPolicy>> {
    // Absence of capability_risk.yaml is fine (optional config).
    // But if it exists and is broken, we fail loud with file path.
    let Some(config) = loader
        .load_config_strict::<serde_yaml::Value>("capability_risk")
        .map_err(|e| anyhow!("loading config `capability_risk`: {e}"))?
    else {
        return Ok(None);
    };

    // Now we have a valid YAML value. Enforce structure.
    let policies = config
        .get("policies")
        .ok_or_else(|| {
            anyhow!(
                "capability_risk config: missing required key `policies`"
            )
        })?
        .as_sequence()
        .ok_or_else(|| {
            anyhow!(
                "capability_risk config: `policies` must be a YAML sequence, got {}",
                yaml_kind(config.get("policies").unwrap())
            )
        })?;

    let mut risk_patterns = Vec::new();
    for (idx, policy_entry) in policies.iter().enumerate() {
        let entry_mapping = policy_entry.as_mapping().ok_or_else(|| {
            anyhow!(
                "capability_risk config: policies[{idx}] must be a mapping, got {}",
                yaml_kind(policy_entry)
            )
        })?;

        // Read required fields with typed errors, not silent defaults.
        // Policy rules are safety-critical; silent defaults are risky.
        let pattern = entry_mapping
            .get(&serde_yaml::Value::String("pattern".into()))
            .ok_or_else(|| {
                anyhow!(
                    "capability_risk config: policies[{idx}].pattern is required"
                )
            })?
            .as_str()
            .ok_or_else(|| {
                anyhow!(
                    "capability_risk config: policies[{idx}].pattern must be a string"
                )
            })?
            .to_string();

        let level = entry_mapping
            .get(&serde_yaml::Value::String("level".into()))
            .ok_or_else(|| {
                anyhow!(
                    "capability_risk config: policies[{idx}].level is required"
                )
            })?
            .as_str()
            .ok_or_else(|| {
                anyhow!(
                    "capability_risk config: policies[{idx}].level must be a string"
                )
            })?
            .to_string();

        let requires_ack = entry_mapping
            .get(&serde_yaml::Value::String("requires_ack".into()))
            .ok_or_else(|| {
                anyhow!(
                    "capability_risk config: policies[{idx}].requires_ack is required"
                )
            })?
            .as_bool()
            .ok_or_else(|| {
                anyhow!(
                    "capability_risk config: policies[{idx}].requires_ack must be a bool"
                )
            })?;

        risk_patterns.push(crate::harness::RiskPattern {
            pattern,
            level,
            requires_ack,
        });
    }

    Ok(Some(crate::harness::RiskPolicy {
        patterns: risk_patterns,
    }))
}

fn yaml_kind(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "bool",
        serde_yaml::Value::Number(_) => "number",
        serde_yaml::Value::String(_) => "string",
        serde_yaml::Value::Sequence(_) => "sequence",
        serde_yaml::Value::Mapping(_) => "mapping",
        serde_yaml::Value::Tagged(_) => "tagged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_from_directive_name() {
        let header = DirectiveHeader {
            model: Some(ModelSpec {
                tier: None,
                provider: None,
                name: Some("gpt-4".to_string()),
            }),
            ..Default::default()
        };
        let (name, _) = resolve_model(&header, &None).unwrap();
        assert_eq!(name, "gpt-4");
    }

    #[test]
    fn resolve_model_from_routing() {
        let header = DirectiveHeader {
            model: Some(ModelSpec {
                tier: Some("fast".to_string()),
                provider: None,
                name: None,
            }),
            ..Default::default()
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
        let (name, ctx) = resolve_model(&header, &Some(routing)).unwrap();
        assert_eq!(name, "gpt-4o-mini");
        assert_eq!(ctx, 128_000);
    }

    #[test]
    fn resolve_model_errors_when_unresolvable() {
        let header = DirectiveHeader::default();
        let result = resolve_model(&header, &None);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("model unresolvable"), "error message: {msg}");
        assert!(msg.contains("general"), "error should mention the default tier");
    }
}
