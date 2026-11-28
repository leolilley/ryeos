use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};

use crate::directive::{BootstrapConfig, ExecutionConfig, ToolSchema};
use ryeos_directive_definition::{
    DirectiveHeader, ProviderConfig, ReasoningConfig, ResolvedProviderSnapshot, SamplingConfig,
};
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
#[cfg(feature = "latency-profiling")]
const DERIVED_RENDERED_CONTEXTS_META: &str = "rendered_contexts_meta";

pub struct BootstrapOutput {
    pub config: BootstrapConfig,
    pub provider: ProviderConfig,
    pub provider_id: String,
    pub model_name: String,
    pub context_window: u64,
    pub sampling: Option<SamplingConfig>,
    pub reasoning: Option<ReasoningConfig>,
}

pub fn bootstrap(
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

    let mut execution = load_execution_config(loader)?;
    execution
        .validate()
        .context("invalid config `ryeos-runtime/execution`")?;
    // Reduce the unknown-outcome policy to the ceiling the launch sealed. The
    // launch preparer derives the same number with the same function, so the
    // runner reads one value and the daemon's check agrees with it.
    let unknown_outcome_ceiling = ryeos_directive_definition::unknown_outcome_retry_ceiling(
        &header,
        &provider_snapshot.provider,
        execution.unknown_outcome_retry.as_ref(),
    );
    execution.unknown_outcome_retry = match execution.unknown_outcome_retry.take() {
        Some(policy) if unknown_outcome_ceiling > 0 => {
            Some(ryeos_directive_definition::UnknownOutcomeRetryPolicy {
                max_retries: unknown_outcome_ceiling,
                on: policy.on,
            })
        }
        _ => None,
    };

    // Continuation behavior config (context-threshold ratio, carry bounds) —
    // loaded the same way as execution config, defaults if absent. Directive-
    // runtime config, not executor limit resolution: no envelope threading.
    let continuation_runtime = load_continuation_config(loader)?;

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
    let tools = project_tool_inventory(inventory)?;
    let hooks = compile_admitted_directive_hooks(&header, composed_view)?;

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

    #[cfg(feature = "latency-profiling")]
    {
        let context_item_contributions = composed_view
            .derived
            .get(DERIVED_RENDERED_CONTEXTS_META)
            .and_then(sanitized_context_item_contributions)
            .unwrap_or_else(|| serde_json::json!({}));
        crate::startup_timing::record_context_composition(&context_item_contributions);
    }

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

    let provider_effects_recorded = header.effects.records_provider_calls();

    Ok(BootstrapOutput {
        config: BootstrapConfig {
            execution,
            provider_effects_recorded,
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
        reasoning: resolved.reasoning.clone(),
    })
}

/// Retain only signed item refs, positions, and composer token estimates. The
/// augmentation metadata value is deliberately not logged wholesale because a
/// future composer may add request-correlated or sensitive fields.
#[cfg(feature = "latency-profiling")]
fn sanitized_context_item_contributions(value: &serde_json::Value) -> Option<serde_json::Value> {
    let positions = value.as_object()?;
    let mut sanitized = serde_json::Map::new();
    for (position, composition) in positions {
        let items = composition
            .get("resolved_items")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(serde_json::json!({
                            "item_id": item.get("item_id")?.as_str()?,
                            "tokens": item.get("tokens")?.as_u64()?,
                        }))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        sanitized.insert(position.clone(), serde_json::Value::Array(items));
    }
    Some(serde_json::Value::Object(sanitized))
}

/// Invoke the directive-kind parser over the engine-supplied composed value.
/// Launch preparation uses the same function before token minting, so the
/// runtime cannot discover a second semantic interpretation after spawn.
pub(super) fn parse_effective_header(view: &KindComposedView) -> Result<DirectiveHeader> {
    ryeos_directive_definition::parse_effective_header(&view.composed)
        .map_err(|error| anyhow!("parse effective directive header: {error:#}"))
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
///
/// `time_left` names the runtime's own in-process tool, so an inventory tool
/// of that name is refused here rather than offered to the model twice.
fn project_tool_inventory(
    inventory: &std::collections::HashMap<String, Vec<ryeos_runtime::envelope::ItemDescriptor>>,
) -> Result<Vec<ToolSchema>> {
    let tools: Vec<ToolSchema> = inventory
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
        .unwrap_or_default();
    if let Some(reserved) = tools
        .iter()
        .find(|tool| tool.name == crate::directive::TIME_LEFT_TOOL)
    {
        anyhow::bail!(
            "tool `{}` uses the name `{}`, which the directive runtime reserves for its \
             built-in time-left tool",
            reserved.item_id,
            reserved.name
        );
    }
    Ok(tools)
}

fn compile_admitted_directive_hooks(
    header: &DirectiveHeader,
    composed_view: &KindComposedView,
) -> Result<Vec<ryeos_runtime::CompiledHook>> {
    let mut directive_hooks = Vec::new();
    if let Some(ref header_hooks) = header.hooks
        && !header_hooks.is_empty()
    {
        tracing::info!(
            count = header_hooks.len(),
            "directive runtime: compiling directive-declared hooks"
        );
        for definition in header_hooks.iter().cloned() {
            directive_hooks.push(definition);
        }
    }

    let plan_value = composed_view
        .derived
        .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
        .ok_or_else(|| anyhow!("directive effective definition has no captured hook plan"))?;
    let plan = ryeos_engine::hooks::EffectiveHookPlan::from_value(plan_value)
        .map_err(|error| anyhow!(error))?;
    if plan.owner_kind != "directive" || plan.authored.hooks != directive_hooks {
        return Err(anyhow!(
            "directive authored hooks differ from the captured effective hook plan"
        ));
    }
    let compiled = ryeos_runtime::compile_effective_hook_plan(
        &plan,
        &ryeos_runtime::CompilationLimits::default(),
    )
    .context("compile captured directive hook plan")?;
    refuse_hooks_on_unfired_events(&compiled)?;
    Ok(compiled)
}

/// The hook events this runtime fires. The signed kind-schema declares which
/// events a directive may hook; a schema newer than this runtime can declare
/// an event the runtime never fires, and a hook on it would sit silently
/// inert. It is refused instead, so a launch never carries a hook that cannot
/// run. A test holds this list equal to the kind-schema's events.
pub(super) const FIRED_HOOK_EVENTS: [&str; 9] = [
    "after_step",
    "continuation",
    "provider_error",
    "context_pressure",
    "before_tool",
    "after_tool",
    "turn_incomplete",
    "idle",
    "finish_check",
];

fn refuse_hooks_on_unfired_events(hooks: &[ryeos_runtime::CompiledHook]) -> Result<()> {
    match hooks
        .iter()
        .find(|hook| !FIRED_HOOK_EVENTS.contains(&hook.event()))
    {
        Some(hook) => Err(anyhow!(
            "hook_event_unknown_to_runtime: hook `{}` is on the event `{}`, which this \
             directive runtime does not fire; it ships with a newer runtime",
            hook.id(),
            hook.event()
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
pub(super) fn compile_directive_hook_sources(
    sources: ryeos_runtime::HookSources,
) -> Result<Vec<ryeos_runtime::CompiledHook>> {
    let schemas = [
        ryeos_runtime::HookContextSchema::new("after_step", ["turn"]),
        ryeos_runtime::HookContextSchema::new("continuation", ["event"]),
        ryeos_runtime::HookContextSchema::new("provider_error", ["event"]),
        ryeos_runtime::HookContextSchema::new("context_pressure", ["event"]),
        ryeos_runtime::HookContextSchema::new("before_tool", ["event"]),
        ryeos_runtime::HookContextSchema::new("after_tool", ["event"]),
        ryeos_runtime::HookContextSchema::new("turn_incomplete", ["event"]),
        ryeos_runtime::HookContextSchema::new("idle", ["event"]),
        ryeos_runtime::HookContextSchema::new("finish_check", ["event"]),
    ];
    ryeos_runtime::compile_hooks(
        sources,
        &schemas,
        &ryeos_runtime::CompilationLimits::default(),
    )
    .context("compile directive hooks")
}

/// Execution config (retries, backoff, unknown-outcome policy). Absent means
/// defaults; a present file must be signed by a signer trusted for its space.
fn load_execution_config(loader: &VerifiedLoader) -> Result<ExecutionConfig> {
    Ok(loader
        .load_config_strict_signed::<ExecutionConfig>("ryeos-runtime/execution")
        .map_err(|e| anyhow!("loading config `ryeos-runtime/execution`: {e}"))?
        .unwrap_or_default())
}

/// Continuation config (context-threshold ratio, carry bounds). Absent means
/// defaults; a present file must be signed by a signer trusted for its space.
fn load_continuation_config(
    loader: &VerifiedLoader,
) -> Result<crate::directive::ContinuationRuntimeConfig> {
    Ok(loader
        .load_config_strict_signed::<crate::directive::ContinuationRuntimeConfig>(
            "ryeos-runtime/continuation",
        )
        .map_err(|e| anyhow!("loading config `ryeos-runtime/continuation`: {e}"))?
        .unwrap_or_default())
}

fn load_risk_policy(loader: &VerifiedLoader) -> Result<Option<crate::harness::RiskPolicy>> {
    // Absence of ryeos-runtime/capability_risk.yaml is fine (optional config).
    // If it exists it must be signed by a trusted signer and well formed; any
    // other state fails loud with the file path.
    let Some(config) = loader
        .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/capability_risk")
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
    use ryeos_engine::hooks::{
        EFFECTIVE_HOOK_PLAN_SCHEMA, EffectiveHookLayer, EffectiveHookPlan, ExpressionCondition,
        HOOK_CONTEXT_SCHEMA, HookContextContract, HookEventContract,
    };
    use ryeos_runtime::{HookDefinition, HookLayer, HookResultMode, HookSources};
    use serde_json::json;

    #[test]
    fn an_inventory_tool_named_time_left_is_refused_at_load() {
        use ryeos_runtime::envelope::ItemDescriptor;
        let mut inventory = std::collections::HashMap::new();
        inventory.insert(
            "tool".to_string(),
            vec![ItemDescriptor {
                name: "time_left".to_string(),
                item_id: "tool:project/time_left".to_string(),
                ..ItemDescriptor::default()
            }],
        );
        let error = project_tool_inventory(&inventory).unwrap_err().to_string();
        assert!(
            error.contains("time_left") && error.contains("reserves"),
            "{error}"
        );

        inventory.insert(
            "tool".to_string(),
            vec![ItemDescriptor {
                name: "noop".to_string(),
                item_id: "tool:project/noop".to_string(),
                ..ItemDescriptor::default()
            }],
        );
        assert_eq!(project_tool_inventory(&inventory).unwrap().len(), 1);
    }
    use std::collections::{BTreeMap, BTreeSet};

    fn hook(id: &str, event: &str, action: serde_json::Value) -> HookDefinition {
        HookDefinition {
            id: id.to_string(),
            event: event.to_string(),
            result: HookResultMode::Control,
            condition: ExpressionCondition::Absent,
            steer: None,
            action,
        }
    }

    fn admitted_plan(authored: Vec<HookDefinition>) -> EffectiveHookPlan {
        let empty = EffectiveHookLayer::empty();
        EffectiveHookPlan {
            schema: EFFECTIVE_HOOK_PLAN_SCHEMA.to_string(),
            owner_kind: "directive".to_string(),
            event_contracts: BTreeMap::from([(
                "after_step".to_string(),
                HookEventContract {
                    context_contract: HookContextContract {
                        schema: HOOK_CONTEXT_SCHEMA.to_string(),
                        allowed_roots: BTreeSet::from(["turn".to_string()]),
                    },
                    allowed_results: BTreeSet::from([HookResultMode::Control]),
                },
            )]),
            authored: EffectiveHookLayer {
                hooks: authored,
                dispatch_caps: vec!["ryeos.execute.tool.test/directive".to_string()],
            },
            builtin: empty.clone(),
            infrastructure: empty.clone(),
            context: empty.clone(),
            operator: empty.clone(),
            project: empty,
            sources: Vec::new(),
        }
    }

    fn effective_view(authored: Vec<HookDefinition>) -> KindComposedView {
        let plan = admitted_plan(authored.clone());
        KindComposedView {
            composed: json!({"hooks": authored}),
            derived: HashMap::from([("effective_hook_plan".to_string(), plan.to_value().unwrap())]),
            policy_facts: HashMap::new(),
        }
    }

    #[test]
    fn admitted_composed_hooks_compile_only_when_the_captured_plan_matches() {
        let authored = hook(
            "directive",
            "after_step",
            json!({"item_id": "tool:test/directive", "params": {"turn": "${turn}"}}),
        );
        let view = effective_view(vec![authored]);
        let header = parse_effective_header(&view).unwrap();
        let compiled = compile_admitted_directive_hooks(&header, &view).unwrap();
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].id(), "directive");

        let mut divergent = view;
        divergent.composed["hooks"] = json!([]);
        let header = parse_effective_header(&divergent).unwrap();
        let error = compile_admitted_directive_hooks(&header, &divergent).unwrap_err();
        assert!(error.to_string().contains("differ from the captured"));
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

    mod signed_config_loads {
        use super::*;
        use ryeos_runtime::verified_loader::ConfigLoadError;

        fn key(seed: u8) -> lillux::crypto::SigningKey {
            lillux::crypto::SigningKey::from_bytes(&[seed; 32])
        }

        fn trusting(
            key: &lillux::crypto::SigningKey,
        ) -> Vec<ryeos_engine::trust::TrustedSignerEntry> {
            use base64::Engine as _;
            let verifying_key = key.verifying_key();
            vec![ryeos_engine::trust::TrustedSignerEntry {
                fingerprint: lillux::signature::compute_fingerprint(&verifying_key),
                public_key: format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode(verifying_key.to_bytes())
                ),
                label: "bootstrap-test".to_string(),
                scope: ryeos_engine::trust::SignerScope::everywhere(),
                endorsements: None,
            }]
        }

        fn write(project: &std::path::Path, config_id: &str, document: &str) {
            let path = project.join(format!(".ai/config/{config_id}.yaml"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, document).unwrap();
        }

        fn loader(
            project: &std::path::Path,
            trusted: &lillux::crypto::SigningKey,
        ) -> VerifiedLoader {
            VerifiedLoader::new(project.to_path_buf(), vec![], &trusting(trusted)).unwrap()
        }

        fn sign(body: &str, key: &lillux::crypto::SigningKey) -> String {
            lillux::signature::sign_content(body, key, "#", None)
        }

        const EXECUTION: &str = "ryeos-runtime/execution";
        const CONTINUATION: &str = "ryeos-runtime/continuation";
        const RISK: &str = "ryeos-runtime/capability_risk";
        const RISK_BODY: &str =
            "policies:\n  - pattern: \"x.*\"\n    level: high\n    requires_ack: true\n";

        /// The chain root of an error: the typed loader error inside the
        /// bootstrap's `anyhow!` rendering is checked by message, since the
        /// bootstrap wraps it in context text.
        fn assert_refused(message: &str, needle: &str, file: &str) {
            assert!(message.contains(needle), "got {message}");
            assert!(message.contains(file), "must name the file: {message}");
            assert!(message.contains("Cure:"), "must name the cure: {message}");
        }

        #[test]
        fn absent_optional_configs_are_absent_not_errors() {
            let tmp = tempfile::tempdir().unwrap();
            let loader = loader(tmp.path(), &key(1));
            load_execution_config(&loader).unwrap();
            load_continuation_config(&loader).unwrap();
            assert!(load_risk_policy(&loader).unwrap().is_none());
        }

        #[test]
        fn trusted_signed_configs_load() {
            let tmp = tempfile::tempdir().unwrap();
            let k = key(1);
            write(tmp.path(), EXECUTION, &sign("retries: 7\n", &k));
            write(tmp.path(), CONTINUATION, &sign("{}\n", &k));
            write(tmp.path(), RISK, &sign(RISK_BODY, &k));
            let loader = loader(tmp.path(), &k);
            assert_eq!(load_execution_config(&loader).unwrap().retries, 7);
            load_continuation_config(&loader).unwrap();
            assert_eq!(
                load_risk_policy(&loader).unwrap().unwrap().patterns.len(),
                1
            );
        }

        #[test]
        fn unsigned_configs_are_refused() {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), EXECUTION, "retries: 7\n");
            write(tmp.path(), CONTINUATION, "{}\n");
            write(tmp.path(), RISK, RISK_BODY);
            let loader = loader(tmp.path(), &key(1));
            for (err, file) in [
                (
                    load_execution_config(&loader).unwrap_err(),
                    "execution.yaml",
                ),
                (
                    load_continuation_config(&loader).unwrap_err(),
                    "continuation.yaml",
                ),
                (
                    load_risk_policy(&loader).unwrap_err(),
                    "capability_risk.yaml",
                ),
            ] {
                assert_refused(&format!("{err:#}"), "is unsigned", file);
            }
        }

        #[test]
        fn untrusted_signer_configs_are_refused() {
            let tmp = tempfile::tempdir().unwrap();
            let stranger = key(2);
            write(tmp.path(), EXECUTION, &sign("retries: 7\n", &stranger));
            write(tmp.path(), CONTINUATION, &sign("{}\n", &stranger));
            write(tmp.path(), RISK, &sign(RISK_BODY, &stranger));
            let fingerprint = lillux::signature::compute_fingerprint(&stranger.verifying_key());
            let loader = loader(tmp.path(), &key(1));
            for (err, file) in [
                (
                    load_execution_config(&loader).unwrap_err(),
                    "execution.yaml",
                ),
                (
                    load_continuation_config(&loader).unwrap_err(),
                    "continuation.yaml",
                ),
                (
                    load_risk_policy(&loader).unwrap_err(),
                    "capability_risk.yaml",
                ),
            ] {
                let message = format!("{err:#}");
                assert_refused(&message, "not trusted", file);
                assert!(
                    message.contains(&fingerprint),
                    "must name the signer: {message}"
                );
            }
        }

        #[test]
        fn typed_error_is_the_loader_variant() {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), EXECUTION, "retries: 7\n");
            let error = loader(tmp.path(), &key(1))
                .load_config_strict_signed::<ExecutionConfig>(EXECUTION)
                .unwrap_err();
            assert!(matches!(error, ConfigLoadError::Unsigned { .. }));
        }
    }
}

#[cfg(test)]
mod fired_hook_events_tests {
    use super::*;

    fn compiled(event: &str) -> Vec<ryeos_runtime::CompiledHook> {
        let source: ryeos_runtime::HookDefinition = serde_yaml::from_str(&format!(
            "id: probe\nevent: {event}\nresult: observation\naction:\n  primary: execute\n  item_id: \"tool:marker/marker\"\n  ref_bindings: {{}}\n  params: {{}}\n"
        ))
        .unwrap();
        ryeos_runtime::compile_hooks(
            ryeos_runtime::HookSources {
                authored: vec![source],
                ..ryeos_runtime::HookSources::default()
            },
            &[ryeos_runtime::HookContextSchema::new(event, ["event"])],
            &ryeos_runtime::CompilationLimits::default(),
        )
        .unwrap()
    }

    #[test]
    fn a_hook_on_an_event_the_runtime_does_not_fire_is_refused_typed() {
        for event in FIRED_HOOK_EVENTS {
            assert!(
                refuse_hooks_on_unfired_events(&compiled(event)).is_ok(),
                "{event}"
            );
        }
        let error = refuse_hooks_on_unfired_events(&compiled("from_a_newer_schema"))
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("hook_event_unknown_to_runtime"),
            "{error}"
        );
        assert!(error.contains("from_a_newer_schema"), "{error}");
    }

    /// The runtime's list and the signed kind-schema's events are the same set,
    /// so neither can gain an event the other lacks.
    #[test]
    fn the_runtimes_fired_events_equal_the_kind_schemas_events() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../../bundles/directive/.ai/node/engine/kinds/directive/directive.kind-schema.yaml",
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let body: String = text
            .lines()
            .filter(|line| !line.starts_with("# ryeos:signed"))
            .collect::<Vec<_>>()
            .join("\n");
        let schema: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
        let mut declared: Vec<String> = schema["execution"]["hooks"]["events"]
            .as_mapping()
            .unwrap()
            .keys()
            .map(|key| key.as_str().unwrap().to_string())
            .collect();
        declared.sort();
        let mut fired: Vec<String> = FIRED_HOOK_EVENTS.iter().map(|e| e.to_string()).collect();
        fired.sort();
        assert_eq!(declared, fired);
    }
}
