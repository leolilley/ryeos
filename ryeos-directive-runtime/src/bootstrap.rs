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
) -> Result<BootstrapOutput> {
    // The runtime no longer composes — the daemon-side composer has
    // already produced the effective header (in `composed`), the
    // body and context positions (in `derived`), and any
    // launcher-side caps (in `policy_facts`). The runtime fails loud
    // if the daemon shipped a view missing the derived shapes the
    // directive kind's `composer_config` is supposed to populate.
    let header = parse_effective_header(composed_view)?;

    let execution = loader.load_config::<ExecutionConfig>("execution").unwrap_or_default();
    let model_routing = loader.load_config::<ModelRoutingConfig>("model_routing");
    let provider = resolve_provider(&header, &model_routing, loader)?;
    let (model_name, context_window) = resolve_model(&header, &model_routing);

    // Strict tool / hook loading: any unparseable tool YAML or
    // unreadable hook config is a hard error. Silent drops here meant
    // a malformed tool would simply vanish from the runtime's tool
    // list, leaving the user wondering why a declared tool wasn't
    // available.
    let tools = scan_tools(loader)?;
    let hooks = load_hooks(loader)?;

    let risk_policy: Option<crate::harness::RiskPolicy> = loader
        .load_config::<serde_yaml::Value>("capability_risk")
        .and_then(|val| {
            let patterns = val.get("policies").and_then(|p| p.as_sequence())?;
            let mut risk_patterns = Vec::new();
            for p in patterns {
                let pattern = p.get("pattern").and_then(|pv| pv.as_str()).unwrap_or("").to_string();
                let level = p.get("level").and_then(|pv| pv.as_str()).unwrap_or("medium").to_string();
                let requires_ack = p.get("requires_ack").and_then(|pv| pv.as_bool()).unwrap_or(false);
                risk_patterns.push(crate::harness::RiskPattern { pattern, level, requires_ack });
            }
            Some(crate::harness::RiskPolicy { patterns: risk_patterns })
        });

    let context_positions: HashMap<String, Vec<String>> =
        composed_view.derived_string_seq_map(DERIVED_COMPOSED_CONTEXT);

    // Strict context rendering: any declared knowledge item that fails
    // to resolve / verify / load is a hard error. The previous code
    // silently dropped unresolvable items and emitted a partial prompt,
    // which masked typo'd refs and missing knowledge files.
    let system_prompt = render_context_position(&context_positions, "system", loader)?;
    let context_before = render_context_position(&context_positions, "before", loader)?;
    let context_after = render_context_position(&context_positions, "after", loader)?;

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

/// Deserialize the engine-supplied effective header back into the
/// runtime's typed `DirectiveHeader`. The engine ships JSON because it
/// doesn't (and shouldn't) know every directive header field; the
/// runtime owns the typed view of fields it actually consumes
/// (model, hooks, etc.).
fn parse_effective_header(view: &KindComposedView) -> Result<DirectiveHeader> {
    serde_json::from_value(view.composed.clone()).map_err(|e| {
        anyhow!("deserialize composed view into DirectiveHeader: {e}")
    })
}

fn render_context_position(
    positions: &HashMap<String, Vec<String>>,
    position: &str,
    loader: &VerifiedLoader,
) -> Result<Option<String>> {
    let Some(items) = positions.get(position) else {
        return Ok(None);
    };
    let mut rendered = Vec::with_capacity(items.len());
    for item_id in items {
        let resolved = loader.resolve_item("knowledge", item_id).map_err(|e| {
            anyhow::anyhow!(
                "context[{position}]: cannot resolve knowledge `{item_id}`: {e}"
            )
        })?;
        let verified = loader
            .load_verified("knowledge", &resolved.path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "context[{position}]: cannot load knowledge `{item_id}` at {}: {e}",
                    resolved.path.display()
                )
            })?;
        rendered.push(verified.content);
    }
    if rendered.is_empty() {
        Ok(None)
    } else {
        Ok(Some(rendered.join("\n\n---\n\n")))
    }
}

fn resolve_provider(
    directive: &DirectiveHeader,
    routing: &Option<ModelRoutingConfig>,
    loader: &VerifiedLoader,
) -> Result<ProviderConfig> {
    let provider_id = directive
        .model
        .as_ref()
        .and_then(|m| m.provider.as_deref())
        .or_else(|| {
            routing
                .as_ref()
                .and_then(|r| {
                    let tier = directive
                        .model
                        .as_ref()
                        .and_then(|m| m.tier.as_deref())
                        .unwrap_or("general");
                    r.tiers.get(tier).map(|t| t.provider.as_str())
                })
        })
        .unwrap_or("anthropic");

    let config_id = format!("model_providers/{provider_id}");

    loader.load_config::<ProviderConfig>(&config_id)
        .ok_or_else(|| anyhow::anyhow!("provider config not found: {}", config_id))
}

fn resolve_model(
    directive: &DirectiveHeader,
    routing: &Option<ModelRoutingConfig>,
) -> (String, u64) {
    if let Some(ref model) = directive.model {
        if let Some(ref name) = model.name {
            return (name.clone(), 200_000);
        }
    }

    let tier = directive
        .model
        .as_ref()
        .and_then(|m| m.tier.as_deref())
        .unwrap_or("general");

    if let Some(ref routing) = routing {
        if let Some(tier_cfg) = routing.tiers.get(tier) {
            let model_name = format!("{}/{}", tier_cfg.provider, tier_cfg.model);
            let ctx = tier_cfg.context_window.unwrap_or(200_000);
            return (model_name, ctx);
        }
    }

    (format!("anthropic/claude-sonnet-4-20250514"), 200_000)
}

fn scan_tools(loader: &VerifiedLoader) -> Result<Vec<ToolSchema>> {
    let mut tools = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let items = loader
        .scan_kind("tool")
        .map_err(|e| anyhow::anyhow!("scan_tools: cannot enumerate kind `tool`: {e}"))?;

    for item in &items {
        let name = item.name.clone();
        if name.is_empty() {
            return Err(anyhow::anyhow!(
                "scan_tools: tool at {} has empty `name`",
                item.path.display()
            ));
        }
        if seen.contains(&name) {
            // First-write-wins (shadowing project > user > system) is
            // intentional — silently ignoring later duplicates is the
            // correct precedence behaviour, not a "drop". Continue.
            continue;
        }
        let verified = loader.load_verified("tool", &item.path).map_err(|e| {
            anyhow::anyhow!("scan_tools: load `{name}` at {}: {e}", item.path.display())
        })?;
        let doc: serde_yaml::Value = serde_yaml::from_str(&verified.content).map_err(|e| {
            anyhow::anyhow!(
                "scan_tools: parse YAML for `{name}` at {}: {e}",
                item.path.display()
            )
        })?;
        seen.insert(name.clone());
        let rel = item.path.strip_prefix(&item.root).unwrap_or(&item.path);
        let schema_val = doc.get("input_schema").or_else(|| doc.get("parameters"));
        let input_schema = match schema_val {
            None => None,
            Some(v) => Some(serde_json::to_value(v).map_err(|e| {
                anyhow::anyhow!("scan_tools: schema for `{name}` not JSON-able: {e}")
            })?),
        };
        tools.push(ToolSchema {
            name: name.clone(),
            item_id: format!("tool:{}", rel.display()),
            description: doc.get("description").and_then(|v| v.as_str()).map(String::from),
            input_schema,
        });
    }

    Ok(tools)
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
        let (name, _) = resolve_model(&header, &None);
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
        let (name, ctx) = resolve_model(&header, &Some(routing));
        assert_eq!(name, "openai/gpt-4o-mini");
        assert_eq!(ctx, 128_000);
    }

    #[test]
    fn resolve_model_default() {
        let header = DirectiveHeader::default();
        let (name, _) = resolve_model(&header, &None);
        assert!(name.contains("anthropic"));
    }
}
