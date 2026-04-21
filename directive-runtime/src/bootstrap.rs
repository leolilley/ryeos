use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::directive::*;
use rye_runtime::verified_loader::VerifiedLoader;

pub struct BootstrapOutput {
    pub config: BootstrapConfig,
    pub provider: ProviderConfig,
    pub model_name: String,
    pub context_window: u64,
}

struct ComposedDirective {
    header: DirectiveHeader,
    body: String,
}

fn resolve_extends_chain(
    directive: &ParsedDirective,
    loader: &VerifiedLoader,
) -> Result<ComposedDirective> {
    let mut chain = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut current_header = directive.header.clone();
    let mut max_depth = 8;

    while let Some(ref extends) = current_header.extends {
        if max_depth == 0 {
            bail!("extends chain exceeds depth limit of 8");
        }
        if !visited.insert(extends.clone()) {
            bail!("cycle detected in extends chain: {}", extends);
        }

        let resolved = loader.resolve_item("directive", extends)?;
        let verified = loader.load_verified("directive", &resolved.path)?;
        let parent = crate::parser::parse_directive(&verified.content, &resolved.path.to_string_lossy())?;

        chain.push(parent.header.clone());
        current_header = parent.header;
        max_depth -= 1;
    }

    chain.reverse();

    let final_header = if directive.header.permissions.is_none() {
        let mut header = directive.header.clone();
        for parent_header in &chain {
            if parent_header.permissions.is_some() {
                header.permissions = parent_header.permissions.clone();
                break;
            }
        }
        header
    } else {
        directive.header.clone()
    };

    let mut composed_context = HashMap::new();
    for parent_header in &chain {
        if let Some(ref ctx) = parent_header.context {
            for (pos, items) in ctx {
                composed_context.entry(pos.clone())
                    .or_insert_with(Vec::new)
                    .extend(items.clone());
            }
        }
    }
    if let Some(ref ctx) = directive.header.context {
        for (pos, items) in ctx {
            composed_context.entry(pos.clone())
                .or_insert_with(Vec::new)
                .extend(items.clone());
        }
    }

    let header_with_context = DirectiveHeader {
        context: Some(composed_context),
        ..final_header
    };

    tracing::info!(
        chain_depth = chain.len(),
        chain_items = ?chain.iter().map(|h| h.extends.as_deref().unwrap_or("(root)")).collect::<Vec<_>>(),
        "extends chain resolved"
    );

    Ok(ComposedDirective {
        header: header_with_context,
        body: directive.body.clone(),
    })
}

pub fn bootstrap(
    _project_root: &Path,
    _user_root: Option<&Path>,
    _system_roots: &[PathBuf],
    directive: &ParsedDirective,
    _envelope_limits: &rye_runtime::envelope::HardLimits,
    loader: &VerifiedLoader,
) -> Result<BootstrapOutput> {
    let composed = resolve_extends_chain(directive, loader)?;

    let execution = loader.load_config::<ExecutionConfig>("execution").unwrap_or_default();
    let model_routing = loader.load_config::<ModelRoutingConfig>("model_routing");
    let provider = resolve_provider(&composed.header, &model_routing, loader)?;
    let (model_name, context_window) = resolve_model(&composed.header, &model_routing);

    let tools = scan_tools(loader);
    let hooks = load_hooks(loader);

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

    let context_positions = composed.header.context.clone().unwrap_or_default();

    let system_prompt = render_context_position(&context_positions, "system", loader);
    let context_before = render_context_position(&context_positions, "before", loader);
    let context_after = render_context_position(&context_positions, "after", loader);

    let user_prompt = composed.body.clone();

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

fn render_context_position(
    positions: &HashMap<String, Vec<String>>,
    position: &str,
    loader: &VerifiedLoader,
) -> Option<String> {
    let items = positions.get(position)?;
    let mut rendered = Vec::new();
    for item_id in items {
        if let Ok(resolved) = loader.resolve_item("knowledge", item_id) {
            if let Ok(verified) = loader.load_verified("knowledge", &resolved.path) {
                rendered.push(verified.content);
            }
        }
    }
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join("\n\n---\n\n"))
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

fn scan_tools(loader: &VerifiedLoader) -> Vec<ToolSchema> {
    let mut tools = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Ok(items) = loader.scan_kind("tool") {
        for item in &items {
            let name = item.name.clone();
            if name.is_empty() || seen.contains(&name) {
                continue;
            }
            if let Ok(verified) = loader.load_verified("tool", &item.path) {
                if let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&verified.content) {
                    seen.insert(name.clone());
                    let rel = item.path.strip_prefix(&item.root).unwrap_or(&item.path);
                    let schema_val = doc.get("input_schema").or_else(|| doc.get("parameters"));
                    let input_schema = schema_val.and_then(|v| serde_json::to_value(v).ok());
                    tools.push(ToolSchema {
                        name: name.clone(),
                        item_id: format!("tool:{}", rel.display()),
                        description: doc.get("description").and_then(|v| v.as_str()).map(String::from),
                        input_schema,
                    });
                }
            }
        }
    }

    tools
}

fn load_hooks(loader: &VerifiedLoader) -> Vec<rye_runtime::HookDefinition> {
    let mut hooks = Vec::new();

    if let Ok(items) = loader.scan_kind("config") {
        let hook_files = ["hook_conditions"];
        for hook_name in &hook_files {
            let resolved = items.iter().find(|i| i.name == *hook_name);
            if let Some(item) = resolved {
                if let Ok(verified) = loader.load_verified("config", &item.path) {
                    if let Ok(config) = serde_yaml::from_str::<serde_yaml::Value>(&verified.content) {
                        if let Some(hooks_arr) = config.get("builtin_hooks").and_then(|v| v.as_sequence()) {
                            for h in hooks_arr {
                                if let Ok(hook) = serde_yaml::from_value::<rye_runtime::HookDefinition>(h.clone()) {
                                    hooks.push(hook);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    hooks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_from_directive_name() {
        let directive = ParsedDirective {
            header: DirectiveHeader {
                model: Some(ModelSpec {
                    tier: None,
                    provider: None,
                    name: Some("gpt-4".to_string()),
                }),
                ..Default::default()
            },
            body: String::new(),
        };
        let (name, _) = resolve_model(&directive.header, &None);
        assert_eq!(name, "gpt-4");
    }

    #[test]
    fn resolve_model_from_routing() {
        let directive = ParsedDirective {
            header: DirectiveHeader {
                model: Some(ModelSpec {
                    tier: Some("fast".to_string()),
                    provider: None,
                    name: None,
                }),
                ..Default::default()
            },
            body: String::new(),
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
        let (name, ctx) = resolve_model(&directive.header, &Some(routing));
        assert_eq!(name, "openai/gpt-4o-mini");
        assert_eq!(ctx, 128_000);
    }

    #[test]
    fn resolve_model_default() {
        let directive = ParsedDirective {
            header: DirectiveHeader::default(),
            body: String::new(),
        };
        let (name, _) = resolve_model(&directive.header, &None);
        assert!(name.contains("anthropic"));
    }
}
