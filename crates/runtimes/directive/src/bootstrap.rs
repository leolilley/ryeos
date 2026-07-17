use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::directive::*;
use ryeos_directive_core::ResolvedProviderSnapshot;
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
    pub provider_id: String,
    pub model_name: String,
    pub context_window: u64,
    pub sampling: Option<SamplingConfig>,
}

/// Filebundle roots needed for bootstrap resolution.
#[allow(dead_code)] // Used by future bootstrap features (context loading, etc.)
pub struct BootstrapRoots<'a> {
    pub project_root: &'a Path,
    pub bundle_roots: &'a [PathBuf],
}

pub fn bootstrap(
    _roots: &BootstrapRoots<'_>,
    composed_view: &KindComposedView,
    _envelope_limits: &ryeos_runtime::envelope::HardLimits,
    loader: &VerifiedLoader,
    inventory: &HashMap<String, Vec<ryeos_runtime::envelope::ItemDescriptor>>,
    provider_snapshot: &ResolvedProviderSnapshot,
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
        has_limits = header.limits.is_some(),
        context_position_count = header.context.as_ref().map(|c| c.len()).unwrap_or(0),
        hooks_count = header.hooks.as_ref().map(|h| h.len()).unwrap_or(0),
        "directive runtime: typed header surface"
    );

    let execution = loader
        .load_config_strict::<ExecutionConfig>("ryeos-runtime/execution")
        .map_err(|e| anyhow!("loading config `ryeos-runtime/execution`: {e}"))?
        .unwrap_or_default();

    // Continuation behavior config (context-threshold ratio, carry bounds) —
    // loaded the same way as execution config, defaults if absent. Directive-
    // runtime config, not executor limit resolution: no envelope threading.
    let continuation_runtime = loader
        .load_config_strict::<crate::directive::ContinuationRuntimeConfig>(
            "ryeos-runtime/continuation",
        )
        .map_err(|e| anyhow!("loading config `ryeos-runtime/continuation`: {e}"))?
        .unwrap_or_default();

    // Consumer for the `category` round-trip fields on each typed
    // config — keeps `pub category: Option<String>` from being dead
    // code while making operators see a parity signal between bundle
    // and overlay configs at every launch.
    tracing::info!(
        execution_category = ?execution.category.as_deref(),
        "directive runtime: typed config metadata"
    );

    // Provider config is consumed from the launch-preparer snapshot
    // — the runtime never re-reads provider YAML from disk.
    tracing::info!(
        provider_id = %provider_snapshot.provider_id,
        model_name = %provider_snapshot.model_name,
        context_window = provider_snapshot.context_window,
        matched_profile = ?provider_snapshot.matched_profile,
        config_value_digest = %provider_snapshot.config_value_digest,
        config_source_count = provider_snapshot.config_sources.len(),
        config_hash = %provider_snapshot.config_hash,
        "directive runtime: provider snapshot resolved"
    );

    let resolved = provider_snapshot;

    // Tools come from the daemon-baked `LaunchEnvelope.inventory` —
    // the directive-runtime never re-scans, re-parses, or extracts
    // tool metadata. The daemon owns the engine + parser dispatcher
    // and is the single source of truth for kind-aware work
    // (`Engine::build_inventory_for_launching_kind`). Anything missing
    // from the inventory is treated as "no such tool" by the
    // dispatcher's per-call resolution.
    let tools = project_tool_inventory(inventory);
    let mut directive_hooks = Vec::new();

    // Parse directive-declared hooks (header.hooks) as the authored layer.
    // Source ownership, not authored data, fixes their precedence before all
    // configured layers.
    if let Some(ref header_hooks) = header.hooks {
        if !header_hooks.is_empty() {
            tracing::info!(
                count = header_hooks.len(),
                "directive runtime: merging directive-declared hooks"
            );
            for (idx, def) in header_hooks.iter().cloned().enumerate() {
                // Dead-config: a directive-authored `continuation` hook can only
                // fire when continuation is enabled. Validate here, before the
                // merge loses header provenance, and fail loud rather than ship a
                // hook that silently never runs.
                if def.event == "continuation" && !header.continuation.enabled() {
                    return Err(anyhow::anyhow!(
                        "directive header hooks[{idx}]: a `continuation` event hook is \
                         declared but `continuation` is disabled — it can never fire. \
                         Enable `continuation` or remove the hook."
                    ));
                }
                directive_hooks.push(def);
            }
        }
    }

    // Source layers are ordered once, then compiled against the only context
    // roots the directive runner actually supplies. No raw hook definition
    // crosses the bootstrap boundary into execution.
    let hooks = compile_directive_hooks(directive_hooks, loader)?;

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
        let rendered: HashMap<String, String> = obj
            .iter()
            .map(|(k, v)| {
                let s = v.as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "directive runtime: rendered_contexts[{k}] must be a string, got {v:?}"
                    )
                })?;
                Ok((k.clone(), s.to_string()))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        // Completeness check: every declared non-empty position MUST be
        // present in the rendered output. Missing positions mean the
        // daemon's augmentation failed for that slot — the directive
        // would run without declared context, causing silent degradation.
        let expected_keys: std::collections::HashSet<String> = context_positions
            .iter()
            .filter(|(_, items)| !items.is_empty())
            .map(|(k, _)| k.clone())
            .collect();
        let rendered_keys: std::collections::HashSet<String> = rendered.keys().cloned().collect();
        let missing: Vec<&String> = expected_keys.difference(&rendered_keys).collect();
        if !missing.is_empty() {
            return Err(anyhow::anyhow!(
                "directive runtime: context rendering incomplete — declared \
                 non-empty positions {expected:?} but rendered only {rendered:?}; \
                 missing: {missing:?}. The daemon's compose_context_positions \
                 augmentation must populate every declared position",
                expected = expected_keys,
                rendered = rendered_keys,
                missing = missing,
            ));
        }

        rendered
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
            tools,
            system_prompt,
            user_prompt,
            context_before,
            context_after,
            context_positions,
            hooks,
            outputs: header.outputs,
            return_nudge: header.return_nudge,
            continuation: header.continuation,
            continuation_runtime,
            risk_policy,
        },
        provider: resolved.provider.clone(),
        provider_id: resolved.provider_id.clone(),
        model_name: resolved.model_name.clone(),
        context_window: resolved.context_window,
        sampling: resolved.sampling.clone(),
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
pub(super) fn parse_effective_header(view: &KindComposedView) -> Result<DirectiveHeader> {
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
    serde_json::from_value(projected)
        .map_err(|e| anyhow!("deserialize composed view into DirectiveHeader: {e}"))
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

fn compile_directive_hooks(
    directive_hooks: Vec<ryeos_runtime::HookDefinition>,
    loader: &VerifiedLoader,
) -> Result<Vec<ryeos_runtime::CompiledHook>> {
    let mut sources = ryeos_runtime::load_configured_hook_sources(loader)?;
    sources.authored = directive_hooks;
    sources.retain_configured_events(&["after_step", "continuation"]);
    compile_directive_hook_sources(sources)
}

pub(super) fn compile_directive_hook_sources(
    sources: ryeos_runtime::HookSources,
) -> Result<Vec<ryeos_runtime::CompiledHook>> {
    let schemas = [
        ryeos_runtime::HookContextSchema::new("after_step", ["turn"]),
        ryeos_runtime::HookContextSchema::new("continuation", ["event"]),
    ];
    ryeos_runtime::compile_hooks(
        sources,
        &schemas,
        &ryeos_runtime::CompilationLimits::default(),
    )
    .context("compile directive hooks")
}

fn load_risk_policy(loader: &VerifiedLoader) -> Result<Option<crate::harness::RiskPolicy>> {
    // Absence of ryeos-runtime/capability_risk.yaml is fine (optional config).
    // But if it exists and is broken, we fail loud with file path.
    let Some(config) = loader
        .load_config_strict::<serde_yaml::Value>("ryeos-runtime/capability_risk")
        .map_err(|e| anyhow!("loading config `ryeos-runtime/capability_risk`: {e}"))?
    else {
        return Ok(None);
    };

    // Now we have a valid YAML value. Enforce structure.
    let policies = config
        .get("policies")
        .ok_or_else(|| anyhow!("capability_risk config: missing required key `policies`"))?
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
            .get(serde_yaml::Value::String("pattern".into()))
            .ok_or_else(|| anyhow!("capability_risk config: policies[{idx}].pattern is required"))?
            .as_str()
            .ok_or_else(|| {
                anyhow!("capability_risk config: policies[{idx}].pattern must be a string")
            })?
            .to_string();

        let level_str = entry_mapping
            .get(serde_yaml::Value::String("level".into()))
            .ok_or_else(|| anyhow!("capability_risk config: policies[{idx}].level is required"))?;
        let level: crate::harness::RiskLevel = serde_yaml::from_value(level_str.clone())
            .with_context(|| {
                format!(
                    "capability_risk config: policies[{idx}].level must be \
                     one of: low, medium, high"
                )
            })?;

        let requires_ack = entry_mapping
            .get(serde_yaml::Value::String("requires_ack".into()))
            .ok_or_else(|| {
                anyhow!("capability_risk config: policies[{idx}].requires_ack is required")
            })?
            .as_bool()
            .ok_or_else(|| {
                anyhow!("capability_risk config: policies[{idx}].requires_ack must be a bool")
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
    use ryeos_runtime::{ExpressionCondition, HookDefinition, HookLayer, HookSources};
    use serde_json::json;

    fn hook(id: &str, event: &str, action: serde_json::Value) -> HookDefinition {
        HookDefinition {
            id: id.to_string(),
            event: event.to_string(),
            condition: ExpressionCondition::Absent,
            action,
        }
    }

    #[test]
    fn directive_hooks_merge_then_compile_in_layer_order() {
        let directive = hook(
            "directive",
            "after_step",
            json!({"item_id": "tool:test/directive", "params": {"turn": "${turn}"}}),
        );
        let builtin = hook(
            "builtin",
            "continuation",
            json!({"item_id": "tool:test/builtin", "params": {"event": "${event}"}}),
        );

        let compiled = compile_directive_hook_sources(HookSources {
            authored: vec![directive],
            builtin: vec![builtin],
            ..HookSources::default()
        })
        .unwrap();

        assert_eq!(compiled.len(), 2);
        assert_eq!(compiled[0].id(), "directive");
        assert_eq!(compiled[0].layer(), HookLayer::Authored);
        assert_eq!(compiled[1].id(), "builtin");
        assert_eq!(compiled[1].layer(), HookLayer::Builtin);
    }

    #[test]
    fn directive_hook_rejects_root_outside_event_schema() {
        let invalid = hook(
            "invalid",
            "after_step",
            json!({"item_id": "tool:test/invalid", "params": {"value": "${state.value}"}}),
        );

        let error = compile_directive_hook_sources(HookSources {
            authored: vec![invalid],
            ..HookSources::default()
        })
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("references undeclared root `state`"),
            "expected the after_step schema to reject state, got: {error:#}"
        );
    }
}
